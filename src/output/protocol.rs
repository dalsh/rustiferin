//! Wire format for the Glow Worm Luciferin MQTT stream topic.
//!
//! `encode_stream` matches the firmware's non-JSON stream path
//! (`JSON_STREAM = false` in Firefly's `Constants.java`): one MQTT message per
//! frame, comma-separated decimal integers.

use crate::pipeline::LedColor;

/// Encode a full LED frame into the firmware's stream wire format.
///
/// Output shape: `{N},{brightness},{packed_rgb_1},...,{packed_rgb_N},0`
/// where `packed_rgb_i = (r << 16) | (g << 8) | b`.
///
/// Matches Firefly's non-JSON stream path with `JSON_STREAM = false`
/// (`firefly_luciferin/.../config/Constants.java:204`), which is the
/// firmware's default and produces one MQTT message per frame.
pub fn encode_stream(leds: &[LedColor], brightness: u8, scratch: &mut String) {
    scratch.clear();
    let mut buf = itoa::Buffer::new();
    scratch.push_str(buf.format(leds.len() as u32));
    scratch.push(',');
    scratch.push_str(buf.format(brightness));
    for led in leds {
        let packed = ((led.r as u32) << 16) | ((led.g as u32) << 8) | (led.b as u32);
        scratch.push(',');
        scratch.push_str(buf.format(packed));
    }
    scratch.push_str(",0");
}

/// Encode the rare-path "we're alive" state announcement that puts the firmware
/// into wireless-stream consumer mode.
///
/// The magic field is `effect = "GlowWormWifi"`
/// (`firefly_luciferin/.../config/Constants.java:206`): the firmware switches
/// to the wireless-stream consumer when it sees this value; any other effect
/// keeps the previous solid color and ignores `set/stream` traffic.
/// `ffeffect = "Bias light"` carries the visual effect name. `MAC`, when
/// present, scopes the announcement in multi-device setups.
pub fn encode_state_on(scratch: &mut String, mac: Option<&str>, brightness: u8) {
    scratch.clear();
    let mut buf = itoa::Buffer::new();
    scratch.push_str("{\"state\":\"ON\",\"effect\":\"GlowWormWifi\",\"ffeffect\":\"Bias light\"");
    if let Some(mac) = mac {
        scratch.push_str(",\"MAC\":\"");
        scratch.push_str(mac);
        scratch.push('"');
    }
    scratch.push_str(",\"brightness\":");
    scratch.push_str(buf.format(brightness));
    scratch.push('}');
}

/// Encode the "shutdown" state message that turns the strip off.
///
/// Matches Firefly's `CommonUtility.turnOffLEDs` payload: `state=OFF` clears
/// the firmware's `ledManager.stateOn`, `effect=solid` pulls it out of
/// `GlowWormWifi` stream-consumer mode, and `brightness=0` ensures the strip
/// renders dark even if the firmware processes the state without re-running
/// `setColor`. Published to the `set` topic (not `set/stream`) so the firmware
/// JSON callback path handles it; intended to be sent with `retain=true` so
/// the broker overwrites our prior retained `state=ON` announce.
pub fn encode_state_off(scratch: &mut String, mac: Option<&str>) {
    scratch.clear();
    scratch.push_str("{\"state\":\"OFF\",\"effect\":\"solid\"");
    if let Some(mac) = mac {
        scratch.push_str(",\"MAC\":\"");
        scratch.push_str(mac);
        scratch.push('"');
    }
    scratch.push_str(",\"brightness\":0}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn led(r: u8, g: u8, b: u8) -> LedColor {
        LedColor { r, g, b }
    }

    #[test]
    fn encode_stream_one_led_golden() {
        let mut s = String::new();
        encode_stream(&[led(10, 20, 30)], 255, &mut s);
        // 10<<16 | 20<<8 | 30 = 660510
        assert_eq!(s, "1,255,660510,0");
    }

    #[test]
    fn encode_stream_three_leds_golden() {
        let mut s = String::new();
        encode_stream(
            &[led(0, 0, 0), led(255, 255, 255), led(1, 2, 3)],
            128,
            &mut s,
        );
        // 0, 255<<16|255<<8|255 = 16777215, 1<<16|2<<8|3 = 66051
        assert_eq!(s, "3,128,0,16777215,66051,0");
    }

    #[test]
    fn encode_stream_zero_leds_golden() {
        let mut s = String::new();
        encode_stream(&[], 255, &mut s);
        assert_eq!(s, "0,255,0");
    }

    #[test]
    fn encode_stream_reuses_scratch_buffer() {
        let mut s = String::with_capacity(128);
        let cap_before = s.capacity();
        encode_stream(&[led(1, 2, 3)], 255, &mut s);
        encode_stream(&[led(4, 5, 6)], 255, &mut s);
        // 4<<16|5<<8|6 = 263430
        assert_eq!(s, "1,255,263430,0");
        assert_eq!(
            s.capacity(),
            cap_before,
            "encode_stream must not allocate on reuse"
        );
    }

    proptest! {
        #[test]
        fn encode_stream_well_formed_for_any_input(
            leds in proptest::collection::vec(
                (0u8..=255, 0u8..=255, 0u8..=255).prop_map(|(r, g, b)| LedColor { r, g, b }),
                0..510,
            ),
            brightness in 0u8..=255,
        ) {
            let mut s = String::new();
            encode_stream(&leds, brightness, &mut s);

            // First token is the LED count, second is brightness, last is "0".
            let parts: Vec<&str> = s.split(',').collect();
            prop_assert_eq!(parts.len(), leds.len() + 3, "expected N+3 comma-separated tokens");
            prop_assert_eq!(parts[0].parse::<u32>().unwrap(), leds.len() as u32);
            prop_assert_eq!(parts[1].parse::<u8>().unwrap(), brightness);
            prop_assert_eq!(*parts.last().unwrap(), "0");

            // Middle tokens round-trip back to LedColor.
            for (i, led) in leds.iter().enumerate() {
                let packed = parts[i + 2].parse::<u32>().unwrap();
                prop_assert_eq!(((packed >> 16) & 0xFF) as u8, led.r);
                prop_assert_eq!(((packed >>  8) & 0xFF) as u8, led.g);
                prop_assert_eq!(( packed        & 0xFF) as u8, led.b);
            }
        }
    }

    #[test]
    fn encode_state_on_without_mac_golden() {
        let mut s = String::new();
        encode_state_on(&mut s, None, 200);
        assert_eq!(
            s,
            r#"{"state":"ON","effect":"GlowWormWifi","ffeffect":"Bias light","brightness":200}"#
        );
    }

    #[test]
    fn encode_state_on_with_mac_golden() {
        let mut s = String::new();
        encode_state_on(&mut s, Some("AC:A7:04:BB:F4:9C"), 255);
        assert_eq!(
            s,
            r#"{"state":"ON","effect":"GlowWormWifi","ffeffect":"Bias light","MAC":"AC:A7:04:BB:F4:9C","brightness":255}"#
        );
    }

    #[test]
    fn encode_state_on_uses_glowworm_wifi_effect() {
        // Regression guard: the firmware enters stream-consumer mode only when
        // `effect` is the literal `GlowWormWifi` string; "Bias light" or "Solid"
        // leave it in its prior mode and the strip ignores `set/stream`.
        let mut s = String::new();
        encode_state_on(&mut s, None, 128);
        assert!(s.contains("\"effect\":\"GlowWormWifi\""));
    }

    #[test]
    fn encode_state_off_without_mac_golden() {
        let mut s = String::new();
        encode_state_off(&mut s, None);
        assert_eq!(s, r#"{"state":"OFF","effect":"solid","brightness":0}"#);
    }

    #[test]
    fn encode_state_off_with_mac_golden() {
        let mut s = String::new();
        encode_state_off(&mut s, Some("AC:A7:04:BB:F4:9C"));
        assert_eq!(
            s,
            r#"{"state":"OFF","effect":"solid","MAC":"AC:A7:04:BB:F4:9C","brightness":0}"#
        );
    }

    #[test]
    fn encode_state_off_uses_solid_effect() {
        // Regression guard: the firmware leaves GlowWormWifi stream-consumer
        // mode only when `effect` flips to a non-stream value. `solid` matches
        // Firefly's turnOffLEDs and is what the firmware's stream-stale
        // watchdog also falls back to.
        let mut s = String::new();
        encode_state_off(&mut s, None);
        assert!(s.contains("\"effect\":\"solid\""));
        assert!(s.contains("\"state\":\"OFF\""));
    }

    #[test]
    fn encode_state_on_reuses_scratch_buffer() {
        let mut s = String::with_capacity(256);
        let cap_before = s.capacity();
        encode_state_on(&mut s, Some("AA:BB:CC:DD:EE:FF"), 255);
        encode_state_on(&mut s, None, 128);
        assert!(s.contains("\"brightness\":128"));
        assert!(!s.contains("MAC"));
        assert_eq!(
            s.capacity(),
            cap_before,
            "encode_state_on must not allocate on reuse"
        );
    }
}
