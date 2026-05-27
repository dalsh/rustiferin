//! System-tray icon (StatusNotifier). Reads `watch::Receiver<Stats>` for the
//! tooltip and forwards menu activations as `TrayCommand`s.
//!
//! Failure contract: an unrunnable tray is fatal; `run` returns `Err` and
//! the global `CancellationToken` tears the process down. Users who want
//! to skip the tray pass `--headless`.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use anyhow::Context;
use arc_swap::ArcSwap;
use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

use crate::runtime::BrightnessGain;
use crate::stats::Stats;

/// Preset values offered by the tray's "Brightness" radio submenu. Index
/// alignment with `BRIGHTNESS_GAIN_LABELS` is load-bearing; keep in sync.
pub const BRIGHTNESS_GAIN_PRESETS: &[f32] = &[0.8, 1.0, 1.2, 1.5, 2.0, 3.0];
pub const BRIGHTNESS_GAIN_LABELS: &[&str] =
    &["0.8x", "1.0x (default)", "1.2x", "1.5x", "2.0x", "3.0x"];

/// Index of the preset closest to `value` (Euclidean on the scalar). Used to
/// drive the radio group's `selected` field.
pub fn closest_preset_index(value: f32) -> usize {
    BRIGHTNESS_GAIN_PRESETS
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| {
            (*a - value)
                .abs()
                .partial_cmp(&(*b - value).abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(i, _)| i)
        .unwrap_or(1)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TrayCommand {
    ToggleRun,
    Quit,
    OpenConfig,
    SetBrightnessGain(f32),
}

/// Owned by the tray service. Shares stats + the running flag with the async
/// `run` task via `Arc` clones returned through accessor methods.
pub struct RustiferinTray {
    stats: Arc<ArcSwap<Stats>>,
    commands: mpsc::Sender<TrayCommand>,
    running: Arc<AtomicBool>,
    brightness_gain: BrightnessGain,
}

impl RustiferinTray {
    pub fn new(
        initial_stats: Stats,
        commands: mpsc::Sender<TrayCommand>,
        brightness_gain: BrightnessGain,
    ) -> Self {
        Self {
            stats: Arc::new(ArcSwap::from_pointee(initial_stats)),
            commands,
            running: Arc::new(AtomicBool::new(true)),
            brightness_gain,
        }
    }

    pub fn stats_handle(&self) -> Arc<ArcSwap<Stats>> {
        self.stats.clone()
    }
}

pub trait TrayHandle: Send {
    fn update(&self);
    fn shutdown(&self);
}

pub struct TrayServiceGuard {
    pub handle: Box<dyn TrayHandle>,
    pub completion: oneshot::Receiver<anyhow::Result<()>>,
}

#[async_trait]
pub trait TrayServiceFactory: Send {
    async fn spawn(&self, tray: RustiferinTray) -> anyhow::Result<TrayServiceGuard>;
}

/// Drive the tray for the lifetime of the process. On `cancel` or stream
/// shutdown returns `Ok(())`. On factory failure or unexpected service
/// termination returns `Err`.
pub async fn run(
    factory: Box<dyn TrayServiceFactory>,
    mut stats_in: watch::Receiver<Stats>,
    commands_out: mpsc::Sender<TrayCommand>,
    brightness_gain: BrightnessGain,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    let initial = *stats_in.borrow();
    let tray = RustiferinTray::new(initial, commands_out, brightness_gain);
    let stats_shared = tray.stats_handle();

    let TrayServiceGuard {
        handle,
        mut completion,
    } = factory
        .spawn(tray)
        .await
        .context("bringing up system tray service")?;

    let outcome: anyhow::Result<()> = loop {
        tokio::select! {
            _ = cancel.cancelled() => break Ok(()),
            res = stats_in.changed() => {
                if res.is_err() { break Ok(()); }
                stats_shared.store(Arc::new(*stats_in.borrow()));
                handle.update();
            }
            res = &mut completion => {
                match res {
                    Ok(Ok(())) => break Ok(()),
                    Ok(Err(e)) => break Err(e).context("system tray service exited"),
                    Err(_) => break Ok(()),
                }
            }
        }
    };

    handle.shutdown();
    outcome
}

#[cfg(feature = "tray")]
pub use production::ProductionTrayServiceFactory;

#[cfg(feature = "tray")]
mod production {
    use std::sync::atomic::Ordering;

    use anyhow::Context;
    use async_trait::async_trait;
    use ksni::menu::{RadioGroup, RadioItem, StandardItem, SubMenu};
    use ksni::TrayMethods;

    use super::{
        closest_preset_index, RustiferinTray, TrayCommand, TrayHandle, TrayServiceFactory,
        TrayServiceGuard, BRIGHTNESS_GAIN_LABELS, BRIGHTNESS_GAIN_PRESETS,
    };

    pub struct ProductionTrayServiceFactory;

    impl ProductionTrayServiceFactory {
        pub fn new() -> Self {
            Self
        }
    }

    impl Default for ProductionTrayServiceFactory {
        fn default() -> Self {
            Self::new()
        }
    }

    #[async_trait]
    impl TrayServiceFactory for ProductionTrayServiceFactory {
        async fn spawn(&self, tray: RustiferinTray) -> anyhow::Result<TrayServiceGuard> {
            // ksni 0.3 reports a missing StatusNotifier watcher synchronously
            // via `spawn().await` returning Err (assume_sni_available defaults
            // to false). No grace window or extra thread needed.
            let handle = tray
                .spawn()
                .await
                .map_err(anyhow::Error::new)
                .context("ksni tray service")?;

            // ksni 0.3 doesn't surface post-init service termination, the
            // service is kept alive by the Handle. Hand the consumer a
            // completion receiver that stays pending until shutdown: we keep
            // the sender alive inside `KsniHandle` so the oneshot only fires
            // (with `Err(RecvError)`) when the handle is dropped, by which
            // point `tray::run`'s select loop has already exited via another
            // arm. Without this, binding `_tx` would drop the sender at end
            // of `spawn`, closing the channel immediately and making the
            // completion arm of the select fire on the first poll, taking
            // the whole app down.
            let (completion_tx, rx) = tokio::sync::oneshot::channel();
            Ok(TrayServiceGuard {
                handle: Box::new(KsniHandle {
                    handle,
                    _completion_tx: completion_tx,
                }),
                completion: rx,
            })
        }
    }

    struct KsniHandle {
        handle: ksni::Handle<RustiferinTray>,
        // Held purely to keep the `completion` oneshot in `TrayServiceGuard`
        // pending for the lifetime of the handle. See the comment in `spawn`.
        _completion_tx: tokio::sync::oneshot::Sender<anyhow::Result<()>>,
    }

    impl TrayHandle for KsniHandle {
        fn update(&self) {
            // `Handle::update` is async in ksni 0.3; fire-and-forget so the
            // tooltip refresh doesn't block the caller's select loop.
            let h = self.handle.clone();
            tokio::spawn(async move {
                let _ = h.update(|_| {}).await;
            });
        }
        fn shutdown(&self) {
            // The returned ShutdownAwaiter is only useful if you want to wait
            // for the service loop to finish; we don't.
            drop(self.handle.shutdown());
        }
    }

    impl ksni::Tray for RustiferinTray {
        fn id(&self) -> String {
            "rustiferin".into()
        }
        fn title(&self) -> String {
            "Rustiferin".into()
        }
        fn icon_name(&self) -> String {
            // Freedesktop name; users can drop a custom theme later.
            "video-display".into()
        }
        fn tool_tip(&self) -> ksni::ToolTip {
            let s = self.stats.load();
            ksni::ToolTip {
                title: "Rustiferin".into(),
                description: format!(
                    "{:?}\nCapture: {:.1} fps\nOutput: {:.1} fps\nDropped: {}",
                    s.output_state, s.capture_fps, s.output_fps, s.frames_dropped
                ),
                icon_name: "video-display".into(),
                icon_pixmap: Vec::new(),
            }
        }
        fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
            let running = self.running.load(Ordering::Relaxed);
            let current_gain = self.brightness_gain.load();
            let brightness_options: Vec<RadioItem> = BRIGHTNESS_GAIN_LABELS
                .iter()
                .map(|label| RadioItem {
                    label: (*label).into(),
                    ..Default::default()
                })
                .collect();
            vec![
                StandardItem {
                    label: if running {
                        "Pause".into()
                    } else {
                        "Resume".into()
                    },
                    activate: Box::new(|this: &mut Self| {
                        this.running.fetch_xor(true, Ordering::AcqRel);
                        let _ = this.commands.try_send(TrayCommand::ToggleRun);
                    }),
                    ..Default::default()
                }
                .into(),
                ksni::MenuItem::Separator,
                SubMenu {
                    label: "Brightness".into(),
                    submenu: vec![RadioGroup {
                        selected: closest_preset_index(current_gain),
                        select: Box::new(|this: &mut Self, index: usize| {
                            if let Some(&value) = BRIGHTNESS_GAIN_PRESETS.get(index) {
                                let _ = this
                                    .commands
                                    .try_send(TrayCommand::SetBrightnessGain(value));
                            }
                        }),
                        options: brightness_options,
                    }
                    .into()],
                    ..Default::default()
                }
                .into(),
                ksni::MenuItem::Separator,
                StandardItem {
                    label: "Open config...".into(),
                    activate: Box::new(|this: &mut Self| {
                        let _ = this.commands.try_send(TrayCommand::OpenConfig);
                    }),
                    ..Default::default()
                }
                .into(),
                ksni::MenuItem::Separator,
                StandardItem {
                    label: "Quit".into(),
                    activate: Box::new(|this: &mut Self| {
                        let _ = this.commands.try_send(TrayCommand::Quit);
                    }),
                    ..Default::default()
                }
                .into(),
            ]
        }
    }
}
