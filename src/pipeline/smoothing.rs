//! Temporal smoothing of the LED stream via per-channel EMA.
//!
//! Smoothing happens *after* color correction. Correcting a smoothed pixel and smoothing
//! a corrected pixel produce equivalent results for deterministic corrections, and
//! smoothing last keeps the EMA state in the same color space the output sees, which
//! is what users tune `ema_alpha` against.

use crate::pipeline::LedColor;

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
