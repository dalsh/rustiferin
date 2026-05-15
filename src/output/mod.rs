//! Output: encode `LedFrame`s as Glow Worm Luciferin stream payloads and
//! publish them over MQTT. Connection state is published on a watch channel
//! for the tray.

pub mod protocol;

#[cfg(any(test, feature = "test-fakes"))]
pub mod fake;

/// Connection state, published on a watch channel for tray / stats consumers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputState {
    Connecting,
    Connected,
    Disconnected,
    Failed,
}

#[cfg(feature = "mqtt")]
mod mqtt_impl;
#[cfg(feature = "mqtt")]
pub use mqtt_impl::{run, spawn};
