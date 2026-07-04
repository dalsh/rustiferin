use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub general: GeneralConfig,
    pub capture: CaptureConfig,
    pub led_matrix: LedMatrixConfig,
    pub color: ColorConfig,
    pub smoothing: SmoothingConfig,
    pub mqtt: MqttConfig,
    pub power: PowerConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GeneralConfig {
    pub device_name: String,
    /// Optional `tracing-subscriber` `EnvFilter` directive applied at startup.
    /// Examples: `"info"` (quiet daily-driver), `"info,rustiferin=debug"`
    /// (the binary's default), `"trace"` (verbose troubleshooting).
    /// `RUST_LOG` and `--log-level` both override this; if all three are
    /// unset the binary uses `"info,rustiferin=debug"`.
    pub log_level: Option<String>,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            device_name: "rustiferin".into(),
            log_level: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CaptureConfig {
    pub target_fps: u32,
    pub monitor_index: Option<u32>,
    /// Read every Nth pixel in both axes inside each zone. 1 = full quality,
    /// 4 = 16× less pixel work per frame, irrelevant for averaging because
    /// each zone still gets hundreds of samples at typical strip sizes.
    pub subsample: u32,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            target_fps: 30,
            monitor_index: None,
            subsample: 4,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LedZone {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LedMatrixConfig {
    pub reference_width: u32,
    pub reference_height: u32,
    pub zones: Vec<LedZone>,
    /// Rotate the published color array so `zones[start_offset]` lands on the
    /// strip's electrical LED 0. Mirrors Firefly's `ledStartOffset`.
    pub start_offset: u32,
    /// Reverse the published color array. Set when the physical LED strip
    /// runs counter to the order zones were authored in (e.g. the strip is
    /// wired counter-clockwise but zones are listed clockwise).
    pub reverse: bool,
}

impl Default for LedMatrixConfig {
    fn default() -> Self {
        Self {
            reference_width: 1920,
            reference_height: 1080,
            zones: vec![
                LedZone {
                    x: 0,
                    y: 0,
                    w: 960,
                    h: 540,
                },
                LedZone {
                    x: 960,
                    y: 0,
                    w: 960,
                    h: 540,
                },
                LedZone {
                    x: 960,
                    y: 540,
                    w: 960,
                    h: 540,
                },
                LedZone {
                    x: 0,
                    y: 540,
                    w: 960,
                    h: 540,
                },
            ],
            start_offset: 0,
            reverse: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct HslOffsets {
    pub h: f32,
    pub s: f32,
    pub l: f32,
}

/// How `pipeline::zones` collapses a zone's pixels to a single LED color.
///
/// `Mean` is the arithmetic linear-light average; `MeanSquared` is the
/// root-mean-square of the same linear values, which biases toward the bright
/// pixels in a zone (Hyperion's `multicolor_mean_squared`); `DominantAdv` is a
/// k-means clustering pass that picks the most-represented color. Mean and
/// MeanSquared are continuous (they slide as content moves); Dominant is
/// winner-take-all and snaps between clusters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum AveragingMode {
    #[default]
    Mean,
    MeanSquared,
    DominantAdv,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ColorConfig {
    pub gamma: f32,
    pub white_balance_kelvin: u32,
    pub night_light_strength: f32,
    pub brightness_max: u8,
    /// Linear multiplier applied to every channel before the brightness limit.
    /// `1.0` is the no-op default. Values > 1 brighten the strip; when the boost
    /// would push a channel past 255 the LED is scaled uniformly to keep hue.
    /// Validated range: `(0.0, 10.0]`.
    pub brightness_gain: f32,
    pub hsl_offsets: HslOffsets,
    /// Minimum HSB brightness floor in `[0, 1]`. Any LED that would otherwise
    /// fall below this after gamma is boosted up to it (preserving hue/sat).
    /// Default `0.0` disables. Mirrors Firefly Luciferin's `luminosityThreshold`
    /// (configured there in percent, `5` in Firefly = `0.05` here).
    pub luminosity_floor: f32,
    /// Per-zone pixel-to-LED reduction. See [`AveragingMode`].
    pub averaging: AveragingMode,
}

impl Default for ColorConfig {
    fn default() -> Self {
        Self {
            gamma: 2.2,
            white_balance_kelvin: 6500,
            night_light_strength: 0.0,
            brightness_max: 255,
            brightness_gain: 1.0,
            hsl_offsets: HslOffsets::default(),
            luminosity_floor: 0.0,
            averaging: AveragingMode::default(),
        }
    }
}

/// Temporal smoothing configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SmoothingConfig {
    /// Exponential-smoothing time constant in milliseconds: the wall-clock time
    /// for the strip to close ~63% of the gap to a new color. Frame-rate
    /// independent (unlike a per-frame alpha). `0.0` disables smoothing.
    pub time_constant_ms: f32,
}

impl Default for SmoothingConfig {
    fn default() -> Self {
        Self {
            time_constant_ms: 50.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MqttConfig {
    pub broker_url: String,
    pub username: Option<String>,
    pub password: Option<String>,
    /// MQTT topic root the firmware listens on. Default matches stock Glow Worm
    /// Luciferin. The agent publishes to `{topic_base}/set` (state) and
    /// `{topic_base}/set/stream` (per-frame stream).
    pub topic_base: String,
    /// Glow Worm device MAC, e.g. `AC:A7:04:BB:F4:9C`. When set, the state
    /// announcement includes a `MAC` field so the firmware can scope the
    /// command in multi-device setups. Stock single-device firmware tolerates
    /// its absence.
    pub device_mac: Option<String>,
}

impl Default for MqttConfig {
    fn default() -> Self {
        Self {
            broker_url: "mqtt://127.0.0.1:1883".into(),
            username: None,
            password: None,
            topic_base: "lights/glowwormluciferin".into(),
            device_mac: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PowerConfig {
    pub idle_pause_after_secs: Option<u64>,
    pub respect_screensaver: bool,
}

impl Default for PowerConfig {
    fn default() -> Self {
        Self {
            idle_pause_after_secs: None,
            respect_screensaver: true,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("led_matrix.zones must be non-empty")]
    EmptyZones,
    #[error("led_matrix.zones[{index}] ({x},{y},{w}x{h}) is outside reference frame {rw}x{rh}")]
    ZoneOutOfBounds {
        index: usize,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
        rw: u32,
        rh: u32,
    },
    #[error("color.gamma must be in (0.0, 5.0], got {0}")]
    InvalidGamma(f32),
    #[error("smoothing.time_constant_ms must be in [0.0, 10000.0], got {0}")]
    InvalidTimeConstant(f32),
    #[error("color.night_light_strength must be in [0.0, 1.0], got {0}")]
    InvalidNightLightStrength(f32),
    #[error("color.luminosity_floor must be in [0.0, 1.0], got {0}")]
    InvalidLuminosityFloor(f32),
    #[error("color.brightness_gain must be in (0.0, 10.0], got {0}")]
    InvalidBrightnessGain(f32),
    #[error("mqtt.broker_url is not a valid URL: {0}")]
    InvalidBrokerUrl(String),
    #[error("mqtt.broker_url scheme must be `mqtt` or `mqtts`, got `{0}`")]
    InvalidBrokerScheme(String),
    #[error("mqtt.broker_url must include a host")]
    MissingBrokerHost,
    #[error("led_matrix.zones must contain at most {max} entries, got {count}")]
    TooManyZones { count: usize, max: usize },
}

impl Config {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.led_matrix.zones.is_empty() {
            return Err(ConfigError::EmptyZones);
        }
        // Stock Glow Worm firmware sizes its receive buffer for MAX_CHUNK = 510 LEDs
        // in the non-JSON stream path (Firefly's Constants.java:273). We emit one
        // message per frame, so cap the LED count here rather than chunking.
        if self.led_matrix.zones.len() > 510 {
            return Err(ConfigError::TooManyZones {
                count: self.led_matrix.zones.len(),
                max: 510,
            });
        }
        let rw = self.led_matrix.reference_width;
        let rh = self.led_matrix.reference_height;
        for (index, z) in self.led_matrix.zones.iter().enumerate() {
            let x_end = z.x.checked_add(z.w);
            let y_end = z.y.checked_add(z.h);
            let in_bounds = z.w > 0
                && z.h > 0
                && x_end.is_some_and(|v| v <= rw)
                && y_end.is_some_and(|v| v <= rh);
            if !in_bounds {
                return Err(ConfigError::ZoneOutOfBounds {
                    index,
                    x: z.x,
                    y: z.y,
                    w: z.w,
                    h: z.h,
                    rw,
                    rh,
                });
            }
        }
        if !(self.color.gamma > 0.0 && self.color.gamma <= 5.0) {
            return Err(ConfigError::InvalidGamma(self.color.gamma));
        }
        let tc = self.smoothing.time_constant_ms;
        if !(tc.is_finite() && (0.0..=10_000.0).contains(&tc)) {
            return Err(ConfigError::InvalidTimeConstant(tc));
        }
        let nls = self.color.night_light_strength;
        if !(0.0..=1.0).contains(&nls) {
            return Err(ConfigError::InvalidNightLightStrength(nls));
        }
        let lf = self.color.luminosity_floor;
        if !(0.0..=1.0).contains(&lf) {
            return Err(ConfigError::InvalidLuminosityFloor(lf));
        }
        let bg = self.color.brightness_gain;
        if !(bg > 0.0 && bg <= 10.0) {
            return Err(ConfigError::InvalidBrightnessGain(bg));
        }
        let url = url::Url::parse(&self.mqtt.broker_url)
            .map_err(|e| ConfigError::InvalidBrokerUrl(e.to_string()))?;
        if !matches!(url.scheme(), "mqtt" | "mqtts") {
            return Err(ConfigError::InvalidBrokerScheme(url.scheme().to_string()));
        }
        if url.host().is_none() {
            return Err(ConfigError::MissingBrokerHost);
        }
        Ok(())
    }
}
