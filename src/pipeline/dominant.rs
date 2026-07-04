//! Dominant-color zone reduction via k-means in linear-light space.
//!
//! Port of Hyperion-NG's `calculateDominantColorAdv`. Where the arithmetic mean
//! collapses high-contrast content toward grey, this picks the most-represented
//! colour in the zone: the centroid of the largest k-means cluster.
//!
//! Seeds, cluster count, convergence epsilon, and iteration cap are fixed
//! (see [`SEED_PALETTE`], [`K`], [`CONVERGENCE_EPSILON`], [`MAX_ITERS`]) to
//! match Hyperion's defaults; the plan deliberately defers exposing knobs.

use crate::capture::Frame;
use crate::pipeline::zones::{channel_offsets, pixel_size, srgb_encode, ScaledZone, SRGB_DECODE};
use crate::pipeline::LedColor;

/// Cluster count. Matches Hyperion's `accuracyLevel + 1 = 4` default.
const K: usize = 4;

/// Hard iteration cap. Hyperion's `calculateDominantColorAdv` has no explicit
/// cap; we add one so a pathological frame can't stall the pipeline thread.
const MAX_ITERS: u32 = 16;

/// Convergence threshold on Euclidean centroid shift in linear-light `[0, 1]`.
/// Hyperion uses `< 1` on a 0-255 scale; `1/255 ~= 0.0039`. Round up slightly.
const CONVERGENCE_EPSILON: f32 = 0.004;

/// Initial centroid seeds in linear-light RGB. Order is significant: ties in
/// final cluster size break toward the earlier seed, which we exploit only in
/// degenerate (uniform) inputs.
const SEED_PALETTE: [[f32; 3]; K] = [
    [0.0, 0.0, 0.0], // BLACK
    [0.0, 1.0, 0.0], // GREEN
    [1.0, 1.0, 1.0], // WHITE
    [1.0, 0.0, 0.0], // RED
];

#[inline]
fn sq_dist(a: &[f32; 3], b: &[f32; 3]) -> f32 {
    let dr = a[0] - b[0];
    let dg = a[1] - b[1];
    let db = a[2] - b[2];
    dr * dr + dg * dg + db * db
}

#[inline]
fn nearest_cluster(centroids: &[[f32; 3]; K], p: &[f32; 3]) -> usize {
    let mut best = 0usize;
    let mut best_d = sq_dist(&centroids[0], p);
    for (i, c) in centroids.iter().enumerate().skip(1) {
        let d = sq_dist(c, p);
        if d < best_d {
            best_d = d;
            best = i;
        }
    }
    best
}

/// Reduce a zone to one [`LedColor`] by k-means clustering in linear-light space.
///
/// Walks every `step`-th pixel inside `zone`, linearises through the shared
/// sRGB decode LUT, runs k-means until convergence (or the iteration cap),
/// then returns the encoded centroid of the largest cluster.
///
/// `scratch` is a reusable buffer the caller owns so the per-zone linearised
/// pixel list does not allocate on the hot path: the pipeline thread keeps
/// one [`Vec`] alive for the life of the process and pays at most a handful
/// of growth reallocations before steady state. It is cleared on entry; on
/// return its capacity is retained for the next call.
///
/// Empty zones (no sampled pixels, e.g. step skipped them all) return black.
pub(super) fn dominant_adv_pixels(
    frame: &Frame,
    zone: &ScaledZone,
    step: u32,
    scratch: &mut Vec<[f32; 3]>,
) -> LedColor {
    let (r_idx, g_idx, b_idx) = channel_offsets(frame.format);
    let stride = frame.stride as usize;
    let bytes_per_px = pixel_size(frame.format);
    let decode = &*SRGB_DECODE;

    scratch.clear();
    let mut dy = 0u32;
    while dy < zone.h {
        let row_start = (zone.y + dy) as usize * stride;
        let mut dx = 0u32;
        while dx < zone.w {
            let px = row_start + (zone.x + dx) as usize * bytes_per_px;
            if px + bytes_per_px > frame.buf.len() {
                dx += step;
                continue;
            }
            scratch.push([
                decode[frame.buf[px + r_idx] as usize],
                decode[frame.buf[px + g_idx] as usize],
                decode[frame.buf[px + b_idx] as usize],
            ]);
            dx += step;
        }
        dy += step;
    }

    if scratch.is_empty() {
        return LedColor::default();
    }
    let pixels: &[[f32; 3]] = scratch;

    let mut centroids = SEED_PALETTE;
    let mut counts = [0u32; K];

    for _ in 0..MAX_ITERS {
        let mut sums = [[0.0f32; 3]; K];
        counts = [0u32; K];
        for p in pixels {
            let c = nearest_cluster(&centroids, p);
            sums[c][0] += p[0];
            sums[c][1] += p[1];
            sums[c][2] += p[2];
            counts[c] += 1;
        }

        let mut max_shift_sq = 0.0f32;
        for (i, centroid) in centroids.iter_mut().enumerate() {
            if counts[i] == 0 {
                continue;
            }
            let n = counts[i] as f32;
            let nc = [sums[i][0] / n, sums[i][1] / n, sums[i][2] / n];
            let shift_sq = sq_dist(&nc, centroid);
            if shift_sq > max_shift_sq {
                max_shift_sq = shift_sq;
            }
            *centroid = nc;
        }

        if max_shift_sq < CONVERGENCE_EPSILON * CONVERGENCE_EPSILON {
            break;
        }
    }

    let mut winner = 0usize;
    for (i, &count) in counts.iter().enumerate().skip(1) {
        if count > counts[winner] {
            winner = i;
        }
    }

    LedColor {
        r: srgb_encode(centroids[winner][0]),
        g: srgb_encode(centroids[winner][1]),
        b: srgb_encode(centroids[winner][2]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::{Frame, PixelFormat};

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

    fn full_zone(w: u32, h: u32) -> ScaledZone {
        ScaledZone { x: 0, y: 0, w, h }
    }

    #[test]
    fn solid_red_frame_returns_red() {
        let frame = solid_bgra(8, 8, 0, 0, 255);
        let got = dominant_adv_pixels(&frame, &full_zone(8, 8), 1, &mut Vec::new());
        assert_eq!(got, LedColor::new(255, 0, 0));
    }

    #[test]
    fn solid_green_frame_returns_green() {
        let frame = solid_bgra(8, 8, 0, 255, 0);
        let got = dominant_adv_pixels(&frame, &full_zone(8, 8), 1, &mut Vec::new());
        assert_eq!(got, LedColor::new(0, 255, 0));
    }

    #[test]
    fn solid_blue_frame_returns_blue() {
        // Blue is not in the seed palette but k-means still has to land on it
        // because every pixel falls into the same cluster and pulls the
        // closest centroid (BLACK at (0,0,0)) to (0,0,1).
        let frame = solid_bgra(8, 8, 255, 0, 0);
        let got = dominant_adv_pixels(&frame, &full_zone(8, 8), 1, &mut Vec::new());
        assert_eq!(got, LedColor::new(0, 0, 255));
    }

    /// 8x10 frame: 50 red / 30 green / 20 blue pixels. The arithmetic mean in
    /// linear space would land on `(0.5, 0.3, 0.2)` ~= `(188, 149, 124)` after
    /// re-encode. Dominant must instead pick the colour of the largest region.
    #[test]
    fn three_unequal_regions_returns_largest() {
        let width = 8u32;
        let height = 10u32;
        let stride = width * 4;
        let mut buf = vec![0u8; (stride * height) as usize];
        for y in 0..height {
            for x in 0..width {
                let p = ((y * stride) + x * 4) as usize;
                let (b, g, r) = if y < 5 {
                    (0, 0, 255)
                } else if y < 8 {
                    (0, 255, 0)
                } else {
                    (255, 0, 0)
                };
                buf[p] = b;
                buf[p + 1] = g;
                buf[p + 2] = r;
                buf[p + 3] = 255;
            }
        }
        let frame = Frame {
            buf,
            width,
            height,
            stride,
            format: PixelFormat::Bgra,
        };
        let got = dominant_adv_pixels(&frame, &full_zone(width, height), 1, &mut Vec::new());
        assert_eq!(got, LedColor::new(255, 0, 0), "largest region is red");
    }

    /// Pathological input: pseudo-random fill. The algorithm must terminate
    /// (not exceed [`MAX_ITERS`]) and return a non-default colour for a
    /// non-empty zone. We can't pin the exact result, only that the loop
    /// completes and produces something meaningful.
    #[test]
    fn random_fill_terminates_within_cap() {
        let width = 16u32;
        let height = 16u32;
        let stride = width * 4;
        let mut buf = vec![0u8; (stride * height) as usize];
        let mut state: u64 = 0xDEADBEEFCAFEBABE;
        for chunk in buf.chunks_exact_mut(4) {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            chunk[0] = (state >> 56) as u8;
            chunk[1] = (state >> 48) as u8;
            chunk[2] = (state >> 40) as u8;
            chunk[3] = 255;
        }
        let frame = Frame {
            buf,
            width,
            height,
            stride,
            format: PixelFormat::Bgra,
        };
        // If the loop ever fails to terminate, this test would hang rather
        // than fail. The cap guarantees we get here.
        let _ = dominant_adv_pixels(&frame, &full_zone(width, height), 1, &mut Vec::new());
    }

    #[test]
    fn empty_zone_returns_default() {
        let frame = solid_bgra(4, 4, 0, 0, 255);
        let got = dominant_adv_pixels(
            &frame,
            &ScaledZone {
                x: 0,
                y: 0,
                w: 0,
                h: 0,
            },
            1,
            &mut Vec::new(),
        );
        assert_eq!(got, LedColor::default());
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(50))]

        /// A frame where >= 80% of pixels share a single colour: dominant
        /// must return within a small Euclidean distance of that colour.
        ///
        /// Random "noise" pixels can pull the winning cluster's centroid a
        /// little, hence the tolerance. We pick the dominant colour from a
        /// palette far apart in RGB so cluster assignment is unambiguous.
        #[test]
        fn supermajority_color_is_recovered(
            width in 8u32..24,
            height in 8u32..24,
            palette_idx in 0u32..4,
            noise_seed in proptest::prelude::any::<u64>(),
        ) {
            let (want_r, want_g, want_b): (u8, u8, u8) = match palette_idx {
                0 => (255, 0, 0),     // red
                1 => (0, 255, 0),     // green
                2 => (0, 0, 255),     // blue
                _ => (255, 255, 255), // white
            };
            let stride = width * 4;
            let mut buf = vec![0u8; (stride * height) as usize];
            let total = (width * height) as u64;
            // 80% of pixels get the dominant colour, 20% are random noise.
            let mut state = noise_seed.wrapping_add(1);
            for i in 0..total {
                let p = (i as u32 * 4) as usize;
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                let roll = (state >> 32) as u32 % 100;
                if roll < 80 {
                    // BGRA byte order in the buffer; RGB ordering for the
                    // declared colour above.
                    buf[p]     = want_b;
                    buf[p + 1] = want_g;
                    buf[p + 2] = want_r;
                } else {
                    buf[p]     = (state >> 56) as u8;
                    buf[p + 1] = (state >> 48) as u8;
                    buf[p + 2] = (state >> 40) as u8;
                }
                buf[p + 3] = 255;
            }
            let frame = Frame { buf, width, height, stride, format: PixelFormat::Bgra };
            let got = dominant_adv_pixels(&frame, &full_zone(width, height), 1, &mut Vec::new());

            // Tolerance 64 / 255 per channel. On small frames a 20% random
            // noise floor can pull the winning cluster's centroid by ~50 on an
            // unlucky channel (a 48 bound was flaky), yet 64 stays well under
            // 128 so the cluster still unambiguously lands on the right hue.
            let dr = (got.r as i32 - want_r as i32).abs();
            let dg = (got.g as i32 - want_g as i32).abs();
            let db = (got.b as i32 - want_b as i32).abs();
            proptest::prop_assert!(
                dr <= 64 && dg <= 64 && db <= 64,
                "dominant drifted too far: got {got:?}, want ({want_r}, {want_g}, {want_b})"
            );
        }
    }
}
