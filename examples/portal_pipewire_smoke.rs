//! End-to-end smoke driver for `PortalCapture`, exercises FramePool + FrameSlot
//! integration on a real Plasma Wayland session.
//!
//! Run: `cargo run --features wayland --example portal_pipewire_smoke`
//!
//! Drives the production `PortalCapture` for ~3 seconds, draining the
//! `FrameSlot` from an async consumer and logging frame metadata. The first
//! run prompts via the portal dialog; subsequent runs reuse the restore token
//! stored under `$XDG_STATE_HOME/rustiferin/restore_token` and start silently.

use std::sync::Arc;
use std::time::Duration;

use rustiferin::capture::{portal::PortalCapture, CaptureSource, FramePool, FrameSlot};
use tokio_util::sync::CancellationToken;

const PIXEL_BUFFER_HINT: usize = 4 * 3840 * 2160; // up to 4K BGRA
const POOL_SIZE: usize = 4;
const RUN_FOR: Duration = Duration::from_secs(3);

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,rustiferin=debug")),
        )
        .init();

    let pool = FramePool::new(POOL_SIZE, PIXEL_BUFFER_HINT);
    let slot = FrameSlot::new();
    let cancel = CancellationToken::new();

    let restore_path = std::env::var_os("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(std::env::var_os("HOME").unwrap()).join(".local/state")
        })
        .join("rustiferin")
        .join("restore_token");
    let capture = PortalCapture::new(restore_path, 30);

    let capture_task = {
        let pool = pool.clone();
        let slot = slot.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            capture
                .run(pool, slot, rustiferin::stats::Metrics::new(), cancel)
                .await
        })
    };

    let frames = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let consumer = {
        let slot = slot.clone();
        let pool = pool.clone();
        let cancel = cancel.clone();
        let frames = frames.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = slot.wait() => {}
                }
                if let Some(frame) = slot.take() {
                    let n = frames.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                    if n <= 3 || n.is_multiple_of(30) {
                        tracing::info!(
                            frame = n,
                            width = frame.width,
                            height = frame.height,
                            stride = frame.stride,
                            format = ?frame.format,
                            bytes = frame.buf.len(),
                            "drained frame"
                        );
                    }
                    pool.release(frame.buf);
                }
            }
        })
    };

    tokio::time::sleep(RUN_FOR).await;
    tracing::info!("stopping");
    cancel.cancel();

    let _ = capture_task.await?;
    let _ = consumer.await;
    tracing::info!(
        total = frames.load(std::sync::atomic::Ordering::Relaxed),
        "smoke done"
    );
    Ok(())
}
