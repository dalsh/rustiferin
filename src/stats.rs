//! Stats aggregator shared between producers (capture, pipeline, output) and
//! consumers (tray, logs).
//!
//! [`Metrics`] is an `Arc`-backed handle of `AtomicU64` counters that producer
//! tasks increment cheaply on each event. A 1-Hz aggregator task in `app.rs`
//! reads the counters, computes deltas, fills [`Stats`], and publishes it on a
//! `tokio::sync::watch::Sender<Stats>`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::output::OutputState;

/// Snapshot of throughput and connection state. Published once per second.
#[derive(Debug, Clone, Copy)]
pub struct Stats {
    pub capture_fps: f32,
    pub output_fps: f32,
    pub output_state: OutputState,
    pub frames_dropped: u64,
}

impl Default for Stats {
    fn default() -> Self {
        Self {
            capture_fps: 0.0,
            output_fps: 0.0,
            output_state: OutputState::Connecting,
            frames_dropped: 0,
        }
    }
}

/// Cloneable handle: every clone shares the same atomic counters.
///
/// `frames_dropped` is intentionally **not** a counter: the aggregator
/// computes it as `captured_delta - processed_delta` each tick so it stays
/// a per-second gauge.
/// `mqtt_publishes` is redundant with `frames_published` in v1 (one publish =
/// one frame, no chunking), so it's omitted too; re-add when chunking returns.
#[derive(Clone, Default)]
pub struct Metrics {
    frames_captured: Arc<AtomicU64>,
    frames_processed: Arc<AtomicU64>,
    frames_published: Arc<AtomicU64>,
    mqtt_reconnects: Arc<AtomicU64>,
}

impl Metrics {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn frames_captured(&self) -> &AtomicU64 {
        &self.frames_captured
    }
    pub fn frames_processed(&self) -> &AtomicU64 {
        &self.frames_processed
    }
    pub fn frames_published(&self) -> &AtomicU64 {
        &self.frames_published
    }
    pub fn mqtt_reconnects(&self) -> &AtomicU64 {
        &self.mqtt_reconnects
    }
}

/// 1-Hz loop that snapshots [`Metrics`] counters, computes deltas, and publishes
/// a fresh [`Stats`] on the watch channel. Cancelled by the shared shutdown
/// token.
pub async fn aggregator(
    metrics: Metrics,
    output_state_rx: watch::Receiver<OutputState>,
    stats_out: watch::Sender<Stats>,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Consume the immediately-firing first tick so the first publish reflects a
    // real one-second window rather than zero.
    ticker.tick().await;

    let mut last_captured = 0u64;
    let mut last_processed = 0u64;
    let mut last_published = 0u64;

    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Ok(()),
            _ = ticker.tick() => {
                let captured = metrics.frames_captured().load(Ordering::Relaxed);
                let processed = metrics.frames_processed().load(Ordering::Relaxed);
                let published = metrics.frames_published().load(Ordering::Relaxed);
                let captured_delta = captured.saturating_sub(last_captured);
                let processed_delta = processed.saturating_sub(last_processed);
                let stats = Stats {
                    capture_fps: captured_delta as f32,
                    output_fps: published.saturating_sub(last_published) as f32,
                    output_state: *output_state_rx.borrow(),
                    frames_dropped: captured_delta.saturating_sub(processed_delta),
                };
                tracing::debug!(
                    capture_fps = stats.capture_fps,
                    output_fps = stats.output_fps,
                    output_state = ?stats.output_state,
                    frames_dropped = stats.frames_dropped,
                    "stats tick"
                );
                let _ = stats_out.send(stats);
                last_captured = captured;
                last_processed = processed;
                last_published = published;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[test]
    fn clones_share_underlying_counters() {
        let a = Metrics::new();
        let b = a.clone();
        a.frames_captured().fetch_add(3, Ordering::Relaxed);
        b.frames_captured().fetch_add(2, Ordering::Relaxed);
        assert_eq!(a.frames_captured().load(Ordering::Relaxed), 5);
        assert_eq!(b.frames_captured().load(Ordering::Relaxed), 5);
    }

    #[tokio::test(start_paused = true)]
    async fn aggregator_publishes_per_second_deltas() {
        let metrics = Metrics::new();
        let (_state_tx, state_rx) = watch::channel(OutputState::Connected);
        let (stats_tx, mut stats_rx) = watch::channel(Stats::default());
        let cancel = CancellationToken::new();

        let task = tokio::spawn({
            let metrics = metrics.clone();
            let cancel = cancel.clone();
            async move { aggregator(metrics, state_rx, stats_tx, cancel).await }
        });

        metrics.frames_captured().fetch_add(30, Ordering::Relaxed);
        metrics.frames_processed().fetch_add(28, Ordering::Relaxed);
        metrics.frames_published().fetch_add(28, Ordering::Relaxed);

        // The aggregator consumes its immediately-firing first tick, then sleeps
        // 1s before the first publish, advance past that boundary.
        tokio::time::advance(Duration::from_secs(1)).await;
        stats_rx.changed().await.expect("first stats tick");
        let s = *stats_rx.borrow();
        assert_eq!(s.capture_fps, 30.0);
        assert_eq!(s.output_fps, 28.0);
        assert_eq!(s.frames_dropped, 2);
        assert_eq!(s.output_state, OutputState::Connected);

        cancel.cancel();
        let _ = task.await;
    }

    #[tokio::test(start_paused = true)]
    async fn aggregator_resets_deltas_between_ticks() {
        let metrics = Metrics::new();
        let (_state_tx, state_rx) = watch::channel(OutputState::Connected);
        let (stats_tx, mut stats_rx) = watch::channel(Stats::default());
        let cancel = CancellationToken::new();

        let task = tokio::spawn({
            let metrics = metrics.clone();
            let cancel = cancel.clone();
            async move { aggregator(metrics, state_rx, stats_tx, cancel).await }
        });

        // First window: 30 captured, 30 published.
        metrics.frames_captured().fetch_add(30, Ordering::Relaxed);
        metrics.frames_processed().fetch_add(30, Ordering::Relaxed);
        metrics.frames_published().fetch_add(30, Ordering::Relaxed);
        tokio::time::advance(Duration::from_secs(1)).await;
        stats_rx.changed().await.expect("first stats tick");
        assert_eq!(stats_rx.borrow().capture_fps, 30.0);

        // Second window: 25 more captured/published; the delta must be 25, not 55.
        metrics.frames_captured().fetch_add(25, Ordering::Relaxed);
        metrics.frames_processed().fetch_add(25, Ordering::Relaxed);
        metrics.frames_published().fetch_add(25, Ordering::Relaxed);
        tokio::time::advance(Duration::from_secs(1)).await;
        stats_rx.changed().await.expect("second stats tick");
        let s = *stats_rx.borrow();
        assert_eq!(s.capture_fps, 25.0);
        assert_eq!(s.output_fps, 25.0);
        assert_eq!(s.frames_dropped, 0);

        cancel.cancel();
        let _ = task.await;
    }

    #[test]
    fn default_stats_starts_at_zero() {
        let s = Stats::default();
        assert_eq!(s.capture_fps, 0.0);
        assert_eq!(s.output_fps, 0.0);
        assert_eq!(s.frames_dropped, 0);
        assert_eq!(s.output_state, OutputState::Connecting);
    }
}
