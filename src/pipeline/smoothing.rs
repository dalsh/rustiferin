//! Temporal smoothing of the LED stream via per-channel EMA.
//!
//! Smoothing happens *after* color correction. Correcting a smoothed pixel and smoothing
//! a corrected pixel produce equivalent results for deterministic corrections, and
//! smoothing last keeps the EMA state in the same color space the output sees.
//!
//! The blend factor is derived from a wall-clock time constant via
//! [`alpha_from_time_constant`] rather than being a fixed per-frame value, so the
//! settling speed is independent of the capture frame rate.

use crate::pipeline::LedColor;

/// Convert a smoothing time constant into a per-step EMA blend factor.
///
/// `alpha = 1 - exp(-dt/tau)`, the exact single-pole IIR response. Because the
/// exponential composes (`exp(-2x/tau) == exp(-x/tau)^2`), the amount of
/// smoothing over a given wall-clock span is the same regardless of how many
/// frames land in it. `tau <= 0` returns `1.0` (no smoothing); `dt == 0`
/// returns `0.0` (keep the previous value).
pub fn alpha_from_time_constant(dt_secs: f32, tau_secs: f32) -> f32 {
    if tau_secs <= 0.0 {
        return 1.0;
    }
    (1.0 - (-dt_secs / tau_secs).exp()).clamp(0.0, 1.0)
}

#[derive(Debug, Default, Clone)]
pub struct EmaState {
    last: Vec<LedColor>,
}

impl EmaState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Blend `current` toward the previous frame's value: `out = (1-α)·last + α·current`.
    /// `alpha = 1.0` is a pass-through; `alpha = 0.0` would freeze the output, so the
    /// caller is expected to validate the config range.
    ///
    /// On the first call (or when the LED count changes), the current frame is adopted
    /// verbatim with no smoothing applied: there is no prior state to blend against.
    pub fn step(&mut self, current: &mut [LedColor], alpha: f32) {
        if self.last.len() != current.len() {
            self.last = current.to_vec();
            return;
        }
        let a = alpha.clamp(0.0, 1.0);
        let one_minus = 1.0 - a;
        for (last, cur) in self.last.iter_mut().zip(current.iter_mut()) {
            let r = (last.r as f32 * one_minus + cur.r as f32 * a)
                .round()
                .clamp(0.0, 255.0) as u8;
            let g = (last.g as f32 * one_minus + cur.g as f32 * a)
                .round()
                .clamp(0.0, 255.0) as u8;
            let b = (last.b as f32 * one_minus + cur.b as f32 * a)
                .round()
                .clamp(0.0, 255.0) as u8;
            *cur = LedColor { r, g, b };
            *last = *cur;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_step_passes_through() {
        let mut ema = EmaState::default();
        let mut frame = vec![LedColor::new(10, 20, 30), LedColor::new(40, 50, 60)];
        ema.step(&mut frame, 0.5);
        assert_eq!(
            frame,
            vec![LedColor::new(10, 20, 30), LedColor::new(40, 50, 60)]
        );
    }

    #[test]
    fn alpha_one_is_pass_through_after_first_frame() {
        let mut ema = EmaState::default();
        let mut a = vec![LedColor::new(0, 0, 0)];
        ema.step(&mut a, 1.0);
        let mut b = vec![LedColor::new(100, 200, 50)];
        ema.step(&mut b, 1.0);
        assert_eq!(b[0], LedColor::new(100, 200, 50));
    }

    #[test]
    fn alpha_half_averages_consecutive_frames() {
        let mut ema = EmaState::default();
        let mut a = vec![LedColor::new(0, 0, 0)];
        ema.step(&mut a, 0.5);
        let mut b = vec![LedColor::new(100, 200, 50)];
        ema.step(&mut b, 0.5);
        // 0.5 * 0 + 0.5 * 100 = 50, etc.
        assert_eq!(b[0], LedColor::new(50, 100, 25));
    }

    #[test]
    fn alpha_from_time_constant_zero_tau_is_passthrough() {
        // A zero time constant means "no smoothing": adopt each frame verbatim.
        assert_eq!(alpha_from_time_constant(0.016, 0.0), 1.0);
        assert_eq!(alpha_from_time_constant(0.5, 0.0), 1.0);
    }

    #[test]
    fn alpha_from_time_constant_zero_dt_keeps_previous() {
        // No time elapsed -> nothing new blends in.
        assert_eq!(alpha_from_time_constant(0.0, 0.05), 0.0);
    }

    #[test]
    fn alpha_from_time_constant_large_dt_approaches_one() {
        // dt far beyond tau -> almost fully the new frame.
        assert!(alpha_from_time_constant(1.0, 0.05) > 0.999);
    }

    #[test]
    fn alpha_from_time_constant_is_frame_rate_independent() {
        // One step over 2 dt must retain the same fraction as two steps over 1 dt,
        // because exponential decay composes: exp(-2x/tau) == exp(-x/tau)^2.
        // This is the whole point: settling depends on wall-clock time, not fps.
        let tau = 0.05;
        let one_big = 1.0 - alpha_from_time_constant(0.02, tau);
        let two_small = (1.0 - alpha_from_time_constant(0.01, tau)).powi(2);
        assert!(
            (one_big - two_small).abs() < 1e-6,
            "retention diverged: {one_big} vs {two_small}"
        );
    }

    #[test]
    fn ema_state_resets_when_led_count_changes() {
        let mut ema = EmaState::default();
        let mut a = vec![LedColor::new(255, 255, 255); 4];
        ema.step(&mut a, 0.5);
        // Pretend the user added LEDs between frames.
        let mut b = vec![LedColor::new(0, 0, 0); 6];
        ema.step(&mut b, 0.5);
        assert_eq!(b, vec![LedColor::new(0, 0, 0); 6]);
    }
}
