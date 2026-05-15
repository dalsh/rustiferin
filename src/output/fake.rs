//! Test consumer that records every distinct [`LedFrame`] received on the
//! pipeline's `watch::Receiver<LedFrame>`. Mirrors the protocol shape of the
//! real MQTT output without doing any I/O.

use std::sync::{Arc, Mutex};

use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::pipeline::LedFrame;

#[derive(Clone, Default)]
pub struct FakeOutput {
    frames: Arc<Mutex<Vec<LedFrame>>>,
}

impl FakeOutput {
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot of every frame observed so far, in publish order.
    pub fn frames(&self) -> Vec<LedFrame> {
        self.frames.lock().expect("fake output poisoned").clone()
    }

    pub async fn run(self, mut leds_in: watch::Receiver<LedFrame>, cancel: CancellationToken) {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                changed = leds_in.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    let frame = leds_in.borrow_and_update().clone();
                    self.frames.lock().expect("fake output poisoned").push(frame);
                }
            }
        }
    }
}
