//! MQTT output task: subscribes to the pipeline's `LedFrame` watch channel,
//! encodes each new frame as a Glow Worm stream message, and publishes it on
//! `{topic_base}/set/stream`.
//!
//! Reconnection is delegated to `rumqttc::AsyncClient`; we drive its event
//! loop in a sibling task that updates [`OutputState`] for the tray.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context};
use rumqttc::{AsyncClient, ConnectionError, Event, EventLoop, MqttOptions, Outgoing, Packet, QoS};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::config::schema::Config;
use crate::pipeline::LedFrame;
use crate::shutdown::Shutdown;

use super::protocol;
use super::OutputState;

const KEEP_ALIVE: Duration = Duration::from_secs(15);
// Tighter backpressure: 2 slots is enough to mask the broker's per-message
// roundtrip on a healthy LAN, but small enough that any WiFi congestion
// stalls the publisher rather than queueing stale frames downstream.
const MAX_INFLIGHT: u16 = 2;
const OUTGOING_CHAN: usize = 32;
const RECONNECT_BACKOFF_INITIAL: Duration = Duration::from_secs(1);
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(30);
const STATE_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
// Upper bound for the driver to flush the final state-OFF publish + Disconnect
// packet over the network on shutdown. The publish itself is QoS 1 (one
// roundtrip to the broker); we don't wait for the PUBACK explicitly, but a
// clean Disconnect closes the eventloop once outgoing has drained.
const SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

/// Spawn the output task under the shared [`Shutdown`] set.
pub fn spawn(
    shutdown: &mut Shutdown,
    config: Arc<Config>,
    leds_in: watch::Receiver<LedFrame>,
    state_out: watch::Sender<OutputState>,
    metrics: crate::stats::Metrics,
) {
    let cancel = shutdown.token();
    shutdown.spawn("output", async move {
        run(config, leds_in, state_out, metrics, cancel).await
    });
}

/// Public entry point. Returns when `cancel` is fired or the pipeline drops
/// its `watch::Sender`.
pub async fn run(
    config: Arc<Config>,
    leds_in: watch::Receiver<LedFrame>,
    state_out: watch::Sender<OutputState>,
    metrics: crate::stats::Metrics,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    let span = tracing::info_span!("output");
    let _enter = span.enter();

    let mut scratch = String::with_capacity(8 * 1024);

    if cancel.is_cancelled() {
        return Ok(());
    }
    let _ = state_out.send_replace(OutputState::Connecting);
    let (options, _broker_host) = build_options(&config)
        .with_context(|| format!("invalid mqtt config: {}", config.mqtt.broker_url))?;
    let (client, eventloop) = AsyncClient::new(options, OUTGOING_CHAN);

    // Driver gets its own cancel token, kept alive past the inner-loop
    // cancellation so a final blackout publish has an eventloop to drain
    // through. We cancel it manually after the blackout on timeout.
    let driver_cancel = CancellationToken::new();
    let mut driver = tokio::spawn(drive_eventloop(
        eventloop,
        state_out.clone(),
        metrics.clone(),
        driver_cancel.clone(),
    ));

    // Announce presence on connect, with `retain=true` so the broker replays
    // it to the device on every (re)subscribe. Without retain, the device
    // misses the one-shot message whenever it isn't connected at the exact
    // moment we publish (transient wifi drops, broker restarts, race with
    // our own startup). `effect = "GlowWormWifi"` (set inside
    // `encode_state_on`) is what flips the device into wireless-stream
    // consumer mode.
    publish_state_on(&client, &config, &mut scratch).await;

    let outcome = run_inner(
        &client,
        &mut leds_in.clone(),
        &config,
        &mut scratch,
        &metrics,
        cancel.clone(),
    )
    .await;

    // On cancellation or pipeline shutdown, push a final state-OFF message
    // so the firmware paints the strip dark immediately instead of holding
    // the last colors until its own stream-stale timeout (~10s).
    //
    // The blackout must happen *before* we cancel the driver, `publish`
    // is an enqueue onto rumqttc's request channel, which is read by the
    // eventloop the driver is polling. We then ask the client to
    // disconnect (also queued, after the publish) and wait for the driver
    // to exit on its own, so the publish and disconnect packets actually
    // hit the wire. Only force-cancel on timeout.
    publish_blackout(&client, &config, &mut scratch).await;
    let _ = client.disconnect().await;
    if tokio::time::timeout(SHUTDOWN_DRAIN_TIMEOUT, &mut driver)
        .await
        .is_err()
    {
        tracing::warn!(
            timeout_ms = SHUTDOWN_DRAIN_TIMEOUT.as_millis() as u64,
            "mqtt driver did not drain blackout in time; forcing cancel"
        );
        driver_cancel.cancel();
        let _ = driver.await;
    }

    match outcome {
        ExitReason::Cancelled => Ok(()),
        ExitReason::PipelineGone => {
            tracing::info!("pipeline channel closed, output exiting");
            Ok(())
        }
    }
}

/// Publish the state-announce JSON to the `/set` topic with `retain=true`.
/// Used on every reconnect *and* on the heartbeat tick inside the inner loop.
async fn publish_state_on(client: &AsyncClient, cfg: &Config, scratch: &mut String) {
    protocol::encode_state_on(
        scratch,
        cfg.mqtt.device_mac.as_deref(),
        cfg.color.brightness_max,
    );
    let topic = state_topic(cfg);
    if let Err(err) = client
        .publish(&topic, QoS::AtMostOnce, true, scratch.as_bytes())
        .await
    {
        tracing::warn!(error = ?err, "state announcement publish failed");
    }
}

/// Publish the shutdown state-OFF message on the `set` topic.
///
/// Mirrors Firefly's `CommonUtility.turnOffLEDs`: a JSON `{state:OFF,
/// effect:solid, brightness:0}` on `{topic_base}/set`, QoS 1, retain=true.
/// `retain=true` overwrites our prior retained `state=ON` announce so the
/// firmware doesn't get pinned back ON the next time it (re)subscribes.
/// The `set/stream` path is unsuitable for shutdown: the firmware only
/// consumes stream frames while `effect == GlowWormWifi`, and a single black
/// frame leaves the firmware waiting up to ~10s for the stream-stale watchdog
/// before it actually goes solid+off.
async fn publish_blackout(client: &AsyncClient, cfg: &Config, scratch: &mut String) {
    protocol::encode_state_off(scratch, cfg.mqtt.device_mac.as_deref());
    let topic = state_topic(cfg);
    if let Err(err) = client
        .publish(&topic, QoS::AtLeastOnce, true, scratch.as_bytes())
        .await
    {
        tracing::warn!(error = ?err, "final blackout publish failed");
    }
}

enum ExitReason {
    Cancelled,
    PipelineGone,
}

/// Inner loop: publish frames until the pipeline disappears or we're cancelled.
async fn run_inner(
    client: &AsyncClient,
    leds_in: &mut watch::Receiver<LedFrame>,
    config: &Config,
    scratch: &mut String,
    metrics: &crate::stats::Metrics,
    cancel: CancellationToken,
) -> ExitReason {
    // Belt-and-suspenders alongside the retained state announce: re-publish
    // `state=ON, effect=GlowWormWifi` periodically so a device that briefly
    // dropped, or had its effect changed by another client, gets pinned back
    // without waiting for a full rustiferin restart.
    let mut state_tick = tokio::time::interval(STATE_HEARTBEAT_INTERVAL);
    state_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    state_tick.tick().await;

    // Cap the publish rate so we don't fill the broker -> device TCP send buffer
    // with frames the firmware can't drain. Glow Worm consumes ~20-30 fps at
    // ~110 LEDs; the screen capture happily delivers 60-144. Without this gate
    // the kernel buffers minutes of stale frames downstream.
    let mut gate = PublishGate::new(config.capture.target_fps, Instant::now());
    let mut permute_buf: Vec<crate::pipeline::LedColor> = Vec::new();

    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return ExitReason::Cancelled,
            res = leds_in.changed() => {
                if res.is_err() {
                    return ExitReason::PipelineGone;
                }
                let wait = gate.wait_from(Instant::now());
                if !wait.is_zero() {
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => return ExitReason::Cancelled,
                        _ = tokio::time::sleep(wait) => {}
                    }
                }
                // Newest-wins: any frames produced during the sleep are coalesced
                // into the watch slot; we grab whatever is current now.
                let frame = leds_in.borrow_and_update().clone();
                publish_frame(client, config, &frame, scratch, &mut permute_buf, metrics).await;
                gate.advance(Instant::now());
            }
            _ = state_tick.tick() => {
                publish_state_on(client, config, scratch).await;
            }
        }
    }
}

/// Rate-limit gate for MQTT publishes. Constructed once per connection from
/// `capture.target_fps`; advanced after each successful publish.
struct PublishGate {
    interval: Duration,
    next_allowed: Instant,
}

impl PublishGate {
    fn new(target_fps: u32, now: Instant) -> Self {
        Self {
            interval: publish_interval(target_fps),
            next_allowed: now,
        }
    }

    /// How long the caller should sleep before publishing the current frame.
    /// Returns `Duration::ZERO` when the gate is already open.
    fn wait_from(&self, now: Instant) -> Duration {
        self.next_allowed.saturating_duration_since(now)
    }

    /// Mark a publish as done at `now`; schedule the next allowed publish.
    /// Simple `now + interval` form: per-publish overhead naturally throttles
    /// the rate to whatever the broker->device round-trip yields.
    fn advance(&mut self, now: Instant) {
        self.next_allowed = now + self.interval;
    }
}

fn publish_interval(target_fps: u32) -> Duration {
    // A target of zero means "no throttling"; map to a zero-width interval so
    // the gate is a no-op rather than a divide-by-zero.
    if target_fps == 0 {
        return Duration::ZERO;
    }
    Duration::from_micros(1_000_000 / target_fps as u64)
}

async fn publish_frame(
    client: &AsyncClient,
    cfg: &Config,
    frame: &LedFrame,
    scratch: &mut String,
    permute_buf: &mut Vec<crate::pipeline::LedColor>,
    metrics: &crate::stats::Metrics,
) {
    let topic = stream_topic(cfg);
    let leds = permute_for_strip(
        &frame.colors,
        cfg.led_matrix.start_offset,
        cfg.led_matrix.reverse,
        permute_buf,
    );
    // Brightness is sourced from the color config; audio-driven brightness lives in a different epic.
    protocol::encode_stream(leds, cfg.color.brightness_max, scratch);
    // QoS 0: dropping a frame is invisible at 30+ fps; retransmitting a stale one is worse.
    match client
        .publish(&topic, QoS::AtMostOnce, false, scratch.as_bytes())
        .await
    {
        Ok(_) => {
            metrics
                .frames_published()
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        Err(err) => {
            tracing::warn!(error = ?err, "publish failed; dropping frame");
        }
    }
}

/// Permute the zone-order color array into the strip's electrical order.
///
/// Matches Firefly's `sendColors` transform exactly: reverse the matrix order
/// first (when the strip is wired clockwise relative to the zone authoring
/// direction), then rotate left by `start_offset` so the reversed-then-rotated
/// `out[0]` lands on the strip's electrical LED 0.
///
/// Returns a slice that's either `colors` itself (identity case, no permutation
/// needed) or a view into `scratch` after it's been filled with the permuted
/// sequence. `scratch` is reused across frames to avoid per-frame allocation.
fn permute_for_strip<'a>(
    colors: &'a [crate::pipeline::LedColor],
    start_offset: u32,
    reverse: bool,
    scratch: &'a mut Vec<crate::pipeline::LedColor>,
) -> &'a [crate::pipeline::LedColor] {
    let n = colors.len();
    let offset = if n == 0 {
        0
    } else {
        (start_offset as usize) % n
    };
    if !reverse && offset == 0 {
        return colors;
    }
    scratch.clear();
    scratch.reserve(n);
    if reverse {
        // Reversed-then-rotated: for output index k, source index is
        // (n - 1 - ((k + offset) mod n)).
        for k in 0..n {
            let src = n - 1 - ((k + offset) % n);
            scratch.push(colors[src]);
        }
    } else {
        scratch.extend_from_slice(&colors[offset..]);
        scratch.extend_from_slice(&colors[..offset]);
    }
    scratch.as_slice()
}

fn stream_topic(cfg: &Config) -> String {
    format!("{}/set/stream", cfg.mqtt.topic_base)
}

fn state_topic(cfg: &Config) -> String {
    format!("{}/set", cfg.mqtt.topic_base)
}

fn build_options(cfg: &Config) -> anyhow::Result<(MqttOptions, String)> {
    let url = url::Url::parse(&cfg.mqtt.broker_url)
        .with_context(|| format!("parsing broker_url `{}`", cfg.mqtt.broker_url))?;
    let host = url
        .host_str()
        .ok_or_else(|| anyhow!("broker_url missing host"))?
        .to_string();
    let port = url.port().unwrap_or(match url.scheme() {
        "mqtts" => 8883,
        _ => 1883,
    });
    let client_id = format!("{}-rustiferin", cfg.general.device_name);
    let mut opts = MqttOptions::new(client_id, host.clone(), port);
    opts.set_keep_alive(KEEP_ALIVE);
    opts.set_inflight(MAX_INFLIGHT);
    if let (Some(u), Some(p)) = (cfg.mqtt.username.as_ref(), cfg.mqtt.password.as_ref()) {
        opts.set_credentials(u, p);
    }
    Ok((opts, host))
}

/// Polls the rumqttc event loop, surfacing connection state transitions on
/// `state_out`. On connection error, applies an exponential backoff before
/// returning control to `eventloop.poll()` (which will reconnect internally).
async fn drive_eventloop(
    mut eventloop: EventLoop,
    state_out: watch::Sender<OutputState>,
    metrics: crate::stats::Metrics,
    cancel: CancellationToken,
) {
    let mut backoff = RECONNECT_BACKOFF_INITIAL;
    // The first ConnAck per process is the initial connect, not a reconnect.
    // Track previous state locally so we only increment on transitions from
    // an established connection back to Connected.
    let mut was_connected = false;
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return,
            event = eventloop.poll() => {
                match event {
                    Ok(Event::Incoming(Packet::ConnAck(_))) => {
                        if was_connected {
                            metrics
                                .mqtt_reconnects()
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                        was_connected = true;
                        tracing::info!("mqtt connected");
                        let _ = state_out.send_replace(OutputState::Connected);
                        backoff = RECONNECT_BACKOFF_INITIAL;
                    }
                    Ok(Event::Incoming(Packet::Disconnect)) => {
                        was_connected = false;
                        tracing::warn!("mqtt server disconnected");
                        let _ = state_out.send_replace(OutputState::Disconnected);
                    }
                    // We initiated a clean disconnect via `AsyncClient::disconnect`;
                    // exit instead of letting `eventloop.poll()` surface the
                    // server-side socket close as an error and trigger rumqttc's
                    // automatic reconnect. Without this branch, shutdown logs a
                    // spurious "connection closed by peer" warn followed by a
                    // reconnect, and the driver never drains on its own.
                    Ok(Event::Outgoing(Outgoing::Disconnect)) => {
                        tracing::debug!("mqtt clean disconnect sent");
                        let _ = state_out.send_replace(OutputState::Disconnected);
                        return;
                    }
                    Ok(_) => {}
                    Err(err) => {
                        was_connected = false;
                        let new_state = classify(&err);
                        tracing::warn!(error = ?err, "mqtt eventloop error; backing off");
                        let _ = state_out.send_replace(new_state);
                        tokio::select! {
                            _ = tokio::time::sleep(backoff) => {}
                            _ = cancel.cancelled() => return,
                        }
                        backoff = (backoff * 2).min(RECONNECT_BACKOFF_MAX);
                    }
                }
            }
        }
    }
}

fn classify(err: &ConnectionError) -> OutputState {
    match err {
        ConnectionError::MqttState(_)
        | ConnectionError::Tls(_)
        | ConnectionError::NotConnAck(_) => OutputState::Failed,
        _ => OutputState::Disconnected,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::{GeneralConfig, MqttConfig};

    fn cfg(broker: &str, device: &str, base: &str) -> Config {
        Config {
            general: GeneralConfig {
                device_name: device.into(),
                ..Default::default()
            },
            mqtt: MqttConfig {
                broker_url: broker.into(),
                topic_base: base.into(),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn build_options_parses_plain_mqtt_url() {
        let c = cfg(
            "mqtt://broker.local:1884",
            "strip-a",
            "lights/glowwormluciferin",
        );
        let (_opts, host) = build_options(&c).expect("ok");
        assert_eq!(host, "broker.local");
    }

    #[test]
    fn build_options_defaults_mqtts_port_to_8883() {
        let c = cfg("mqtts://example.com", "strip-a", "lights/glowwormluciferin");
        let (opts, host) = build_options(&c).expect("ok");
        assert_eq!(host, "example.com");
        assert_eq!(opts.broker_address().1, 8883);
    }

    #[test]
    fn build_options_defaults_mqtt_port_to_1883() {
        let c = cfg("mqtt://example.com", "strip-a", "lights/glowwormluciferin");
        let (opts, _) = build_options(&c).expect("ok");
        assert_eq!(opts.broker_address().1, 1883);
    }

    #[test]
    fn gate_returns_zero_when_already_past_due() {
        let now = Instant::now();
        let gate = PublishGate::new(30, now);
        assert_eq!(
            gate.wait_from(now + Duration::from_millis(100)),
            Duration::ZERO
        );
    }

    #[test]
    fn gate_returns_remaining_wait_when_not_yet_due() {
        let now = Instant::now();
        let mut gate = PublishGate::new(30, now); // 33ms interval
        gate.advance(now); // next_allowed = now + 33ms
        let elapsed = Duration::from_millis(10);
        let wait = gate.wait_from(now + elapsed);
        assert!(
            wait >= Duration::from_millis(20) && wait <= Duration::from_millis(24),
            "expected ~23ms remaining, got {wait:?}"
        );
    }

    #[test]
    fn gate_advance_pushes_next_allowed_forward_by_interval() {
        let now = Instant::now();
        let mut gate = PublishGate::new(30, now);
        gate.advance(now);
        let wait_immediately_after = gate.wait_from(now);
        // 30 fps = 33.3ms interval, so the wait should be ~33ms.
        assert!(
            wait_immediately_after >= Duration::from_millis(32)
                && wait_immediately_after <= Duration::from_millis(34),
            "expected ~33ms, got {wait_immediately_after:?}"
        );
    }

    #[test]
    fn gate_with_zero_target_fps_never_waits() {
        let now = Instant::now();
        let mut gate = PublishGate::new(0, now);
        gate.advance(now);
        assert_eq!(
            gate.wait_from(now + Duration::from_millis(1)),
            Duration::ZERO
        );
    }

    #[test]
    fn permute_offset_zero_no_reverse_is_identity() {
        use crate::pipeline::LedColor;
        let colors = vec![
            LedColor::new(1, 1, 1),
            LedColor::new(2, 2, 2),
            LedColor::new(3, 3, 3),
            LedColor::new(4, 4, 4),
        ];
        let mut scratch = Vec::new();
        let out = permute_for_strip(&colors, 0, false, &mut scratch);
        assert_eq!(out, colors.as_slice());
        assert!(
            scratch.is_empty(),
            "identity case must not touch scratch buffer"
        );
    }

    #[test]
    fn permute_offset_rotates_left() {
        use crate::pipeline::LedColor;
        let colors = vec![
            LedColor::new(1, 1, 1),
            LedColor::new(2, 2, 2),
            LedColor::new(3, 3, 3),
            LedColor::new(4, 4, 4),
        ];
        let mut scratch = Vec::new();
        let out = permute_for_strip(&colors, 1, false, &mut scratch);
        assert_eq!(
            out,
            [
                LedColor::new(2, 2, 2),
                LedColor::new(3, 3, 3),
                LedColor::new(4, 4, 4),
                LedColor::new(1, 1, 1),
            ]
        );
    }

    #[test]
    fn permute_reverse_then_rotate_matches_firefly_order() {
        // Mirrors `FireflyLuciferin.sendColors`: reverse first, then rotate left
        // by `start_offset`. Input [1,2,3,4] with offset=1, reverse=true yields
        // [4,3,2,1] after reverse -> [3,2,1,4] after left-rotate by 1.
        use crate::pipeline::LedColor;
        let colors = vec![
            LedColor::new(1, 1, 1),
            LedColor::new(2, 2, 2),
            LedColor::new(3, 3, 3),
            LedColor::new(4, 4, 4),
        ];
        let mut scratch = Vec::new();
        let out = permute_for_strip(&colors, 1, true, &mut scratch);
        assert_eq!(
            out,
            [
                LedColor::new(3, 3, 3),
                LedColor::new(2, 2, 2),
                LedColor::new(1, 1, 1),
                LedColor::new(4, 4, 4),
            ]
        );
    }

    #[test]
    fn permute_scratch_buffer_is_reused_across_calls() {
        use crate::pipeline::LedColor;
        let colors = vec![LedColor::new(1, 2, 3); 8];
        let mut scratch = Vec::with_capacity(8);
        let cap_before = scratch.capacity();
        let _ = permute_for_strip(&colors, 1, false, &mut scratch);
        let _ = permute_for_strip(&colors, 2, true, &mut scratch);
        assert_eq!(scratch.capacity(), cap_before);
    }

    #[test]
    fn default_topic_base_matches_stock_glow_worm() {
        let cfg = Config::default();
        assert_eq!(cfg.mqtt.topic_base, "lights/glowwormluciferin");
    }
}
