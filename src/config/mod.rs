pub mod schema;

pub use schema::{
    AveragingMode, CaptureConfig, ColorConfig, Config, ConfigError, GeneralConfig, HslOffsets,
    LedMatrixConfig, LedZone, MqttConfig, PowerConfig, SmoothingConfig,
};

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

const DEFAULT_HEADER: &str = "\
# Rustiferin configuration.
# Missing fields fall back to defaults; see README for the full schema.
# LED order = order of `led_matrix.zones`; reorder this array to remap.
";

pub fn default_path() -> Result<PathBuf> {
    resolve_default_path(
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("HOME"),
    )
}

fn resolve_default_path(xdg: Option<OsString>, home: Option<OsString>) -> Result<PathBuf> {
    let base = xdg
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            home.filter(|v| !v.is_empty())
                .map(|h| PathBuf::from(h).join(".config"))
        })
        .ok_or_else(|| {
            anyhow!("neither XDG_CONFIG_HOME nor HOME is set; pass --config explicitly")
        })?;
    Ok(base.join("rustiferin").join("config.yaml"))
}

pub fn load(path: &Path) -> Result<Config> {
    if !path.exists() {
        write_default(path)
            .with_context(|| format!("writing default config to {}", path.display()))?;
        let cfg = Config::default();
        cfg.validate().context("validating default config")?;
        return Ok(cfg);
    }
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading config from {}", path.display()))?;
    let cfg: Config = serde_yaml_ng::from_str(&text)
        .with_context(|| format!("parsing config from {}", path.display()))?;
    cfg.validate()
        .with_context(|| format!("validating config from {}", path.display()))?;
    Ok(cfg)
}

/// Update only `color.brightness_gain` on disk. Re-serialises the whole
/// config (so any hand-written comments in the YAML are lost). Validates the
/// new value before writing so an invalid number cannot corrupt the file.
pub fn update_brightness_gain(path: &Path, value: f32) -> Result<()> {
    let mut cfg = load(path).context("loading config before brightness_gain update")?;
    cfg.color.brightness_gain = value;
    cfg.validate()
        .with_context(|| format!("brightness_gain={value} failed validation"))?;
    let yaml = serde_yaml_ng::to_string(&cfg).context("serializing config")?;
    std::fs::write(path, yaml).with_context(|| format!("writing config to {}", path.display()))?;
    Ok(())
}

pub fn write_default(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating config directory {}", parent.display()))?;
        }
    }
    let cfg = Config::default();
    let yaml = serde_yaml_ng::to_string(&cfg).context("serializing default config")?;
    let contents = format!("{DEFAULT_HEADER}{yaml}");
    std::fs::write(path, contents)
        .with_context(|| format!("writing default config to {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn round_trip_preserves_sample_config() {
        let mut cfg = Config::default();
        cfg.general.device_name = "kitchen".into();
        cfg.capture.target_fps = 60;
        cfg.capture.monitor_index = Some(1);
        cfg.color.gamma = 2.4;
        cfg.color.white_balance_kelvin = 5500;
        cfg.color.brightness_max = 200;
        cfg.color.night_light_strength = 0.3;
        cfg.smoothing.ema_alpha = 0.25;
        cfg.mqtt.broker_url = "mqtts://broker.local:8883".into();
        cfg.mqtt.username = Some("alice".into());
        cfg.mqtt.password = Some("secret".into());
        cfg.mqtt.topic_base = "homes/k".into();
        cfg.power.idle_pause_after_secs = Some(120);
        cfg.power.respect_screensaver = false;
        cfg.led_matrix.zones = vec![LedZone {
            x: 10,
            y: 20,
            w: 30,
            h: 40,
        }];

        let yaml = serde_yaml_ng::to_string(&cfg).expect("serialize");
        let parsed: Config = serde_yaml_ng::from_str(&yaml).expect("deserialize");
        assert_eq!(cfg, parsed);
    }

    #[test]
    fn partial_yaml_fills_defaults() {
        let yaml = "mqtt:\n  broker_url: \"mqtt://10.0.0.1:1883\"\n";
        let parsed: Config = serde_yaml_ng::from_str(yaml).expect("deserialize");
        assert_eq!(parsed.mqtt.broker_url, "mqtt://10.0.0.1:1883");
        assert_eq!(parsed.capture.target_fps, 30);
        assert_eq!(parsed.color.gamma, 2.2);
    }

    #[test]
    fn validate_accepts_default() {
        Config::default().validate().expect("default is valid");
    }

    #[test]
    fn validate_rejects_out_of_bounds_zone() {
        let mut cfg = Config::default();
        cfg.led_matrix.reference_width = 100;
        cfg.led_matrix.reference_height = 100;
        cfg.led_matrix.zones = vec![LedZone {
            x: 80,
            y: 0,
            w: 40,
            h: 10,
        }];
        let err = cfg.validate().expect_err("out-of-bounds zone");
        assert!(matches!(err, ConfigError::ZoneOutOfBounds { .. }));
    }

    #[test]
    fn validate_rejects_empty_zones() {
        let mut cfg = Config::default();
        cfg.led_matrix.zones = vec![];
        let err = cfg.validate().expect_err("empty zones");
        assert!(matches!(err, ConfigError::EmptyZones));
    }

    #[test]
    fn validate_rejects_gamma_zero() {
        let mut cfg = Config::default();
        cfg.color.gamma = 0.0;
        let err = cfg.validate().expect_err("gamma zero");
        assert!(matches!(err, ConfigError::InvalidGamma(_)));
    }

    #[test]
    fn validate_rejects_gamma_too_high() {
        let mut cfg = Config::default();
        cfg.color.gamma = 5.5;
        let err = cfg.validate().expect_err("gamma too high");
        assert!(matches!(err, ConfigError::InvalidGamma(_)));
    }

    #[test]
    fn update_brightness_gain_persists_to_yaml() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("config.yaml");
        // Seed a default config on disk.
        write_default(&path).expect("write default");
        // Mutate just the gain.
        update_brightness_gain(&path, 2.5).expect("persist gain");
        // Reload and verify only the gain changed.
        let reloaded = load(&path).expect("reload");
        assert_eq!(reloaded.color.brightness_gain, 2.5);
        // Other defaults preserved.
        assert_eq!(reloaded.color.gamma, 2.2);
    }

    #[test]
    fn update_brightness_gain_rejects_invalid_value() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("config.yaml");
        write_default(&path).expect("write default");
        let err = update_brightness_gain(&path, 0.0).expect_err("zero rejected");
        // Make sure the on-disk file was not clobbered when validation fails.
        let reloaded = load(&path).expect("reload");
        assert_eq!(reloaded.color.brightness_gain, 1.0);
        // Surface check: error message mentions brightness_gain so users get a clue.
        let msg = format!("{:#}", err);
        assert!(
            msg.contains("brightness_gain"),
            "error should mention field name, got: {msg}"
        );
    }

    #[test]
    fn validate_rejects_brightness_gain_zero() {
        let mut cfg = Config::default();
        cfg.color.brightness_gain = 0.0;
        let err = cfg.validate().expect_err("brightness_gain zero");
        assert!(matches!(err, ConfigError::InvalidBrightnessGain(_)));
    }

    #[test]
    fn validate_rejects_brightness_gain_too_high() {
        let mut cfg = Config::default();
        cfg.color.brightness_gain = 11.0;
        let err = cfg.validate().expect_err("brightness_gain too high");
        assert!(matches!(err, ConfigError::InvalidBrightnessGain(_)));
    }

    #[test]
    fn validate_accepts_brightness_gain_default() {
        let cfg = Config::default();
        assert_eq!(cfg.color.brightness_gain, 1.0);
        cfg.validate().expect("default brightness_gain valid");
    }

    #[test]
    fn validate_rejects_ema_alpha_zero() {
        let mut cfg = Config::default();
        cfg.smoothing.ema_alpha = 0.0;
        let err = cfg.validate().expect_err("ema_alpha zero");
        assert!(matches!(err, ConfigError::InvalidEmaAlpha(_)));
    }

    #[test]
    fn validate_rejects_ema_alpha_above_one() {
        let mut cfg = Config::default();
        cfg.smoothing.ema_alpha = 1.5;
        let err = cfg.validate().expect_err("ema_alpha above 1");
        assert!(matches!(err, ConfigError::InvalidEmaAlpha(_)));
    }

    #[test]
    fn validate_rejects_night_light_strength_negative() {
        let mut cfg = Config::default();
        cfg.color.night_light_strength = -0.1;
        let err = cfg.validate().expect_err("night_light_strength < 0");
        assert!(matches!(err, ConfigError::InvalidNightLightStrength(_)));
    }

    #[test]
    fn validate_rejects_night_light_strength_above_one() {
        let mut cfg = Config::default();
        cfg.color.night_light_strength = 1.5;
        let err = cfg.validate().expect_err("night_light_strength > 1");
        assert!(matches!(err, ConfigError::InvalidNightLightStrength(_)));
    }

    #[test]
    fn validate_accepts_night_light_bounds() {
        let mut cfg = Config::default();
        cfg.color.night_light_strength = 0.0;
        cfg.validate().expect("0.0 valid");
        cfg.color.night_light_strength = 1.0;
        cfg.validate().expect("1.0 valid");
    }

    #[test]
    fn validate_rejects_non_mqtt_scheme() {
        let mut cfg = Config::default();
        cfg.mqtt.broker_url = "http://broker.local:1883".into();
        let err = cfg.validate().expect_err("wrong scheme");
        assert!(matches!(err, ConfigError::InvalidBrokerScheme(_)));
    }

    #[test]
    fn validate_rejects_malformed_url() {
        let mut cfg = Config::default();
        cfg.mqtt.broker_url = "::not a url".into();
        let err = cfg.validate().expect_err("malformed url");
        assert!(matches!(err, ConfigError::InvalidBrokerUrl(_)));
    }

    #[test]
    fn validate_rejects_missing_broker_host() {
        let mut cfg = Config::default();
        cfg.mqtt.broker_url = "mqtt:///path".into();
        let err = cfg.validate().expect_err("missing host");
        assert!(matches!(err, ConfigError::MissingBrokerHost));
    }

    #[test]
    fn validate_accepts_mqtts_scheme() {
        let mut cfg = Config::default();
        cfg.mqtt.broker_url = "mqtts://broker.local:8883".into();
        cfg.validate().expect("mqtts is valid");
    }

    #[test]
    fn load_missing_path_writes_default_and_returns_it() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("rustiferin").join("config.yaml");
        assert!(!path.exists(), "precondition: file absent");

        let cfg = load(&path).expect("load creates default");
        assert!(path.exists(), "default written to disk");
        assert_eq!(cfg, Config::default());

        let on_disk = std::fs::read_to_string(&path).expect("read default");
        let reparsed: Config = serde_yaml_ng::from_str(&on_disk).expect("parse default");
        assert_eq!(reparsed, Config::default());
    }

    #[test]
    fn load_existing_file_returns_parsed_config() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("config.yaml");
        let yaml = "mqtt:\n  broker_url: \"mqtt://example:1883\"\n";
        std::fs::write(&path, yaml).expect("seed file");

        let cfg = load(&path).expect("load existing");
        assert_eq!(cfg.mqtt.broker_url, "mqtt://example:1883");
    }

    #[test]
    fn load_invalid_config_errors() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("config.yaml");
        let yaml = "color:\n  gamma: 0.0\n";
        std::fs::write(&path, yaml).expect("seed file");

        let err = load(&path).expect_err("validation should fail");
        let chain = format!("{err:#}");
        assert!(chain.contains("gamma"), "error mentions gamma: {chain}");
    }

    #[test]
    fn write_default_creates_parent_directories() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("a").join("b").join("config.yaml");
        write_default(&path).expect("write_default creates parents");
        assert!(path.exists());
    }

    #[test]
    fn resolve_default_path_prefers_xdg() {
        let p = resolve_default_path(Some("/x".into()), Some("/h".into())).expect("ok");
        assert_eq!(p, PathBuf::from("/x/rustiferin/config.yaml"));
    }

    #[test]
    fn resolve_default_path_falls_back_to_home_dot_config() {
        let p = resolve_default_path(None, Some("/h".into())).expect("ok");
        assert_eq!(p, PathBuf::from("/h/.config/rustiferin/config.yaml"));
    }

    #[test]
    fn resolve_default_path_treats_empty_xdg_as_unset() {
        let p = resolve_default_path(Some(OsString::new()), Some("/h".into())).expect("ok");
        assert_eq!(p, PathBuf::from("/h/.config/rustiferin/config.yaml"));
    }

    #[test]
    fn resolve_default_path_errors_when_neither_is_set() {
        let err = resolve_default_path(None, None).expect_err("no fallback");
        assert!(err.to_string().contains("XDG_CONFIG_HOME"));
    }

    #[test]
    fn validate_rejects_more_than_510_zones() {
        let mut cfg = Config::default();
        cfg.led_matrix.reference_width = 10000;
        cfg.led_matrix.zones = (0..511)
            .map(|i| LedZone {
                x: i,
                y: 0,
                w: 1,
                h: 1,
            })
            .collect();
        let err = cfg.validate().expect_err("over the limit");
        assert!(matches!(
            err,
            ConfigError::TooManyZones {
                count: 511,
                max: 510
            }
        ));
    }

    #[test]
    fn averaging_mode_default_is_mean() {
        assert_eq!(Config::default().color.averaging, AveragingMode::Mean);
    }

    #[test]
    fn averaging_mode_round_trips_through_yaml() {
        let mut cfg = Config::default();
        cfg.color.averaging = AveragingMode::DominantAdv;
        let yaml = serde_yaml_ng::to_string(&cfg).expect("serialize");
        assert!(
            yaml.contains("averaging: dominant-adv"),
            "expected kebab-case serialization, got:\n{yaml}"
        );
        let parsed: Config = serde_yaml_ng::from_str(&yaml).expect("deserialize");
        assert_eq!(parsed.color.averaging, AveragingMode::DominantAdv);
    }

    #[test]
    fn averaging_mode_accepts_kebab_case_in_yaml() {
        let yaml = "color:\n  averaging: dominant-adv\n";
        let parsed: Config = serde_yaml_ng::from_str(yaml).expect("deserialize kebab");
        assert_eq!(parsed.color.averaging, AveragingMode::DominantAdv);
    }

    #[test]
    fn averaging_mode_rejects_unknown_variant() {
        let yaml = "color:\n  averaging: random\n";
        let err = serde_yaml_ng::from_str::<Config>(yaml).expect_err("unknown variant rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("averaging") || msg.contains("variant"),
            "error should mention the bad variant, got: {msg}"
        );
    }

    #[test]
    fn validate_accepts_exactly_510_zones() {
        let mut cfg = Config::default();
        cfg.led_matrix.reference_width = 10000;
        cfg.led_matrix.zones = (0..510)
            .map(|i| LedZone {
                x: i,
                y: 0,
                w: 1,
                h: 1,
            })
            .collect();
        cfg.validate().expect("at the limit");
    }
}
