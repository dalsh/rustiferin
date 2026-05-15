//! Shared test helpers for end-to-end integration tests.

use std::sync::Arc;
use std::time::{Duration, Instant};

use rustiferin::capture::{
    CaptureSource, FakeCapture, FramePool, FrameSlot, FrameTemplate, PixelFormat,
};
use rustiferin::config::schema::{
    ColorConfig, Config, HslOffsets, LedMatrixConfig, LedZone, SmoothingConfig,
};
use rustiferin::output::fake::FakeOutput;
use rustiferin::pipeline::{self, LedFrame};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

const FRAME_INTERVAL: Duration = Duration::from_millis(20);
const SETTLE_DEADLINE: Duration = Duration::from_secs(2);
const POST_SETTLE_TICK: Duration = Duration::from_millis(150);
// Generous upper bound, sized for the 3840×2160 BGRA case in the resolution-scaling test.
const POOL_BUFFER_CAPACITY: usize = 4 * 3840 * 2160;

/// Build a configuration with the pipeline collapsed to identity transforms: no
/// gamma, no white-balance shift, no smoothing. Tests override fields per case.
pub fn identity_config(zones: Vec<LedZone>, reference_width: u32, reference_height: u32) -> Config {
    Config {
        led_matrix: LedMatrixConfig {
            reference_width,
            reference_height,
            zones,
            ..Default::default()
        },
        color: ColorConfig {
            gamma: 1.0,
            white_balance_kelvin: 6500,
            night_light_strength: 0.0,
            brightness_max: 255,
            hsl_offsets: HslOffsets::default(),
            ..Default::default()
        },
        smoothing: SmoothingConfig { ema_alpha: 1.0 },
        ..Default::default()
    }
}

pub fn solid_color_frame(width: u32, height: u32, b: u8, g: u8, r: u8) -> FrameTemplate {
    let stride = width * 4;
    let mut pixels = vec![0u8; (stride * height) as usize];
    for px in pixels.chunks_exact_mut(4) {
        px[0] = b;
        px[1] = g;
        px[2] = r;
        px[3] = 255;
    }
    FrameTemplate {
        pixels,
        width,
        height,
        stride,
        format: PixelFormat::Bgra,
    }
}

/// Left half / right half split. Both halves are solid colors.
pub fn split_frame_bgra(
    width: u32,
    height: u32,
    left: (u8, u8, u8),
    right: (u8, u8, u8),
) -> FrameTemplate {
    let stride = width * 4;
    let mut pixels = vec![0u8; (stride * height) as usize];
    let half = width / 2;
    for y in 0..height {
        for x in 0..width {
            let p = (y * stride + x * 4) as usize;
            let (b, g, r) = if x < half { left } else { right };
            pixels[p] = b;
            pixels[p + 1] = g;
            pixels[p + 2] = r;
            pixels[p + 3] = 255;
        }
    }
    FrameTemplate {
        pixels,
        width,
        height,
        stride,
        format: PixelFormat::Bgra,
    }
}

/// Solid BGRA frame with extra per-row padding bytes after the pixel data.
pub fn padded_solid_frame(
    width: u32,
    height: u32,
    padding_per_row: u32,
    b: u8,
    g: u8,
    r: u8,
) -> FrameTemplate {
    let stride = width * 4 + padding_per_row;
    let mut pixels = vec![0u8; (stride * height) as usize];
    for y in 0..height {
        for x in 0..width {
            let p = (y * stride + x * 4) as usize;
            pixels[p] = b;
            pixels[p + 1] = g;
            pixels[p + 2] = r;
            pixels[p + 3] = 255;
        }
        // Overwrite the padding bytes with sentinel garbage so an off-by-one stride
        // computation in the consumer would corrupt the averaged color.
        for k in 0..padding_per_row as usize {
            pixels[(y * stride) as usize + (width * 4) as usize + k] = 0xFF;
        }
    }
    FrameTemplate {
        pixels,
        width,
        height,
        stride,
        format: PixelFormat::Bgra,
    }
}

/// Drive the real pipeline thread with [`FakeCapture`] and a [`FakeOutput`]
/// subscriber, returning every published [`LedFrame`] in publish order.
///
/// `min_frames` is the number of distinct frames the test expects to observe
/// before settling; the helper waits up to [`SETTLE_DEADLINE`] for them.
pub async fn run_e2e(
    config: Config,
    templates: Vec<FrameTemplate>,
    min_frames: usize,
) -> Vec<LedFrame> {
    let zone_count = config.led_matrix.zones.len();
    let config = Arc::new(config);
    let pool = FramePool::new(2, POOL_BUFFER_CAPACITY);
    let slot = FrameSlot::new();
    let (leds_tx, leds_rx) = watch::channel(LedFrame::black(zone_count, 0));
    let (_ctrl_tx, ctrl_rx) = mpsc::channel(8);
    let cancel = CancellationToken::new();

    let cfg_t = config.clone();
    let pool_t = pool.clone();
    let slot_t = slot.clone();
    let cancel_t = cancel.clone();
    let pipeline_thread = std::thread::spawn(move || {
        pipeline::run_loop(
            cfg_t,
            pool_t,
            slot_t,
            leds_tx,
            ctrl_rx,
            rustiferin::stats::Metrics::new(),
            cancel_t,
        );
    });

    let fake_output = FakeOutput::new();
    let cancel_for_output = cancel.clone();
    let output_for_task = fake_output.clone();
    let output_handle = tokio::spawn(async move {
        output_for_task.run(leds_rx, cancel_for_output).await;
    });

    let capture = FakeCapture::new(templates, FRAME_INTERVAL);
    capture
        .run(
            pool.clone(),
            slot.clone(),
            rustiferin::stats::Metrics::new(),
            cancel.clone(),
        )
        .await
        .expect("fake capture should succeed");

    let deadline = Instant::now() + SETTLE_DEADLINE;
    while fake_output.frames().len() < min_frames && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(POST_SETTLE_TICK).await;

    cancel.cancel();
    output_handle.await.expect("output task joins");
    pipeline_thread.join().expect("pipeline thread joins");

    fake_output.frames()
}

/// Channel-wise `(r,g,b)` tuple comparison with a tolerance, for assertions on
/// EMA / gamma intermediate values.
pub fn assert_color_near(actual: rustiferin::pipeline::LedColor, expected: (u8, u8, u8), tol: u8) {
    let (er, eg, eb) = expected;
    let dr = actual.r.abs_diff(er);
    let dg = actual.g.abs_diff(eg);
    let db = actual.b.abs_diff(eb);
    assert!(
        dr <= tol && dg <= tol && db <= tol,
        "color mismatch: got ({}, {}, {}) expected ({}, {}, {}) tol {}",
        actual.r,
        actual.g,
        actual.b,
        er,
        eg,
        eb,
        tol
    );
}
