//! End-to-end integration tests covering the wiring of capture -> pipeline -> output.
//!
//! These exercise the real pipeline thread with [`FakeCapture`] feeding frames and
//! [`FakeOutput`] subscribing to the `watch::Receiver<LedFrame>`, so no PipeWire,
//! D-Bus, or MQTT is required.

#![cfg(feature = "test-fakes")]

mod helpers;

use helpers::{
    assert_color_near, identity_config, padded_solid_frame, run_e2e, solid_color_frame,
    split_frame_bgra,
};
use rustiferin::capture::{FrameTemplate, PixelFormat};
use rustiferin::config::schema::{AveragingMode, LedZone};

#[tokio::test(flavor = "current_thread")]
async fn identity_pipeline_preserves_color() {
    let cfg = identity_config(
        vec![LedZone {
            x: 0,
            y: 0,
            w: 8,
            h: 8,
        }],
        8,
        8,
    );
    // BGRA red.
    let frame = solid_color_frame(8, 8, 0, 0, 255);
    let frames = run_e2e(cfg, vec![frame], 1).await;
    assert!(
        !frames.is_empty(),
        "pipeline must publish at least one frame"
    );
    assert_color_near(
        *frames.last().unwrap().colors.first().unwrap(),
        (255, 0, 0),
        0,
    );
}

#[tokio::test(flavor = "current_thread")]
async fn two_zone_split_frame_is_split_correctly() {
    let cfg = identity_config(
        vec![
            LedZone {
                x: 0,
                y: 0,
                w: 4,
                h: 8,
            },
            LedZone {
                x: 4,
                y: 0,
                w: 4,
                h: 8,
            },
        ],
        8,
        8,
    );
    // Left half red, right half green.
    let frame = split_frame_bgra(8, 8, (0, 0, 255), (0, 255, 0));
    let frames = run_e2e(cfg, vec![frame], 1).await;
    let last = frames.last().expect("at least one frame");
    assert_eq!(last.colors.len(), 2);
    assert_color_near(last.colors[0], (255, 0, 0), 0);
    assert_color_near(last.colors[1], (0, 255, 0), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn gamma_22_darkens_mid_gray() {
    let mut cfg = identity_config(
        vec![LedZone {
            x: 0,
            y: 0,
            w: 4,
            h: 4,
        }],
        4,
        4,
    );
    cfg.color.gamma = 2.2;
    // BGRA mid-gray (128).
    let frame = solid_color_frame(4, 4, 128, 128, 128);
    let frames = run_e2e(cfg, vec![frame], 1).await;
    // (128/255)^2.2 * 255 ≈ 55.5
    let c = frames.last().unwrap().colors[0];
    assert_color_near(c, (56, 56, 56), 1);
    assert!(c.r < 128, "gamma 2.2 must darken midtones, got r={}", c.r);
}

#[tokio::test(flavor = "current_thread")]
async fn ema_smoothing_converges_over_repeated_frames() {
    let mut cfg = identity_config(
        vec![LedZone {
            x: 0,
            y: 0,
            w: 4,
            h: 4,
        }],
        4,
        4,
    );
    cfg.smoothing.time_constant_ms = 50.0;
    let frames_in: Vec<_> = (0..20)
        .map(|_| solid_color_frame(4, 4, 0, 0, 255))
        .collect();
    let observed = run_e2e(cfg, frames_in, 1).await;
    // Feeding the same color repeatedly, the EMA settles onto it within 1 LSB.
    let last = observed.last().expect("at least one frame");
    assert_color_near(last.colors[0], (255, 0, 0), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn brightness_limit_clamps_white() {
    let mut cfg = identity_config(
        vec![LedZone {
            x: 0,
            y: 0,
            w: 4,
            h: 4,
        }],
        4,
        4,
    );
    cfg.color.brightness_max = 128;
    let frame = solid_color_frame(4, 4, 255, 255, 255);
    let frames = run_e2e(cfg, vec![frame], 1).await;
    let c = frames.last().unwrap().colors[0];
    assert_color_near(c, (128, 128, 128), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn stride_padded_frame_averages_correctly() {
    let cfg = identity_config(
        vec![LedZone {
            x: 0,
            y: 0,
            w: 4,
            h: 4,
        }],
        4,
        4,
    );
    // Stride pads 8 bytes per row past the pixel data; padding bytes are sentinel 0xFF.
    let frame = padded_solid_frame(4, 4, 8, 50, 100, 150);
    let frames = run_e2e(cfg, vec![frame], 1).await;
    assert_color_near(frames.last().unwrap().colors[0], (150, 100, 50), 0);
}

/// Build a BGRA frame whose top fraction is one solid colour and bottom is
/// another. Used to exercise the dominant-vs-mean divergence: with the fraction
/// skewed (60/40), the arithmetic mean produces a mix of the two while
/// dominant-mode locks onto the larger region's colour.
fn two_band_frame_bgra(
    width: u32,
    height: u32,
    top_rows: u32,
    top: (u8, u8, u8),
    bot: (u8, u8, u8),
) -> FrameTemplate {
    let stride = width * 4;
    let mut pixels = vec![0u8; (stride * height) as usize];
    for y in 0..height {
        for x in 0..width {
            let p = (y * stride + x * 4) as usize;
            let (b, g, r) = if y < top_rows { top } else { bot };
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

#[tokio::test(flavor = "current_thread")]
async fn dominant_mode_returns_largest_region_color() {
    // 60% red on top, 40% green on bottom. Mean would land somewhere between
    // ~(193, 168, 0) sRGB. Dominant must pick pure red.
    let mut cfg = identity_config(
        vec![LedZone {
            x: 0,
            y: 0,
            w: 10,
            h: 10,
        }],
        10,
        10,
    );
    cfg.color.averaging = AveragingMode::DominantAdv;
    let frame = two_band_frame_bgra(10, 10, 6, (0, 0, 255), (0, 255, 0));
    let frames = run_e2e(cfg, vec![frame], 1).await;
    let last = frames.last().expect("at least one frame");
    // Tolerance covers re-encode rounding; the centroid lands on exactly red.
    assert_color_near(last.colors[0], (255, 0, 0), 2);
}

#[tokio::test(flavor = "current_thread")]
async fn mean_mode_is_unchanged_by_dominant_addition() {
    // Same 60/40 frame: default `Mean` mode must continue producing the
    // linear-light average, not the dominant colour. Pins backward compat.
    let cfg = identity_config(
        vec![LedZone {
            x: 0,
            y: 0,
            w: 10,
            h: 10,
        }],
        10,
        10,
    );
    assert_eq!(cfg.color.averaging, AveragingMode::Mean);
    let frame = two_band_frame_bgra(10, 10, 6, (0, 0, 255), (0, 255, 0));
    let frames = run_e2e(cfg, vec![frame], 1).await;
    let last = frames.last().expect("at least one frame");
    // Linear-light mean of (0.6 red linear, 0.4 green linear) re-encoded.
    // Red channel: 0.6 -> ~ 203 sRGB. Green channel: 0.4 -> ~ 171 sRGB.
    // Pin "not pure red" rather than the exact value, that's the only
    // claim we make beyond the existing per-component mean tests.
    let c = last.colors[0];
    assert!(
        c.g > 100,
        "mean must include green contribution, got {:?}",
        c
    );
    assert!(c.r < 255, "mean cannot be pure red, got {:?}", c);
}

#[tokio::test(flavor = "current_thread")]
async fn upscaled_frame_against_reference_resolution_preserves_zone_split() {
    // Reference is 1920×1080 with two left/right halves; the frame the pipeline
    // actually sees is 3840×2160 (a 2× upscale). Zone bounds must rescale.
    let cfg = identity_config(
        vec![
            LedZone {
                x: 0,
                y: 0,
                w: 960,
                h: 1080,
            },
            LedZone {
                x: 960,
                y: 0,
                w: 960,
                h: 1080,
            },
        ],
        1920,
        1080,
    );
    // 3840×2160 BGRA frame: left half red, right half green. Memory: ~32 MiB;
    // FakeCapture copies it once into a pool buffer of capacity ~32 MiB (see
    // `helpers::POOL_BUFFER_CAPACITY`).
    let frame = split_frame_bgra(3840, 2160, (0, 0, 255), (0, 255, 0));
    let frames = run_e2e(cfg, vec![frame], 1).await;
    let last = frames.last().expect("at least one frame");
    assert_eq!(last.colors.len(), 2);
    // Allow a couple of LSB of tolerance, `scale_zone` rounds and the boundary
    // between zones is on a pixel that could fall either side after rounding.
    assert_color_near(last.colors[0], (255, 0, 0), 2);
    assert_color_near(last.colors[1], (0, 255, 0), 2);
}
