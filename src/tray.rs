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
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

use crate::stats::Stats;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayCommand {
    ToggleRun,
    Quit,
    OpenConfig,
}

/// Owned by the tray service thread. Shares stats + the running flag with
/// the async `run` task via `Arc` clones returned through accessor methods.
pub struct RustiferinTray {
    stats: Arc<ArcSwap<Stats>>,
    commands: mpsc::Sender<TrayCommand>,
    running: Arc<AtomicBool>,
    watcher_missing: Arc<AtomicBool>,
    init_done: Arc<AtomicBool>,
}

impl RustiferinTray {
    pub fn new(initial_stats: Stats, commands: mpsc::Sender<TrayCommand>) -> Self {
        Self {
            stats: Arc::new(ArcSwap::from_pointee(initial_stats)),
            commands,
            running: Arc::new(AtomicBool::new(true)),
            watcher_missing: Arc::new(AtomicBool::new(false)),
            init_done: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn stats_handle(&self) -> Arc<ArcSwap<Stats>> {
        self.stats.clone()
    }

    pub fn watcher_missing_handle(&self) -> Arc<AtomicBool> {
        self.watcher_missing.clone()
    }

    pub fn init_done_handle(&self) -> Arc<AtomicBool> {
        self.init_done.clone()
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

pub trait TrayServiceFactory: Send {
    fn spawn(&self, tray: RustiferinTray) -> anyhow::Result<TrayServiceGuard>;
}

/// Drive the tray for the lifetime of the process. On `cancel` or stream
/// shutdown returns `Ok(())`. On factory failure or unexpected service
/// termination returns `Err`.
pub async fn run(
    factory: Box<dyn TrayServiceFactory>,
    mut stats_in: watch::Receiver<Stats>,
    commands_out: mpsc::Sender<TrayCommand>,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    let initial = *stats_in.borrow();
    let tray = RustiferinTray::new(initial, commands_out);
    let stats_shared = tray.stats_handle();

    let TrayServiceGuard {
        handle,
        mut completion,
    } = factory
        .spawn(tray)
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
    use std::sync::mpsc as std_mpsc;
    use std::time::Duration;

    use anyhow::Context;
    use ksni::menu::StandardItem;

    use super::{RustiferinTray, TrayCommand, TrayHandle, TrayServiceFactory, TrayServiceGuard};

    /// Window we give ksni to surface an init failure (D-Bus error, missing
    /// StatusNotifier host) before considering the service started.
    const INIT_GRACE: Duration = Duration::from_millis(500);

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

    impl TrayServiceFactory for ProductionTrayServiceFactory {
        fn spawn(&self, tray: RustiferinTray) -> anyhow::Result<TrayServiceGuard> {
            let watcher_missing = tray.watcher_missing_handle();
            let init_done = tray.init_done_handle();

            let service = ksni::TrayService::new(tray);
            let ksni_handle = service.handle();

            // ksni's `run()` does both init and the dbus event loop in one call.
            // We can't split them, so we run it on a dedicated thread and wait
            // briefly to catch init failure. After the grace window any error
            // we missed will surface through `completion`.
            let (result_tx, result_rx) = std_mpsc::sync_channel::<anyhow::Result<()>>(1);
            std::thread::Builder::new()
                .name("tray-ksni".into())
                .spawn(move || {
                    let r = service
                        .run()
                        .map_err(anyhow::Error::new)
                        .context("ksni tray service");
                    let _ = result_tx.send(r);
                })
                .context("spawning ksni service thread")?;

            match result_rx.recv_timeout(INIT_GRACE) {
                Ok(Err(e)) => return Err(e),
                Ok(Ok(())) => {
                    if watcher_missing.load(Ordering::Acquire) {
                        anyhow::bail!("no StatusNotifier watcher available on the session bus");
                    }
                    // ksni exited cleanly during the grace window without flagging
                    // a missing watcher. Treat as immediate "service closed".
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    let _ = tx.send(Ok(()));
                    return Ok(TrayServiceGuard {
                        handle: Box::new(KsniHandle {
                            handle: ksni_handle,
                        }),
                        completion: rx,
                    });
                }
                Err(std_mpsc::RecvTimeoutError::Timeout) => {}
                Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                    anyhow::bail!("ksni service thread terminated without a result");
                }
            }

            // Init succeeded. From this point we treat the watcher as load-bearing
            // for "tray appeared at startup", so any later disappearance is allowed
            // (ksni's default re-register-on-return behavior takes over).
            init_done.store(true, Ordering::Release);

            if watcher_missing.load(Ordering::Acquire) {
                // Race: the watcher was missing during the grace window but the
                // service didn't terminate yet. Treat as init failure.
                ksni_handle.shutdown();
                anyhow::bail!("no StatusNotifier watcher available on the session bus");
            }

            // Forward any future termination result onto the async oneshot.
            let (tx, rx) = tokio::sync::oneshot::channel();
            std::thread::Builder::new()
                .name("tray-completion".into())
                .spawn(move || {
                    let res = result_rx.recv().unwrap_or(Ok(()));
                    let _ = tx.send(res);
                })
                .context("spawning ksni completion forwarder")?;

            Ok(TrayServiceGuard {
                handle: Box::new(KsniHandle {
                    handle: ksni_handle,
                }),
                completion: rx,
            })
        }
    }

    struct KsniHandle {
        handle: ksni::Handle<RustiferinTray>,
    }

    impl TrayHandle for KsniHandle {
        fn update(&self) {
            self.handle.update(|_| {});
        }
        fn shutdown(&self) {
            self.handle.shutdown();
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

        fn watcher_offine(&self) -> bool {
            // While bringing the tray up the absence of a StatusNotifier host
            // is fatal; we want ksni to stop so the async side sees init
            // failure. Once init succeeded a transient host disappearance
            // (Plasma restart) is fine; the default `true` keeps ksni
            // waiting to re-register.
            if self.init_done.load(Ordering::Acquire) {
                true
            } else {
                self.watcher_missing.store(true, Ordering::Release);
                false
            }
        }
    }
}
