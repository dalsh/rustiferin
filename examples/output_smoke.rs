//! Hardware smoke harness for the MQTT output task.
//!
//! Loads a real `Config`, spins up `output::run` against the configured broker,
//! and drives the pipeline -> output `watch` channel with a deterministic color
//! sequence so the LED strip can be observed visually. Stands in for the full
//! capture -> pipeline -> output path until `app.rs` is wired.
//!
//! Run: `cargo run --release --example output_smoke -- /tmp/rustiferin-smoke.yaml`

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use rustiferin::config;
use rustiferin::output::{self, OutputState};
use rustiferin::pipeline::{LedColor, LedFrame};

/// Match the firmware's configured strip length so the whole strip is exercised.
const STRIP_LEN: usize = 110;
/// Frame rate of the synthetic generator.
const FPS: u64 = 30;
/// How long each pattern is held before advancing.
const PATTERN_SECS: u64 = 3;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    init_logging();

    let path: PathBuf = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp/rustiferin-smoke.yaml"));
    let cfg =
        config::load(&path).with_context(|| format!("loading config from {}", path.display()))?;
    let config = Arc::new(cfg);

    let initial = LedFrame {
        colors: vec![LedColor::BLACK; STRIP_LEN],
        frame_number: 0,
    };
    let (leds_tx, leds_rx) = watch::channel(initial);
    let (state_tx, mut state_rx) = watch::channel(OutputState::Connecting);

    let cancel = CancellationToken::new();
    let output_task = tokio::spawn({
        let config = config.clone();
        let cancel = cancel.clone();
        async move {
            output::run(
                config,
                leds_rx,
                state_tx,
                rustiferin::stats::Metrics::new(),
                cancel,
            )
            .await
        }
    });

    let generator = tokio::spawn({
        let cancel = cancel.clone();
        async move { drive_patterns(leds_tx, cancel).await }
    });

    let watcher = tokio::spawn(async move {
        while state_rx.changed().await.is_ok() {
            tracing::info!(state = ?*state_rx.borrow(), "output state");
        }
    });

    tokio::signal::ctrl_c().await.context("ctrl-c handler")?;
    tracing::info!("ctrl-c received, shutting down");
    cancel.cancel();
    let _ = generator.await;
    let _ = output_task.await;
    let _ = watcher.await;
    Ok(())
}

type ColorFn = fn(usize) -> LedColor;

async fn drive_patterns(leds_tx: watch::Sender<LedFrame>, cancel: CancellationToken) {
    let patterns: &[(&str, ColorFn)] = &[
        ("all-red", |_| LedColor::new(255, 0, 0)),
        ("all-green", |_| LedColor::new(0, 255, 0)),
        ("all-blue", |_| LedColor::new(0, 0, 255)),
        ("split-left-red-right-blue", |i| {
            if i < STRIP_LEN / 2 {
                LedColor::new(255, 0, 0)
            } else {
                LedColor::new(0, 0, 255)
            }
        }),
    ];

    let frame_interval = Duration::from_millis(1000 / FPS);
    let mut frame_number: u64 = 0;
    let frames_per_pattern = FPS * PATTERN_SECS;

    'outer: loop {
        for (name, gen) in patterns {
            tracing::info!(pattern = name, "pattern start");
            let colors: Vec<LedColor> = (0..STRIP_LEN).map(gen).collect();
            for _ in 0..frames_per_pattern {
                if cancel.is_cancelled() {
                    break 'outer;
                }
                frame_number += 1;
                let _ = leds_tx.send(LedFrame {
                    colors: colors.clone(),
                    frame_number,
                });
                tokio::select! {
                    _ = tokio::time::sleep(frame_interval) => {}
                    _ = cancel.cancelled() => break 'outer,
                }
            }
        }
    }
}

fn init_logging() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,rustiferin=debug"));
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_target(true))
        .init();
}
