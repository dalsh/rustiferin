//! Per-LED color correction: gamma, HSL offsets, white balance, night light, brightness limit.
//!
//! All functions are pure and operate in place. Conversions match Firefly Luciferin's
//! `ColorUtilities` so users porting profiles between the two see consistent results.

use crate::config::schema::HslOffsets;
use crate::pipeline::LedColor;

/// Precomputed `pow(channel/255, gamma) * 255` lookup. Built once at pipeline
/// start; reused for every channel of every LED of every frame.
#[derive(Clone)]
pub struct GammaLut {
    table: [u8; 256],
}

impl GammaLut {
    pub fn new(gamma: f32) -> Self {
        let mut table = [0u8; 256];
        let g = if gamma > 0.0 { gamma } else { 1.0 };
        for (i, slot) in table.iter_mut().enumerate() {
            let normalized = (i as f32) / 255.0;
            let out = (normalized.powf(g) * 255.0).round().clamp(0.0, 255.0);
            *slot = out as u8;
        }
        Self { table }
    }

    pub fn lookup(&self, byte: u8) -> u8 {
        self.table[byte as usize]
    }
}

/// Apply per-channel gamma correction in place.
pub fn gamma(leds: &mut [LedColor], lut: &GammaLut) {
    for led in leds.iter_mut() {
        led.r = lut.lookup(led.r);
        led.g = lut.lookup(led.g);
        led.b = lut.lookup(led.b);
    }
}

/// Convert each LED to HSL, add the offsets (hue wraps in `[0, 1)`, sat & lightness
/// clamp to `[0, 1]`), then convert back. Identity when all offsets are zero.
///
/// HSL math mirrors Firefly's `ColorUtilities::RGBtoHSL` / `HSLtoRGB` so saved profiles
/// produce the same result in both apps.
pub fn hsl_offset(leds: &mut [LedColor], offsets: &HslOffsets) {
    if offsets.h == 0.0 && offsets.s == 0.0 && offsets.l == 0.0 {
        return;
    }
    for led in leds.iter_mut() {
        let (mut h, mut s, mut l) = rgb_to_hsl(*led);
        h = (h + offsets.h).rem_euclid(1.0);
        s = (s + offsets.s).clamp(0.0, 1.0);
        l = (l + offsets.l).clamp(0.0, 1.0);
        *led = hsl_to_rgb(h, s, l);
    }
}

/// Apply Tanner Helland's color-temperature → RGB approximation as a per-channel scale.
/// 6500 K is treated as the neutral point: at exactly 6500 the function is the identity,
/// so users who never touch the setting see no transformation.
///
/// Reference: <https://tannerhelland.com/2012/09/18/convert-temperature-rgb-algorithm-code.html>
pub fn white_balance(leds: &mut [LedColor], kelvin: u32) {
    if kelvin == 6500 {
        return;
    }
    let (rs, gs, bs) = kelvin_to_rgb_factors(kelvin);
    for led in leds.iter_mut() {
        led.r = scale_channel(led.r, rs);
        led.g = scale_channel(led.g, gs);
        led.b = scale_channel(led.b, bs);
    }
}

/// Shift each LED toward "warm" by `strength` in `[0, 1]`. `0` is a no-op; `1` reduces
/// blue most and gently boosts red on bright colors.
pub fn night_light(leds: &mut [LedColor], strength: f32) {
    if strength <= 0.0 {
        return;
    }
    let s = strength.clamp(0.0, 1.0);
    let blue_reduction = 0.7 * s;
    let red_boost = (30.0 * s) as i32;
    let green_reduction = 0.3 * s;
    for led in leds.iter_mut() {
        let r0 = led.r as f32;
        let g0 = led.g as f32;
        let b0 = led.b as f32;
        let brightness = (r0 + g0 + b0) / (3.0 * 255.0);
        let warm_b = b0 * (1.0 - blue_reduction);
        let mut warm_r = r0;
        let mut warm_g = g0;
        if brightness > 0.5 {
            warm_r = (r0 + red_boost as f32).min(255.0);
            warm_g = (g0 * (1.0 - green_reduction)).max(0.0);
        }
        led.r = lerp(r0, warm_r, s).round().clamp(0.0, 255.0) as u8;
        led.g = lerp(g0, warm_g, s).round().clamp(0.0, 255.0) as u8;
        led.b = lerp(b0, warm_b, s).round().clamp(0.0, 255.0) as u8;
    }
}

/// Raise the HSB brightness of any LED below `floor` up to the floor, preserving
/// hue and saturation. `floor` is in `[0, 1]`; `0` is a no-op. Pure black inputs
/// have no hue to preserve and are returned as neutral grey at the floor level.
///
/// Mirrors Firefly Luciferin's `ImageProcessor.adjustLuminosityThreshold` (which
/// snaps `Color.RGBtoHSB`'s `B` component to a minimum). Applied **after** gamma
/// so it boosts the *darkened-by-gamma* output rather than the raw average.
pub fn luminosity_floor(leds: &mut [LedColor], floor: f32) {
    if floor <= 0.0 {
        return;
    }
    let floor = floor.clamp(0.0, 1.0);
    let floor_byte = (floor * 255.0).round() as u32;
    let floor_byte_u8 = floor_byte.min(255) as u8;
    for led in leds.iter_mut() {
        let max = led.r.max(led.g).max(led.b);
        if max == 0 {
            // No hue to preserve, go neutral.
            led.r = floor_byte_u8;
            led.g = floor_byte_u8;
            led.b = floor_byte_u8;
        } else if (max as u32) < floor_byte {
            // Scale uniformly so the max channel reaches the floor; clamp the
            // others against u8 overflow defensively (the scale factor preserves
            // ordering so only rounding can push us over).
            let scale = floor_byte as f32 / max as f32;
            led.r = ((led.r as f32 * scale).round() as u32).min(255) as u8;
            led.g = ((led.g as f32 * scale).round() as u32).min(255) as u8;
            led.b = ((led.b as f32 * scale).round() as u32).min(255) as u8;
        }
    }
}

/// Clamp each LED so `max(r, g, b) <= max_byte` by uniform per-LED scaling.
pub fn brightness_limit(leds: &mut [LedColor], max_byte: u8) {
    if max_byte == u8::MAX {
        return;
    }
    let max = max_byte as f32;
    for led in leds.iter_mut() {
        let peak = led.r.max(led.g).max(led.b) as f32;
        if peak <= max {
            continue;
        }
        let scale = max / peak;
        led.r = (led.r as f32 * scale).round().clamp(0.0, 255.0) as u8;
        led.g = (led.g as f32 * scale).round().clamp(0.0, 255.0) as u8;
        led.b = (led.b as f32 * scale).round().clamp(0.0, 255.0) as u8;
    }
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

fn scale_channel(c: u8, factor: f32) -> u8 {
    let scaled = (c as f32) * factor;
    scaled.round().clamp(0.0, 255.0) as u8
}

fn kelvin_to_rgb_factors(kelvin: u32) -> (f32, f32, f32) {
    let temp = (kelvin as f32) / 100.0;
    let r;
    let g;
    let b;
    if temp <= 66.0 {
        r = 255.0;
        g = (99.470_8 * temp.max(1.0).ln() - 161.119_57).clamp(0.0, 255.0);
        b = if temp <= 19.0 {
            0.0
        } else {
            (138.517_73 * (temp - 10.0).max(1.0).ln() - 305.044_8).clamp(0.0, 255.0)
        };
    } else {
        r = (329.698_73 * (temp - 60.0).powf(-0.133_204_76)).clamp(0.0, 255.0);
        g = (288.122_16 * (temp - 60.0).powf(-0.075_514_85)).clamp(0.0, 255.0);
        b = 255.0;
    }
    (r / 255.0, g / 255.0, b / 255.0)
}

pub fn rgb_to_hsl(c: LedColor) -> (f32, f32, f32) {
    let r = c.r as f32 / 255.0;
    let g = c.g as f32 / 255.0;
    let b = c.b as f32 / 255.0;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;
    let l = (max + min) / 2.0;
    if delta <= 0.01 {
        return (0.0, 0.0, l);
    }
    let s = if l < 0.5 {
        delta / (max + min)
    } else {
        delta / (2.0 - max - min)
    };
    let del_r = (((max - r) / 6.0) + (delta / 2.0)) / delta;
    let del_g = (((max - g) / 6.0) + (delta / 2.0)) / delta;
    let del_b = (((max - b) / 6.0) + (delta / 2.0)) / delta;
    let mut h = if (r - max).abs() < f32::EPSILON {
        del_b - del_g
    } else if (g - max).abs() < f32::EPSILON {
        (1.0 / 3.0) + del_r - del_b
    } else {
        (2.0 / 3.0) + del_g - del_r
    };
    if h < 0.0 {
        h += 1.0;
    }
    if h > 1.0 {
        h -= 1.0;
    }
    (h, s, l)
}

pub fn hsl_to_rgb(h: f32, s: f32, l: f32) -> LedColor {
    let h = h.clamp(0.0, 1.0);
    let s = s.clamp(0.0, 1.0);
    let l = l.clamp(0.0, 1.0);
    if s <= 0.01 {
        let v = (l * 255.0).round().clamp(0.0, 255.0) as u8;
        return LedColor { r: v, g: v, b: v };
    }
    let v2 = if l < 0.5 {
        l * (1.0 + s)
    } else {
        (l + s) - (s * l)
    };
    let v1 = 2.0 * l - v2;
    LedColor {
        r: (255.0 * hue_to_rgb(v1, v2, h + 1.0 / 3.0))
            .round()
            .clamp(0.0, 255.0) as u8,
        g: (255.0 * hue_to_rgb(v1, v2, h)).round().clamp(0.0, 255.0) as u8,
        b: (255.0 * hue_to_rgb(v1, v2, h - 1.0 / 3.0))
            .round()
            .clamp(0.0, 255.0) as u8,
    }
}

fn hue_to_rgb(v1: f32, v2: f32, mut vh: f32) -> f32 {
    if vh < 0.0 {
        vh += 1.0;
    }
    if vh > 1.0 {
        vh -= 1.0;
    }
    if 6.0 * vh < 1.0 {
        return v1 + (v2 - v1) * 6.0 * vh;
    }
    if 2.0 * vh < 1.0 {
        return v2;
    }
    if 3.0 * vh < 2.0 {
        return v1 + (v2 - v1) * ((2.0 / 3.0) - vh) * 6.0;
    }
    v1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gamma_identity_lut_is_identity() {
        let lut = GammaLut::new(1.0);
        let mut leds = vec![LedColor::new(0, 128, 255), LedColor::new(1, 200, 17)];
        gamma(&mut leds, &lut);
        assert_eq!(leds[0], LedColor::new(0, 128, 255));
        assert_eq!(leds[1], LedColor::new(1, 200, 17));
    }

    #[test]
    fn gamma_22_darkens_midtones_more_than_endpoints() {
        let lut = GammaLut::new(2.2);
        assert_eq!(lut.lookup(0), 0);
        assert_eq!(lut.lookup(255), 255);
        // midpoint: (0.5)^2.2 * 255 ≈ 55.5
        let mid = lut.lookup(128);
        assert!(
            (54..=58).contains(&mid),
            "midtone {mid} outside expected band"
        );
    }

    #[test]
    fn gamma_below_one_brightens() {
        let lut = GammaLut::new(0.5);
        // (0.5)^0.5 * 255 ≈ 180
        let mid = lut.lookup(128);
        assert!((178..=182).contains(&mid));
    }

    #[test]
    fn hsl_offsets_zero_is_identity() {
        let mut leds = vec![LedColor::new(10, 200, 75)];
        hsl_offset(&mut leds, &HslOffsets::default());
        assert_eq!(leds[0], LedColor::new(10, 200, 75));
    }

    #[test]
    fn hsl_hue_offset_one_sixth_shifts_red_toward_yellow() {
        let mut leds = vec![LedColor::new(255, 0, 0)];
        hsl_offset(
            &mut leds,
            &HslOffsets {
                h: 1.0 / 6.0,
                s: 0.0,
                l: 0.0,
            },
        );
        // 60° hue shift on pure red → yellow.
        assert_eq!(leds[0], LedColor::new(255, 255, 0));
    }

    #[test]
    fn hsl_lightness_positive_brightens() {
        let mut leds = vec![LedColor::new(100, 100, 100)];
        hsl_offset(
            &mut leds,
            &HslOffsets {
                h: 0.0,
                s: 0.0,
                l: 0.3,
            },
        );
        assert!(leds[0].r > 100);
    }

    #[test]
    fn rgb_hsl_round_trip_pure_red() {
        let (h, s, l) = rgb_to_hsl(LedColor::new(255, 0, 0));
        let back = hsl_to_rgb(h, s, l);
        assert_eq!(back, LedColor::new(255, 0, 0));
    }

    #[test]
    fn rgb_hsl_round_trip_grey_within_one_unit() {
        let c = LedColor::new(123, 123, 123);
        let (h, s, l) = rgb_to_hsl(c);
        let back = hsl_to_rgb(h, s, l);
        for (a, b) in [(back.r, c.r), (back.g, c.g), (back.b, c.b)] {
            assert!(a.abs_diff(b) <= 1, "channel diverged: got {a}, want {b}");
        }
    }

    #[test]
    fn kelvin_factors_at_6500_are_neutral() {
        let (r, g, b) = kelvin_to_rgb_factors(6500);
        let eps = 0.05;
        assert!((r - 1.0).abs() < eps, "r={r}");
        assert!((g - 1.0).abs() < eps, "g={g}");
        assert!((b - 1.0).abs() < eps, "b={b}");
    }

    #[test]
    fn white_balance_neutral_6500k_is_identity() {
        let mut leds = vec![LedColor::new(50, 100, 200)];
        white_balance(&mut leds, 6500);
        assert_eq!(leds[0], LedColor::new(50, 100, 200));
    }

    #[test]
    fn white_balance_warm_reduces_blue() {
        let mut leds = vec![LedColor::new(200, 200, 200)];
        white_balance(&mut leds, 3000);
        // Warm bias: red stays high, blue drops noticeably below the input.
        assert!(
            leds[0].b < 200,
            "expected blue reduction, got {}",
            leds[0].b
        );
        assert!(leds[0].r >= leds[0].b);
    }

    #[test]
    fn white_balance_cool_reduces_red() {
        let mut leds = vec![LedColor::new(200, 200, 200)];
        white_balance(&mut leds, 10000);
        assert!(leds[0].r < 200, "expected red reduction, got {}", leds[0].r);
        assert!(leds[0].b >= leds[0].r);
    }

    #[test]
    fn night_light_zero_is_identity() {
        let mut leds = vec![LedColor::new(10, 200, 75), LedColor::new(200, 200, 200)];
        let snapshot = leds.clone();
        night_light(&mut leds, 0.0);
        assert_eq!(leds, snapshot);
    }

    #[test]
    fn night_light_strength_one_reduces_blue() {
        let mut leds = vec![LedColor::new(200, 200, 200)];
        night_light(&mut leds, 1.0);
        assert!(leds[0].b < 100, "expected strong blue reduction");
    }

    #[test]
    fn night_light_is_monotonic_in_strength() {
        let mut light = vec![LedColor::new(200, 200, 200)];
        let mut heavy = vec![LedColor::new(200, 200, 200)];
        night_light(&mut light, 0.3);
        night_light(&mut heavy, 0.8);
        assert!(heavy[0].b <= light[0].b);
    }

    #[test]
    fn brightness_limit_255_is_identity() {
        let mut leds = vec![LedColor::new(10, 200, 75)];
        brightness_limit(&mut leds, 255);
        assert_eq!(leds[0], LedColor::new(10, 200, 75));
    }

    #[test]
    fn brightness_limit_halves_when_peak_exceeds_cap() {
        let mut leds = vec![LedColor::new(100, 200, 50)];
        brightness_limit(&mut leds, 100);
        // peak was 200, cap is 100 → scale 0.5
        assert_eq!(leds[0], LedColor::new(50, 100, 25));
    }

    #[test]
    fn brightness_limit_skips_dim_pixels() {
        let mut leds = vec![LedColor::new(40, 30, 20)];
        brightness_limit(&mut leds, 100);
        assert_eq!(leds[0], LedColor::new(40, 30, 20));
    }

    #[test]
    fn luminosity_floor_zero_is_identity() {
        let mut leds = vec![
            LedColor::new(0, 0, 0),
            LedColor::new(10, 5, 3),
            LedColor::new(255, 200, 150),
        ];
        let snapshot = leds.clone();
        luminosity_floor(&mut leds, 0.0);
        assert_eq!(leds, snapshot);
    }

    #[test]
    fn luminosity_floor_leaves_already_bright_pixels_alone() {
        // HSB brightness = max(r, g, b) / 255. (200,0,0) → 0.78 > 0.10 floor.
        let mut leds = vec![LedColor::new(200, 0, 0)];
        luminosity_floor(&mut leds, 0.10);
        assert_eq!(leds[0], LedColor::new(200, 0, 0));
    }

    #[test]
    fn luminosity_floor_boosts_dim_pixel_to_floor_preserving_hue() {
        // (10, 5, 0) has max=10 → HSB brightness 0.039. With floor 0.20 the max
        // channel must reach 51 (= 0.20 * 255). Scale factor 51/10 = 5.1 applied
        // uniformly to all channels: (10, 5, 0) → (51, 26, 0).
        let mut leds = vec![LedColor::new(10, 5, 0)];
        luminosity_floor(&mut leds, 0.20);
        let out = leds[0];
        assert_eq!(out.r, 51);
        assert_eq!(out.g, 26);
        assert_eq!(out.b, 0);
    }

    #[test]
    fn luminosity_floor_handles_pure_black_as_neutral_grey() {
        // (0,0,0) has no hue to preserve. Matches Firefly's
        // `Color.getHSBColor(0, 0, floor)`: pure grey at the floor brightness.
        let mut leds = vec![LedColor::new(0, 0, 0)];
        luminosity_floor(&mut leds, 0.20);
        // floor 0.20 → byte 51. All channels equal.
        assert_eq!(leds[0], LedColor::new(51, 51, 51));
    }

    #[test]
    fn luminosity_floor_clamps_at_one() {
        // floor > 1 should be capped at 1.0 silently rather than panicking.
        let mut leds = vec![LedColor::new(0, 0, 0)];
        luminosity_floor(&mut leds, 2.5);
        assert_eq!(leds[0], LedColor::new(255, 255, 255));
    }
}
