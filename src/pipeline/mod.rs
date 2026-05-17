//! Pipeline: capture frame -> zone averaging -> color correction -> smoothing -> LedFrame.
//!
//! Runs on a dedicated `std::thread` because it is CPU-bound and would otherwise starve
//! tokio's blocking pool. The thread does not host a tokio runtime; it uses the synchronous
//! [`FrameSlot::wait_blocking`] primitive and publishes via a [`tokio::sync::watch`] sender
//! whose `send` is non-blocking.

pub mod color;
pub mod dominant;
pub mod smoothing;
pub mod zones;

use std::sync::Arc;

use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::capture::{FramePool, FrameSlot};
use crate::config::schema::Config;
use crate::shutdown::Shutdown;

use self::color::GammaLut;
use self::smoothing::EmaState;

/// 24-bit RGB color for a single LED. `#[repr(C)]` so downstream code can treat a
/// `&[LedColor]` as a flat byte slice if it ever helps the wire encoder.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct LedColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl LedColor {
    pub const BLACK: LedColor = LedColor { r: 0, g: 0, b: 0 };

    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }
}

/// One snapshot of the LED strip, the pipeline's output unit. Cheap to clone (the
/// `Vec` is a few hundred entries of 3 bytes each).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LedFrame {
    pub colors: Vec<LedColor>,
    pub frame_number: u64,
}

impl LedFrame {
    pub fn black(zone_count: usize, frame_number: u64) -> Self {
        Self {
            colors: vec![LedColor::BLACK; zone_count],
            frame_number,
        }
    }
}

/// Cold-path control surface for the pipeline. Sent by `power` or `tray` to
/// blank the strip during screensaver / idle without tearing down capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipelineCommand {
    /// Publish one all-zero frame, then stop publishing (but keep recycling frame
    /// buffers from the capture side so the pool stays healthy).
    Blackout,
    /// Resume normal publishing.
    Resume,
}

/// Spawn the pipeline OS thread under `shutdown`. The pipeline runs until the shared
/// cancellation token is cancelled.
#[allow(clippy::too_many_arguments)]
pub fn spawn(
    shutdown: &mut Shutdown,
    config: Arc<Config>,
    pool: FramePool,
    frames_in: FrameSlot,
    leds_out: watch::Sender<LedFrame>,
    control: mpsc::Receiver<PipelineCommand>,
    metrics: crate::stats::Metrics,
) -> std::io::Result<()> {
    shutdown.spawn_os_thread("pipeline", move |cancel| {
        run_loop(config, pool, frames_in, leds_out, control, metrics, cancel);
        Ok(())
    })
}

/// Body of the pipeline thread. Pulled out of `spawn` so integration tests can drive it
/// without going through the [`Shutdown`] machinery.
#[allow(clippy::too_many_arguments)]
pub fn run_loop(
    config: Arc<Config>,
    pool: FramePool,
    frames_in: FrameSlot,
    leds_out: watch::Sender<LedFrame>,
    mut control: mpsc::Receiver<PipelineCommand>,
    metrics: crate::stats::Metrics,
    cancel: CancellationToken,
) {
    let span = tracing::info_span!("pipeline");
    let _enter = span.enter();

    tracing::info!(
        averaging = ?config.color.averaging,
        subsample = config.capture.subsample,
        "pipeline starting"
    );

    let mut ema = EmaState::default();
    let zone_count = config.led_matrix.zones.len();
    let mut scratch: Vec<LedColor> = vec![LedColor::default(); zone_count];
    // Reused linear-light pixel buffer for dominant-adv averaging. Stays
    // empty (zero-cost) when `color.averaging` is `Mean`; grows once and
    // is reused for the life of the thread under `DominantAdv`.
    let mut dominant_scratch: Vec<[f32; 3]> = Vec::new();
    let mut frame_number: u64 = 0;
    let mut publishing = true;

    let gamma_lut = GammaLut::new(config.color.gamma);

    // Returns whether the pipeline should keep publishing after draining.
    let drain_control = |control: &mut mpsc::Receiver<PipelineCommand>,
                         publishing: &mut bool,
                         frame_number: u64| {
        while let Ok(cmd) = control.try_recv() {
            match cmd {
                PipelineCommand::Blackout => {
                    tracing::info!("pipeline blackout");
                    *publishing = false;
                    let _ = leds_out.send(LedFrame::black(zone_count, frame_number));
                }
                PipelineCommand::Resume => {
                    tracing::info!("pipeline resume");
                    *publishing = true;
                }
            }
        }
    };

    loop {
        if cancel.is_cancelled() {
            break;
        }

        drain_control(&mut control, &mut publishing, frame_number);

        if !frames_in.wait_blocking() {
            continue;
        }

        // Re-drain after the wait so control messages that arrived while we were blocked
        // are observed before we process the woken frame. Otherwise a `Resume` issued
        // while the pipeline sat on `wait_blocking` would only take effect on the frame
        // after the one that woke us, a 1-frame latency that breaks the test contract.
        drain_control(&mut control, &mut publishing, frame_number);

        let Some(frame) = frames_in.take() else {
            continue;
        };

        zones::average_zones(
            &frame,
            &config.led_matrix,
            config.capture.subsample,
            config.color.averaging,
            &mut dominant_scratch,
            &mut scratch,
        );
        color::gamma(&mut scratch, &gamma_lut);
        color::hsl_offset(&mut scratch, &config.color.hsl_offsets);
        color::white_balance(&mut scratch, config.color.white_balance_kelvin);
        color::night_light(&mut scratch, config.color.night_light_strength);
        // Floor runs *after* gamma so it lifts the gamma-darkened output, matching
        // Firefly's ImageProcessor.correctColors ordering.
        color::luminosity_floor(&mut scratch, config.color.luminosity_floor);
        color::brightness_limit(&mut scratch, config.color.brightness_max);
        ema.step(&mut scratch, config.smoothing.ema_alpha);

        pool.release(frame.buf);

        frame_number += 1;
        if publishing {
            let _ = leds_out.send(LedFrame {
                colors: scratch.clone(),
                frame_number,
            });
            metrics
                .frames_processed()
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::{Frame, PixelFormat};
    use crate::config::schema::{Config, LedMatrixConfig, LedZone};
    use std::time::Duration;
    use tokio::time::timeout;

    fn solid_frame(width: u32, height: u32, b: u8, g: u8, r: u8) -> Frame {
        let stride = width * 4;
        let mut buf = vec![0u8; (stride * height) as usize];
        for px in buf.chunks_exact_mut(4) {
            px[0] = b;
            px[1] = g;
            px[2] = r;
            px[3] = 255;
        }
        Frame {
            buf,
            width,
            height,
            stride,
            format: PixelFormat::Bgra,
        }
    }

    fn one_zone_config() -> Config {
        Config {
            led_matrix: LedMatrixConfig {
                reference_width: 4,
                reference_height: 4,
                zones: vec![LedZone {
                    x: 0,
                    y: 0,
                    w: 4,
                    h: 4,
                }],
                ..Default::default()
            },
            color: crate::config::schema::ColorConfig {
                gamma: 1.0,
                white_balance_kelvin: 6500,
                night_light_strength: 0.0,
                brightness_max: 255,
                hsl_offsets: Default::default(),
                ..Default::default()
            },
            smoothing: crate::config::schema::SmoothingConfig { ema_alpha: 1.0 },
            ..Default::default()
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn blackout_publishes_black_frame_and_stops_publishing() {
        let config = Arc::new(one_zone_config());
        let pool = FramePool::new(2, 64);
        let slot = FrameSlot::new();
        let (leds_tx, mut leds_rx) = watch::channel(LedFrame {
            colors: vec![LedColor::default()],
            frame_number: 0,
        });
        let (ctrl_tx, ctrl_rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();

        let cfg_for_thread = config.clone();
        let pool_for_thread = pool.clone();
        let slot_for_thread = slot.clone();
        let cancel_for_thread = cancel.clone();
        let handle = std::thread::spawn(move || {
            run_loop(
                cfg_for_thread,
                pool_for_thread,
                slot_for_thread,
                leds_tx,
                ctrl_rx,
                crate::stats::Metrics::new(),
                cancel_for_thread,
            );
        });

        // Establish baseline: send a red frame so we know publishing works.
        let mut buf = pool.acquire();
        buf.extend_from_slice(&solid_frame(4, 4, 0, 0, 255).buf);
        if let Some(old) = slot.put(Frame {
            buf,
            width: 4,
            height: 4,
            stride: 16,
            format: PixelFormat::Bgra,
        }) {
            pool.release(old.buf);
        }

        timeout(Duration::from_secs(2), leds_rx.changed())
            .await
            .expect("first publish")
            .expect("sender alive");
        assert_eq!(leds_rx.borrow_and_update().colors[0].r, 255);

        // Blackout: should produce one all-zero frame and then stop.
        ctrl_tx.send(PipelineCommand::Blackout).await.unwrap();

        timeout(Duration::from_secs(2), leds_rx.changed())
            .await
            .expect("blackout frame")
            .expect("sender alive");
        assert_eq!(leds_rx.borrow_and_update().colors[0], LedColor::BLACK);

        // Feed several more frames; the pipeline must consume them but not publish.
        for _ in 0..3 {
            let mut buf = pool.acquire();
            buf.extend_from_slice(&solid_frame(4, 4, 0, 255, 0).buf);
            if let Some(old) = slot.put(Frame {
                buf,
                width: 4,
                height: 4,
                stride: 16,
                format: PixelFormat::Bgra,
            }) {
                pool.release(old.buf);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            timeout(Duration::from_millis(200), leds_rx.changed())
                .await
                .is_err(),
            "no publish should happen while blacked out"
        );

        // Resume: publishing must come back.
        ctrl_tx.send(PipelineCommand::Resume).await.unwrap();
        let mut buf = pool.acquire();
        buf.extend_from_slice(&solid_frame(4, 4, 255, 0, 0).buf);
        if let Some(old) = slot.put(Frame {
            buf,
            width: 4,
            height: 4,
            stride: 16,
            format: PixelFormat::Bgra,
        }) {
            pool.release(old.buf);
        }
        timeout(Duration::from_secs(2), leds_rx.changed())
            .await
            .expect("post-resume publish")
            .expect("sender alive");
        assert_eq!(leds_rx.borrow_and_update().colors[0].b, 255);

        cancel.cancel();
        handle.join().unwrap();
    }
}
