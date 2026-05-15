//! Power saving: pause the pipeline while the user is away.
//!
//! Two signal sources, combined into one state machine:
//! - the session screensaver service via D-Bus (`org.freedesktop.ScreenSaver`'s
//!   `ActiveChanged` signal),
//! - optional Wayland idle notifications via `ext-idle-notify-v1`.
//!
//! The pipeline reacts to [`PipelineCommand::Blackout`] / [`PipelineCommand::Resume`]
//! by publishing one black frame and idling, then resuming normal publishing. Capture and
//! MQTT remain running throughout.

use std::sync::Arc;

use anyhow::Result;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::config::schema::Config;
use crate::pipeline::PipelineCommand;

/// Internal "what the world looks like right now"; module-private because the only
/// consumer outside the state machine is `run`, which translates straight to a
/// pipeline command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PowerEvent {
    Idle,
    Active,
}

/// Merges the two independent idle signals into a single edge-triggered stream.
///
/// `observe_*` returns `Some(event)` only when the combined state flips relative
/// to what was last emitted. Either source going idle is enough to emit
/// [`PowerEvent::Idle`]; both sources must report active before we emit
/// [`PowerEvent::Active`].
#[derive(Debug)]
struct PowerState {
    screensaver_active: bool,
    wayland_idle: bool,
    last_emitted: PowerEvent,
}

impl PowerState {
    fn new() -> Self {
        Self {
            screensaver_active: false,
            wayland_idle: false,
            last_emitted: PowerEvent::Active,
        }
    }

    fn current(&self) -> PowerEvent {
        if self.screensaver_active || self.wayland_idle {
            PowerEvent::Idle
        } else {
            PowerEvent::Active
        }
    }

    fn observe_screensaver(&mut self, active: bool) -> Option<PowerEvent> {
        self.screensaver_active = active;
        self.maybe_emit()
    }

    fn observe_wayland(&mut self, idle: bool) -> Option<PowerEvent> {
        self.wayland_idle = idle;
        self.maybe_emit()
    }

    fn maybe_emit(&mut self) -> Option<PowerEvent> {
        let now = self.current();
        if now == self.last_emitted {
            return None;
        }
        self.last_emitted = now;
        Some(now)
    }
}

/// Spawn target for `app.rs`. Subscribes to both sources (per config) and forwards every
/// edge to `pipeline_ctrl`.
///
/// If both `respect_screensaver` is false and `idle_pause_after_secs` is `None`, this
/// task logs once and idles until cancellation; there is nothing to observe.
pub async fn run(
    config: Arc<Config>,
    pipeline_ctrl: mpsc::Sender<PipelineCommand>,
    cancel: CancellationToken,
) -> Result<()> {
    let span = tracing::info_span!("power");
    let _enter = span.enter();

    let respect_screensaver = config.power.respect_screensaver;
    let idle_secs = config.power.idle_pause_after_secs;

    if !respect_screensaver && idle_secs.is_none() {
        tracing::info!("power management disabled (screensaver off, no wayland idle)");
        cancel.cancelled().await;
        return Ok(());
    }

    #[cfg(feature = "wayland")]
    {
        impls::run_inner(respect_screensaver, idle_secs, pipeline_ctrl, cancel).await
    }
    #[cfg(not(feature = "wayland"))]
    {
        let _ = pipeline_ctrl;
        tracing::warn!(
            "power module compiled without the `wayland` feature; idle detection unavailable"
        );
        cancel.cancelled().await;
        Ok(())
    }
}

#[cfg(feature = "wayland")]
mod impls {
    use super::{PipelineCommand, PowerEvent, PowerState};

    use std::future::pending;

    use anyhow::Result;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    async fn forward(
        event: PowerEvent,
        pipeline_ctrl: &mpsc::Sender<PipelineCommand>,
    ) -> Result<()> {
        let cmd = match event {
            PowerEvent::Idle => PipelineCommand::Blackout,
            PowerEvent::Active => PipelineCommand::Resume,
        };
        pipeline_ctrl
            .send(cmd)
            .await
            .map_err(|_| anyhow::anyhow!("pipeline control channel closed"))
    }

    /// Boolean transitions drained from the Wayland event-loop thread.
    pub(super) enum WaylandSignal {
        Idle,
        Active,
    }

    pub(super) async fn run_inner(
        respect_screensaver: bool,
        idle_secs: Option<u64>,
        pipeline_ctrl: mpsc::Sender<PipelineCommand>,
        cancel: CancellationToken,
    ) -> Result<()> {
        let mut state = PowerState::new();

        let (ss_tx, ss_rx) = mpsc::channel::<bool>(8);
        let (wl_tx, wl_rx) = mpsc::channel::<WaylandSignal>(8);

        // Hold each receiver in an Option so a disabled or terminated source can be
        // routed to `pending()`; otherwise its closed `recv()` would fire Ready(None)
        // on every select! poll and the loop would spin (and starve other tasks under
        // the current_thread runtime).
        let mut ss_rx = if respect_screensaver {
            let cancel = cancel.clone();
            tokio::spawn(async move {
                if let Err(err) = screensaver::subscribe(ss_tx, cancel).await {
                    tracing::warn!(error = ?err, "screensaver source ended");
                }
            });
            Some(ss_rx)
        } else {
            drop(ss_tx);
            drop(ss_rx);
            None
        };

        let mut wl_rx = if let Some(secs) = idle_secs {
            if let Err(err) = wayland_idle::spawn(secs, wl_tx, cancel.clone()) {
                tracing::warn!(error = ?err, "wayland idle-notify unavailable");
                // `wayland_idle::spawn` took `wl_tx` by value and dropped it on the error
                // path, so `wl_rx` is now closed. Don't poll it.
                drop(wl_rx);
                None
            } else {
                Some(wl_rx)
            }
        } else {
            drop(wl_tx);
            drop(wl_rx);
            None
        };

        loop {
            let ss_next = async {
                match ss_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => pending::<Option<bool>>().await,
                }
            };
            let wl_next = async {
                match wl_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => pending::<Option<WaylandSignal>>().await,
                }
            };

            tokio::select! {
                _ = cancel.cancelled() => break,
                msg = ss_next => {
                    let Some(active) = msg else { ss_rx = None; continue };
                    if let Some(ev) = state.observe_screensaver(active) {
                        tracing::info!(?ev, source = "screensaver", "power state changed");
                        forward(ev, &pipeline_ctrl).await?;
                    }
                }
                msg = wl_next => {
                    let Some(sig) = msg else { wl_rx = None; continue };
                    let idle = matches!(sig, WaylandSignal::Idle);
                    if let Some(ev) = state.observe_wayland(idle) {
                        tracing::info!(?ev, source = "wayland", "power state changed");
                        forward(ev, &pipeline_ctrl).await?;
                    }
                }
            }
        }
        Ok(())
    }

    mod screensaver {
        use anyhow::{Context, Result};
        use futures_util::StreamExt;
        use tokio::sync::mpsc;
        use tokio_util::sync::CancellationToken;

        pub(super) async fn subscribe(
            tx: mpsc::Sender<bool>,
            cancel: CancellationToken,
        ) -> Result<()> {
            let conn = zbus::Connection::session()
                .await
                .context("connecting to session bus")?;
            let proxy = zbus::Proxy::new(
                &conn,
                "org.freedesktop.ScreenSaver",
                "/org/freedesktop/ScreenSaver",
                "org.freedesktop.ScreenSaver",
            )
            .await
            .context("constructing ScreenSaver proxy")?;

            let mut stream = proxy
                .receive_signal("ActiveChanged")
                .await
                .context("subscribing to ActiveChanged")?;

            tracing::info!("screensaver source subscribed");

            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    msg = stream.next() => {
                        let Some(msg) = msg else { break };
                        let active: bool = match msg.body().deserialize() {
                            Ok(v) => v,
                            Err(err) => {
                                tracing::warn!(error = ?err, "malformed ActiveChanged payload");
                                continue;
                            }
                        };
                        if tx.send(active).await.is_err() {
                            break;
                        }
                    }
                }
            }
            Ok(())
        }
    }

    mod wayland_idle {
        use super::WaylandSignal;
        use anyhow::{anyhow, Context, Result};
        use tokio::sync::mpsc;
        use tokio_util::sync::CancellationToken;

        use wayland_client::globals::{registry_queue_init, GlobalListContents};
        use wayland_client::protocol::{wl_registry, wl_seat};
        use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};
        use wayland_protocols::ext::idle_notify::v1::client::{
            ext_idle_notification_v1, ext_idle_notifier_v1,
        };

        struct AppData {
            tx: mpsc::Sender<WaylandSignal>,
        }

        impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for AppData {
            fn event(
                _state: &mut Self,
                _proxy: &wl_registry::WlRegistry,
                _event: wl_registry::Event,
                _data: &GlobalListContents,
                _conn: &Connection,
                _qh: &QueueHandle<Self>,
            ) {
            }
        }

        impl Dispatch<wl_seat::WlSeat, ()> for AppData {
            fn event(
                _state: &mut Self,
                _proxy: &wl_seat::WlSeat,
                _event: wl_seat::Event,
                _data: &(),
                _conn: &Connection,
                _qh: &QueueHandle<Self>,
            ) {
            }
        }

        impl Dispatch<ext_idle_notifier_v1::ExtIdleNotifierV1, ()> for AppData {
            fn event(
                _state: &mut Self,
                _proxy: &ext_idle_notifier_v1::ExtIdleNotifierV1,
                _event: ext_idle_notifier_v1::Event,
                _data: &(),
                _conn: &Connection,
                _qh: &QueueHandle<Self>,
            ) {
            }
        }

        impl Dispatch<ext_idle_notification_v1::ExtIdleNotificationV1, ()> for AppData {
            fn event(
                state: &mut Self,
                _proxy: &ext_idle_notification_v1::ExtIdleNotificationV1,
                event: ext_idle_notification_v1::Event,
                _data: &(),
                _conn: &Connection,
                _qh: &QueueHandle<Self>,
            ) {
                let signal = match event {
                    ext_idle_notification_v1::Event::Idled => WaylandSignal::Idle,
                    ext_idle_notification_v1::Event::Resumed => WaylandSignal::Active,
                    _ => return,
                };
                // try_send drops if full or closed, a missed transient edge is recovered
                // by the next one; the channel is sized so this is unlikely in practice.
                let _ = state.tx.try_send(signal);
            }
        }

        pub(super) fn spawn(
            idle_secs: u64,
            tx: mpsc::Sender<WaylandSignal>,
            cancel: CancellationToken,
        ) -> Result<()> {
            // Probe the connection on the calling task so we can fail fast (and gracefully)
            // before spawning a thread.
            let conn = Connection::connect_to_env().context("connecting to wayland display")?;
            let (globals, event_queue) =
                registry_queue_init::<AppData>(&conn).context("initial wayland roundtrip")?;

            let qh = event_queue.handle();
            let notifier = globals
                .bind::<ext_idle_notifier_v1::ExtIdleNotifierV1, _, _>(&qh, 1..=1, ())
                .map_err(|e| anyhow!("ext_idle_notifier_v1 not advertised: {e}"))?;
            let seat = globals
                .bind::<wl_seat::WlSeat, _, _>(&qh, 1..=9, ())
                .map_err(|e| anyhow!("wl_seat unavailable: {e}"))?;

            let timeout_ms = u32::try_from(idle_secs.saturating_mul(1000)).unwrap_or(u32::MAX);
            let notification = notifier.get_idle_notification(timeout_ms, &seat, &qh, ());

            std::thread::Builder::new()
                .name("wayland-idle".into())
                .spawn(move || {
                    // Keep the proxies alive for the thread's lifetime; dropping any of
                    // them issues a Destroy request on the next flush and the compositor
                    // tears the subscription down before any event fires.
                    let _notifier = notifier;
                    let _seat = seat;
                    let _notification = notification;
                    run_thread(conn, event_queue, tx, cancel);
                })
                .context("spawning wayland-idle thread")?;
            Ok(())
        }

        fn run_thread(
            _conn: Connection,
            mut event_queue: EventQueue<AppData>,
            tx: mpsc::Sender<WaylandSignal>,
            cancel: CancellationToken,
        ) {
            let mut state = AppData { tx };
            while !cancel.is_cancelled() {
                if let Err(err) = event_queue.blocking_dispatch(&mut state) {
                    tracing::warn!(error = ?err, "wayland idle dispatch ended");
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_state_is_active() {
        let s = PowerState::new();
        assert_eq!(s.current(), PowerEvent::Active);
    }

    #[test]
    fn screensaver_true_emits_idle() {
        let mut s = PowerState::new();
        assert_eq!(s.observe_screensaver(true), Some(PowerEvent::Idle));
    }

    #[test]
    fn screensaver_back_to_false_emits_active() {
        let mut s = PowerState::new();
        s.observe_screensaver(true);
        assert_eq!(s.observe_screensaver(false), Some(PowerEvent::Active));
    }

    #[test]
    fn wayland_idle_emits_idle_when_screensaver_inactive() {
        let mut s = PowerState::new();
        assert_eq!(s.observe_wayland(true), Some(PowerEvent::Idle));
    }

    #[test]
    fn both_idle_does_not_double_emit() {
        let mut s = PowerState::new();
        assert_eq!(s.observe_screensaver(true), Some(PowerEvent::Idle));
        assert_eq!(s.observe_wayland(true), None);
    }

    #[test]
    fn one_source_active_while_other_idle_keeps_idle() {
        let mut s = PowerState::new();
        s.observe_screensaver(true);
        s.observe_wayland(true);
        // Screensaver releases but wayland still idle → still Idle, no emit.
        assert_eq!(s.observe_screensaver(false), None);

        let mut s = PowerState::new();
        s.observe_screensaver(true);
        s.observe_wayland(true);
        // Wayland releases but screensaver still active → still Idle, no emit.
        assert_eq!(s.observe_wayland(false), None);
    }

    #[test]
    fn both_sources_must_release_to_emit_active() {
        let mut s = PowerState::new();
        s.observe_screensaver(true);
        s.observe_wayland(true);
        assert_eq!(s.observe_screensaver(false), None);
        assert_eq!(s.observe_wayland(false), Some(PowerEvent::Active));
    }

    #[test]
    fn repeated_same_value_does_not_emit() {
        let mut s = PowerState::new();
        assert_eq!(s.observe_screensaver(false), None);
        s.observe_screensaver(true);
        assert_eq!(s.observe_screensaver(true), None);
    }

    // Regression guard for the closed-channel busy-loop: when a source is disabled,
    // its arm must route to `pending()` so the loop only wakes on cancellation. Before
    // the fix, the closed receiver fired Ready(None) on every poll, the current_thread
    // runtime starved the test task that would call `cancel.cancel()`, and (with
    // paused time) the runtime never went idle so this test hung.
    #[cfg(feature = "wayland")]
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn run_inner_does_not_busy_loop_when_one_source_disabled() {
        use crate::pipeline::PipelineCommand;
        use std::time::Duration;
        use tokio::sync::mpsc;
        use tokio_util::sync::CancellationToken;

        let cancel = CancellationToken::new();
        let (tx, mut rx) = mpsc::channel::<PipelineCommand>(8);
        let cancel_for_task = cancel.clone();
        let handle =
            tokio::spawn(async move { impls::run_inner(true, None, tx, cancel_for_task).await });

        // Let the task settle. With start_paused, tokio time is mocked.
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.cancel();

        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("run_inner exits after cancel")
            .expect("join ok")
            .expect("run_inner returns Ok");
        assert!(rx.try_recv().is_err(), "no spurious pipeline commands");
    }
}
