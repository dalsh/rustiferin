//! Per-zone pixel averaging: reduce each frame to one [`LedColor`] per configured zone.
//!
//! Zones are authored against `LedMatrixConfig::reference_{width,height}`; the actual
//! frame may differ (HiDPI scaling, user-chosen resolution mismatch). Scale factors are
//! computed once per frame and applied per zone; when the frame matches the reference
//! resolution the rescale is the identity transform.

use crate::capture::{Frame, PixelFormat};
use crate::config::schema::{LedMatrixConfig, LedZone};
use crate::pipeline::LedColor;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScaledZone {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

pub fn scale_zone(
    zone: &LedZone,
    sx: f32,
    sy: f32,
    frame_width: u32,
    frame_height: u32,
) -> ScaledZone {
    let x = ((zone.x as f32) * sx).round() as u32;
    let y = ((zone.y as f32) * sy).round() as u32;
    let w_raw = ((zone.w as f32) * sx).round() as u32;
    let h_raw = ((zone.h as f32) * sy).round() as u32;
    let w = w_raw.min(frame_width.saturating_sub(x));
    let h = h_raw.min(frame_height.saturating_sub(y));
    ScaledZone { x, y, w, h }
}

/// Average the pixels inside each zone's scaled rectangle into `out[i]`.
///
/// Panics in debug mode if `out.len() != cfg.zones.len()`. In release the loop is bounded
/// by `cfg.zones.iter().zip(out.iter_mut())` so a mismatched output buffer simply ignores
/// the trailing entries, but the pipeline always sizes `scratch` to match.
pub fn average_zones(frame: &Frame, cfg: &LedMatrixConfig, subsample: u32, out: &mut [LedColor]) {
    debug_assert_eq!(out.len(), cfg.zones.len());
    let step = subsample.max(1);
    let sx = frame.width as f32 / cfg.reference_width.max(1) as f32;
    let sy = frame.height as f32 / cfg.reference_height.max(1) as f32;
    for (zone, slot) in cfg.zones.iter().zip(out.iter_mut()) {
        let scaled = scale_zone(zone, sx, sy, frame.width, frame.height);
        *slot = if scaled.w == 0 || scaled.h == 0 {
            LedColor::default()
        } else {
            average_pixels(frame, &scaled, step)
        };
    }
}

fn average_pixels(frame: &Frame, zone: &ScaledZone, step: u32) -> LedColor {
    let (r_idx, g_idx, b_idx) = channel_offsets(frame.format);
    let stride = frame.stride as usize;
    let bytes_per_px = pixel_size(frame.format);

    let mut sum_r: u64 = 0;
    let mut sum_g: u64 = 0;
    let mut sum_b: u64 = 0;
    let mut count: u64 = 0;

    let mut dy = 0u32;
    while dy < zone.h {
        let row_start = (zone.y + dy) as usize * stride;
        let mut dx = 0u32;
        while dx < zone.w {
            let px = row_start + (zone.x + dx) as usize * bytes_per_px;
            // Boundary defence: a malformed `stride` should not crash the pipeline.
            // Skip the row if it would over-read.
            if px + bytes_per_px > frame.buf.len() {
                dx += step;
                continue;
            }
            sum_r += frame.buf[px + r_idx] as u64;
            sum_g += frame.buf[px + g_idx] as u64;
            sum_b += frame.buf[px + b_idx] as u64;
            count += 1;
            dx += step;
        }
        dy += step;
    }

    if count == 0 {
        return LedColor::default();
    }
    LedColor {
        r: (sum_r / count) as u8,
        g: (sum_g / count) as u8,
        b: (sum_b / count) as u8,
    }
}

fn channel_offsets(format: PixelFormat) -> (usize, usize, usize) {
    match format {
        PixelFormat::Bgra | PixelFormat::Bgrx => (2, 1, 0),
        PixelFormat::Rgba => (0, 1, 2),
        PixelFormat::Xrgb => (1, 2, 3),
    }
}

/// Bytes per pixel for each supported [`PixelFormat`]. All variants are 4-byte
/// today, but keeping this next to [`channel_offsets`] means a future 3-byte
/// or 8-byte format can be added without an off-by-one in the inner loop.
fn pixel_size(format: PixelFormat) -> usize {
    match format {
        PixelFormat::Bgra | PixelFormat::Bgrx | PixelFormat::Rgba | PixelFormat::Xrgb => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid_bgra(width: u32, height: u32, b: u8, g: u8, r: u8) -> Frame {
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

    fn full_zone(rw: u32, rh: u32) -> LedMatrixConfig {
        LedMatrixConfig {
            reference_width: rw,
            reference_height: rh,
            zones: vec![LedZone {
                x: 0,
                y: 0,
                w: rw,
                h: rh,
            }],
            ..Default::default()
        }
    }

    #[test]
    fn solid_red_frame_averages_to_red() {
        let frame = solid_bgra(8, 8, 0, 0, 255);
        let cfg = full_zone(8, 8);
        let mut out = vec![LedColor::default(); 1];
        average_zones(&frame, &cfg, 1, &mut out);
        assert_eq!(out[0], LedColor::new(255, 0, 0));
    }

    #[test]
    fn two_halves_average_independently() {
        let mut frame = solid_bgra(8, 8, 0, 0, 0);
        for y in 0..8 {
            for x in 0..4 {
                let p = (y * 8 + x) * 4;
                frame.buf[p + 2] = 200; // left half red
            }
            for x in 4..8 {
                let p = (y * 8 + x) * 4;
                frame.buf[p] = 100; // right half blue
            }
        }
        let cfg = LedMatrixConfig {
            reference_width: 8,
            reference_height: 8,
            zones: vec![
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
            ..Default::default()
        };
        let mut out = vec![LedColor::default(); 2];
        average_zones(&frame, &cfg, 1, &mut out);
        assert_eq!(out[0], LedColor::new(200, 0, 0));
        assert_eq!(out[1], LedColor::new(0, 0, 100));
    }

    #[test]
    fn honors_stride_with_padding() {
        // 4×4 image, stride = 24 bytes (16 px + 8 padding bytes per row).
        let width = 4u32;
        let height = 4u32;
        let stride = 24u32;
        let mut buf = vec![0u8; (stride * height) as usize];
        for y in 0..height {
            for x in 0..width {
                let p = (y * stride + x * 4) as usize;
                buf[p] = 50; // B
                buf[p + 1] = 100; // G
                buf[p + 2] = 150; // R
            }
        }
        let frame = Frame {
            buf,
            width,
            height,
            stride,
            format: PixelFormat::Bgra,
        };
        let cfg = full_zone(width, height);
        let mut out = vec![LedColor::default(); 1];
        average_zones(&frame, &cfg, 1, &mut out);
        assert_eq!(out[0], LedColor::new(150, 100, 50));
    }

    #[test]
    fn rescales_zones_2x_upscale() {
        // Author at 4×4, capture at 8×8. Zone (0,0,2,2) covers the top-left quarter
        // of the reference frame, which is the top-left 4×4 of the actual frame.
        let mut buf = vec![0u8; 8 * 8 * 4];
        // top-left 4x4 = red, rest = blue
        for y in 0..8 {
            for x in 0..8 {
                let p = (y * 8 + x) * 4;
                if x < 4 && y < 4 {
                    buf[p + 2] = 255;
                } else {
                    buf[p] = 255;
                }
            }
        }
        let frame = Frame {
            buf,
            width: 8,
            height: 8,
            stride: 32,
            format: PixelFormat::Bgra,
        };
        let cfg = LedMatrixConfig {
            reference_width: 4,
            reference_height: 4,
            zones: vec![LedZone {
                x: 0,
                y: 0,
                w: 2,
                h: 2,
            }],
            ..Default::default()
        };
        let mut out = vec![LedColor::default(); 1];
        average_zones(&frame, &cfg, 1, &mut out);
        assert_eq!(out[0], LedColor::new(255, 0, 0));
    }

    #[test]
    fn rescales_zones_collapsed_zone_emits_black() {
        // Heavy downscale: ref 100×100 with a 1×1 zone, actual 10×10 frame. Scale 0.1
        // makes the scaled width and height round to zero. Must emit black, not panic.
        let frame = solid_bgra(10, 10, 0, 0, 255);
        let cfg = LedMatrixConfig {
            reference_width: 100,
            reference_height: 100,
            zones: vec![LedZone {
                x: 0,
                y: 0,
                w: 1,
                h: 1,
            }],
            ..Default::default()
        };
        let mut out = vec![LedColor::default(); 1];
        average_zones(&frame, &cfg, 1, &mut out);
        assert_eq!(out[0], LedColor::default());
    }

    #[test]
    fn rgba_format_reads_red_in_byte_zero() {
        let width = 2u32;
        let height = 2u32;
        let stride = width * 4;
        let mut buf = vec![0u8; (stride * height) as usize];
        for px in buf.chunks_exact_mut(4) {
            px[0] = 200; // R
            px[1] = 100; // G
            px[2] = 50; // B
            px[3] = 255;
        }
        let frame = Frame {
            buf,
            width,
            height,
            stride,
            format: PixelFormat::Rgba,
        };
        let cfg = full_zone(width, height);
        let mut out = vec![LedColor::default(); 1];
        average_zones(&frame, &cfg, 1, &mut out);
        assert_eq!(out[0], LedColor::new(200, 100, 50));
    }

    #[test]
    fn xrgb_format_ignores_padding_byte() {
        let width = 2u32;
        let height = 2u32;
        let stride = width * 4;
        let mut buf = vec![0u8; (stride * height) as usize];
        for px in buf.chunks_exact_mut(4) {
            px[0] = 99; // padding (must be ignored)
            px[1] = 10; // R
            px[2] = 20; // G
            px[3] = 30; // B
        }
        let frame = Frame {
            buf,
            width,
            height,
            stride,
            format: PixelFormat::Xrgb,
        };
        let cfg = full_zone(width, height);
        let mut out = vec![LedColor::default(); 1];
        average_zones(&frame, &cfg, 1, &mut out);
        assert_eq!(out[0], LedColor::new(10, 20, 30));
    }

    fn scalar_reference(frame: &Frame, cfg: &LedMatrixConfig) -> Vec<LedColor> {
        // Independent triple-loop reference: no factoring out, no bytes_per_px constant.
        // Its only purpose is to disagree with the production impl if that one is wrong.
        let (r_idx, g_idx, b_idx) = match frame.format {
            PixelFormat::Bgra | PixelFormat::Bgrx => (2usize, 1usize, 0usize),
            PixelFormat::Rgba => (0, 1, 2),
            PixelFormat::Xrgb => (1, 2, 3),
        };
        let sx = frame.width as f32 / cfg.reference_width.max(1) as f32;
        let sy = frame.height as f32 / cfg.reference_height.max(1) as f32;
        let mut out = Vec::with_capacity(cfg.zones.len());
        for zone in &cfg.zones {
            let x = ((zone.x as f32) * sx).round() as u32;
            let y = ((zone.y as f32) * sy).round() as u32;
            let w = (((zone.w as f32) * sx).round() as u32).min(frame.width.saturating_sub(x));
            let h = (((zone.h as f32) * sy).round() as u32).min(frame.height.saturating_sub(y));
            if w == 0 || h == 0 {
                out.push(LedColor::default());
                continue;
            }
            let mut sr = 0u64;
            let mut sg = 0u64;
            let mut sb = 0u64;
            for dy in 0..h {
                for dx in 0..w {
                    let p = ((y + dy) * frame.stride + (x + dx) * 4) as usize;
                    sr += frame.buf[p + r_idx] as u64;
                    sg += frame.buf[p + g_idx] as u64;
                    sb += frame.buf[p + b_idx] as u64;
                }
            }
            let n = (w as u64) * (h as u64);
            out.push(LedColor {
                r: (sr / n) as u8,
                g: (sg / n) as u8,
                b: (sb / n) as u8,
            });
        }
        out
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(100))]
        #[test]
        fn matches_scalar_reference(
            width in 8u32..64,
            height in 8u32..64,
            scale_idx in 0u32..3,
            seed in proptest::prelude::any::<u64>(),
        ) {
            // Deterministic LCG fill, keeps the property reproducible without dragging in rand.
            let stride = width * 4;
            let mut state = seed.wrapping_add(1);
            let mut buf = vec![0u8; (stride * height) as usize];
            for byte in buf.iter_mut() {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                *byte = (state >> 56) as u8;
            }
            let frame = Frame {
                buf,
                width,
                height,
                stride,
                format: PixelFormat::Bgra,
            };
            let (rw, rh) = match scale_idx {
                0 => (width, height),
                1 => (width / 2, height / 2),
                _ => (width * 2, height * 2),
            };
            let cfg = LedMatrixConfig {
                reference_width: rw.max(1),
                reference_height: rh.max(1),
                zones: vec![
                    LedZone { x: 0, y: 0, w: rw.max(1) / 2, h: rh.max(1) / 2 },
                    LedZone { x: rw.max(1) / 2, y: 0, w: rw.max(1) / 2, h: rh.max(1) / 2 },
                    LedZone { x: 0, y: rh.max(1) / 2, w: rw.max(1) / 2, h: rh.max(1) / 2 },
                    LedZone { x: rw.max(1) / 2, y: rh.max(1) / 2, w: rw.max(1) / 2, h: rh.max(1) / 2 },
                ],
                ..Default::default()
            };
            let mut out = vec![LedColor::default(); cfg.zones.len()];
            average_zones(&frame, &cfg, 1, &mut out);
            let expected = scalar_reference(&frame, &cfg);
            proptest::prop_assert_eq!(out, expected);
        }
    }

    #[test]
    fn scale_zone_clamps_against_frame_bounds() {
        let zone = LedZone {
            x: 3,
            y: 3,
            w: 2,
            h: 2,
        };
        // sx = sy = 1, frame is only 4×4 so the zone hangs over the edge by 1px.
        let scaled = scale_zone(&zone, 1.0, 1.0, 4, 4);
        assert_eq!(scaled.x, 3);
        assert_eq!(scaled.y, 3);
        assert_eq!(scaled.w, 1);
        assert_eq!(scaled.h, 1);
    }
}
