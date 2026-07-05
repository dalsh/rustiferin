//! `CaptureSource` implementation for the KMS backend: poll `gsr-kms-server`
//! for the scanout plane, import it via EGL, feed the pipeline. Runs the
//! blocking GL/socket work on a dedicated blocking task (the GL context is
//! thread-bound), checking the cancellation token each frame.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio_util::sync::CancellationToken;

use super::client::{close_response_fds, GsrKmsClient};
use super::egl::KmsImporter;
use super::protocol::select_primary_plane;
use crate::capture::{CaptureSource, Frame, FramePool, FrameSlot};
use crate::stats::Metrics;

pub struct KmsCapture {
    card: String,
    render_node: String,
    target_fps: u32,
}

impl KmsCapture {
    pub fn new(card: String, render_node: String, target_fps: u32) -> Self {
        Self {
            card,
            render_node,
            target_fps,
        }
    }

    fn run_blocking(
        self,
        pool: FramePool,
        slot: FrameSlot,
        metrics: Metrics,
        cancel: CancellationToken,
    ) -> Result<()> {
        use std::sync::atomic::Ordering;

        // Fail fast: if the helper is missing or the GPU can't import, surface it.
        let mut client = GsrKmsClient::spawn(&self.card).context("start gsr-kms-server")?;
        let importer = KmsImporter::new(&self.render_node).context("init EGL importer")?;
        let interval = Duration::from_secs_f64(1.0 / self.target_fps.max(1) as f64);
        tracing::info!(
            card = self.card,
            render_node = self.render_node,
            target_fps = self.target_fps,
            "kms capture started"
        );

        while !cancel.is_cancelled() {
            let start = Instant::now();
            let resp = client.get_kms().context("GET_KMS")?;
            if let Some(idx) = select_primary_plane(&resp, None) {
                let mut buf = pool.acquire();
                let read = importer.read_plane(&resp.items[idx], &mut buf);
                match read {
                    Ok((width, height, format)) => {
                        let frame = Frame {
                            buf,
                            width,
                            height,
                            stride: width * 4,
                            format,
                        };
                        if let Some(displaced) = slot.put(frame) {
                            pool.release(displaced.buf);
                        }
                        metrics.frames_captured().fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => {
                        pool.release(buf);
                        close_response_fds(&resp);
                        return Err(e).context("import scanout plane");
                    }
                }
            }
            close_response_fds(&resp);

            let elapsed = start.elapsed();
            if elapsed < interval {
                std::thread::sleep(interval - elapsed);
            }
        }
        tracing::info!(
            frames = metrics.frames_captured().load(Ordering::Relaxed),
            "kms capture stopped"
        );
        Ok(())
    }
}

#[async_trait::async_trait]
impl CaptureSource for KmsCapture {
    async fn run(
        self,
        pool: FramePool,
        slot: FrameSlot,
        metrics: Metrics,
        cancel: CancellationToken,
    ) -> Result<()> {
        tokio::task::spawn_blocking(move || self.run_blocking(pool, slot, metrics, cancel))
            .await
            .context("kms capture task panicked")?
    }
}
