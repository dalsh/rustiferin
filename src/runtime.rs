//! Runtime-tunable knobs the tray (and other UI surfaces) can mutate while
//! the pipeline is running. Each knob is a small Clone-able handle backed by
//! an atomic; readers and writers share state without locks.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

/// Shared, lock-free handle for the live brightness-gain multiplier.
///
/// Stored as `f32::to_bits` inside an `AtomicU32` so the pipeline can `load()`
/// once per frame on the hot path without contending with the tray's `store()`.
#[derive(Clone, Debug)]
pub struct BrightnessGain {
    bits: Arc<AtomicU32>,
}

impl BrightnessGain {
    pub fn new(initial: f32) -> Self {
        Self {
            bits: Arc::new(AtomicU32::new(initial.to_bits())),
        }
    }

    pub fn load(&self) -> f32 {
        f32::from_bits(self.bits.load(Ordering::Relaxed))
    }

    pub fn store(&self, value: f32) {
        self.bits.store(value.to_bits(), Ordering::Relaxed);
    }
}

impl Default for BrightnessGain {
    fn default() -> Self {
        Self::new(1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brightness_gain_round_trips() {
        let h = BrightnessGain::new(1.0);
        assert_eq!(h.load(), 1.0);
        h.store(1.5);
        assert_eq!(h.load(), 1.5);
        h.store(0.8);
        assert_eq!(h.load(), 0.8);
    }

    #[test]
    fn brightness_gain_clone_shares_state() {
        let a = BrightnessGain::new(1.0);
        let b = a.clone();
        a.store(2.0);
        assert_eq!(b.load(), 2.0);
        b.store(0.5);
        assert_eq!(a.load(), 0.5);
    }

    #[test]
    fn brightness_gain_default_is_one() {
        let h = BrightnessGain::default();
        assert_eq!(h.load(), 1.0);
    }
}
