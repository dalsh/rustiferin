//! Integration tests for `rustiferin::tray::run`.
//!
//! The interesting failure-contract case is "factory cannot bring up the
//! tray service": that is fatal by design. We pin that down with an
//! injected `TrayServiceFactory` that returns `Err`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rustiferin::stats::Stats;
use rustiferin::tray::{self, RustiferinTray, TrayHandle, TrayServiceFactory, TrayServiceGuard};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

struct FailingFactory;

#[async_trait]
impl TrayServiceFactory for FailingFactory {
    async fn spawn(&self, _tray: RustiferinTray) -> anyhow::Result<TrayServiceGuard> {
        anyhow::bail!("simulated: no StatusNotifier host")
    }
}

#[tokio::test]
async fn run_returns_err_when_factory_fails() {
    let (_stats_tx, stats_rx) = watch::channel(Stats::default());
    let (cmd_tx, _cmd_rx) = mpsc::channel(8);
    let cancel = CancellationToken::new();

    let result = timeout(
        Duration::from_secs(2),
        tray::run(Box::new(FailingFactory), stats_rx, cmd_tx, cancel),
    )
    .await
    .expect("tray::run must complete within timeout");

    assert!(result.is_err(), "expected Err, got {result:?}");
}

/// `TrayHandle` stub that keeps its completion sender alive until dropped,
/// so the `completion` arm of `tray::run`'s select never fires
/// spontaneously.
struct CountingHandle {
    updates: Arc<AtomicUsize>,
    shutdowns: Arc<AtomicUsize>,
    _keep_alive: oneshot::Sender<anyhow::Result<()>>,
}

impl TrayHandle for CountingHandle {
    fn update(&self) {
        self.updates.fetch_add(1, Ordering::Relaxed);
    }
    fn shutdown(&self) {
        self.shutdowns.fetch_add(1, Ordering::Relaxed);
    }
}

struct OkFactory {
    updates: Arc<AtomicUsize>,
    shutdowns: Arc<AtomicUsize>,
}

#[async_trait]
impl TrayServiceFactory for OkFactory {
    async fn spawn(&self, _tray: RustiferinTray) -> anyhow::Result<TrayServiceGuard> {
        let (tx, rx) = oneshot::channel();
        Ok(TrayServiceGuard {
            handle: Box::new(CountingHandle {
                updates: self.updates.clone(),
                shutdowns: self.shutdowns.clone(),
                _keep_alive: tx,
            }),
            completion: rx,
        })
    }
}

#[tokio::test]
async fn run_returns_ok_and_shuts_down_handle_on_cancel() {
    let (stats_tx, stats_rx) = watch::channel(Stats::default());
    let (cmd_tx, _cmd_rx) = mpsc::channel(8);
    let cancel = CancellationToken::new();
    let updates = Arc::new(AtomicUsize::new(0));
    let shutdowns = Arc::new(AtomicUsize::new(0));

    let factory = Box::new(OkFactory {
        updates: updates.clone(),
        shutdowns: shutdowns.clone(),
    });

    let inner_cancel = cancel.clone();
    let join = tokio::spawn(tray::run(factory, stats_rx, cmd_tx, inner_cancel));

    // Push a stats update; the tray must call `handle.update()` in response.
    stats_tx
        .send(Stats {
            capture_fps: 30.0,
            ..Stats::default()
        })
        .expect("send stats");

    // Wait for `handle.update()` to be observed before cancelling so the test
    // does not race the select arms.
    for _ in 0..100 {
        if updates.load(Ordering::Relaxed) >= 1 {
            break;
        }
        tokio::task::yield_now().await;
    }

    // Cancellation drives the loop to exit.
    cancel.cancel();
    let result = timeout(Duration::from_secs(2), join)
        .await
        .expect("tray::run joins")
        .expect("tray::run task did not panic");

    assert!(result.is_ok(), "expected Ok, got {result:?}");
    assert_eq!(
        shutdowns.load(Ordering::Relaxed),
        1,
        "handle.shutdown() must be called exactly once"
    );
    assert!(
        updates.load(Ordering::Relaxed) >= 1,
        "stats change must trigger handle.update()"
    );
    // Keep `stats_tx` alive until after the assertions so the channel doesn't
    // close before `run` consumes the change.
    drop(stats_tx);
}

#[tokio::test]
async fn run_surfaces_completion_error_as_err() {
    struct ErrCompletionFactory;

    #[async_trait]
    impl TrayServiceFactory for ErrCompletionFactory {
        async fn spawn(&self, _tray: RustiferinTray) -> anyhow::Result<TrayServiceGuard> {
            let (tx, rx) = oneshot::channel();
            // Service "succeeds" at init but immediately reports an error.
            let _ = tx.send(Err(anyhow::anyhow!("dbus session lost")));
            Ok(TrayServiceGuard {
                handle: Box::new(NoopHandle),
                completion: rx,
            })
        }
    }

    struct NoopHandle;
    impl TrayHandle for NoopHandle {
        fn update(&self) {}
        fn shutdown(&self) {}
    }

    let (_stats_tx, stats_rx) = watch::channel(Stats::default());
    let (cmd_tx, _cmd_rx) = mpsc::channel(8);
    let cancel = CancellationToken::new();

    let result = timeout(
        Duration::from_secs(2),
        tray::run(Box::new(ErrCompletionFactory), stats_rx, cmd_tx, cancel),
    )
    .await
    .expect("tray::run completes within timeout");

    let err = result.expect_err("expected Err");
    assert!(
        format!("{err:#}").contains("dbus session lost"),
        "error must propagate source: {err:#}"
    );
}
