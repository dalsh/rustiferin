use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use clap::Parser;

use crate::config;
#[cfg(all(feature = "wayland", feature = "mqtt"))]
use crate::shutdown::Shutdown;

#[cfg(all(feature = "wayland", feature = "mqtt"))]
use {
    crate::capture::{self, portal::PortalCapture, FramePool, FrameSlot},
    crate::output::{self, OutputState},
    crate::pipeline::{self, LedFrame, PipelineCommand},
    crate::power,
    crate::stats::{self, Metrics, Stats},
    tokio::sync::{mpsc, watch},
};

#[cfg(all(feature = "wayland", feature = "mqtt", feature = "tray"))]
use crate::tray::{self, ProductionTrayServiceFactory, TrayCommand};

/// Recycled buffer slots on the capture -> pipeline hop.
#[cfg(all(feature = "wayland", feature = "mqtt"))]
const POOL_SIZE: usize = 4;
/// Upper bound for a BGRA frame at 3840×2160; the pool sizes every buffer to this.
#[cfg(all(feature = "wayland", feature = "mqtt"))]
const MAX_FRAME_BYTES: usize = 4 * 3840 * 2160;

#[derive(Debug, Parser)]
#[command(name = "rustiferin", version, about)]
pub struct Args {
    /// Override the default `$XDG_CONFIG_HOME/rustiferin/config.yaml` path.
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// `EnvFilter` directive used when `RUST_LOG` is unset. Lower priority than
    /// `RUST_LOG`, higher priority than `general.log_level` in the config file.
    #[arg(long, value_name = "LEVEL")]
    pub log_level: Option<String>,

    /// Skip the system tray icon.
    #[arg(long)]
    pub headless: bool,
}

pub async fn run(
    args: Args,
    config_path: PathBuf,
    config: Arc<config::Config>,
) -> anyhow::Result<()> {
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        headless = args.headless,
        config = %config_path.display(),
        "rustiferin starting"
    );
    tracing::info!(
        device = %config.general.device_name,
        broker = %config.mqtt.broker_url,
        zones = config.led_matrix.zones.len(),
        "config loaded"
    );
    let zone_count = config.led_matrix.zones.len();

    #[cfg(not(all(feature = "wayland", feature = "mqtt")))]
    {
        let _ = (zone_count, &config);
        anyhow::bail!("rustiferin binary requires both `wayland` and `mqtt` features");
    }

    #[cfg(all(feature = "wayland", feature = "mqtt"))]
    {
        let mut shutdown = Shutdown::new();
        let (leds_tx, leds_rx) = watch::channel(LedFrame::black(zone_count, 0));
        let (state_tx, state_rx) = watch::channel(OutputState::Connecting);
        let (pipeline_ctrl_tx, pipeline_ctrl_rx) = mpsc::channel::<PipelineCommand>(8);
        let (stats_tx, _stats_rx_for_tray) = watch::channel(Stats::default());

        let metrics = Metrics::new();
        let pool = FramePool::new(POOL_SIZE, MAX_FRAME_BYTES);
        let frame_slot = FrameSlot::new();

        let restore_path = xdg_state_home()
            .context("resolving XDG_STATE_HOME for portal restore token")?
            .join("rustiferin")
            .join("restore_token");
        let target_fps = config.capture.target_fps;
        let portal = PortalCapture::new(restore_path, target_fps);
        capture::spawn(
            &mut shutdown,
            portal,
            pool.clone(),
            frame_slot.clone(),
            metrics.clone(),
        );

        let brightness_gain = crate::runtime::BrightnessGain::new(config.color.brightness_gain);
        pipeline::spawn(
            &mut shutdown,
            config.clone(),
            pool,
            frame_slot,
            leds_tx,
            pipeline_ctrl_rx,
            metrics.clone(),
            brightness_gain.clone(),
        )
        .context("spawning pipeline thread")?;

        output::spawn(
            &mut shutdown,
            config.clone(),
            leds_rx,
            state_tx,
            metrics.clone(),
        );

        let power_cancel = shutdown.token();
        #[cfg(feature = "tray")]
        let pipeline_ctrl_for_tray = pipeline_ctrl_tx.clone();
        shutdown.spawn(
            "power",
            power::run(config.clone(), pipeline_ctrl_tx, power_cancel),
        );

        let aggregator_cancel = shutdown.token();
        let aggregator_metrics = metrics.clone();
        shutdown.spawn("stats", async move {
            stats::aggregator(aggregator_metrics, state_rx, stats_tx, aggregator_cancel).await
        });

        #[cfg(feature = "tray")]
        {
            if args.headless {
                // `_stats_rx_for_tray` keeps the watch sender alive even though
                // the tray task is skipped; without a live receiver the
                // aggregator's `send` would error.
                let _keep_stats_alive = _stats_rx_for_tray;
            } else {
                spawn_tray(
                    &mut shutdown,
                    config_path.clone(),
                    _stats_rx_for_tray,
                    pipeline_ctrl_for_tray,
                    brightness_gain.clone(),
                );
            }
        }
        #[cfg(not(feature = "tray"))]
        let _keep_stats_alive = _stats_rx_for_tray;

        shutdown.run_until_signal().await?;
        tracing::info!("shutdown complete");
        Ok(())
    }
}

/// Spawn the tray task and a small router that translates [`TrayCommand`]s into
/// global cancellation, `xdg-open` invocations, or `PipelineCommand` toggles.
///
/// The router keeps its own `running` flag in sync with the tray's: each
/// `ToggleRun` flip in `tray::menu` is mirrored here so the router knows
/// whether to send `Blackout` (now paused) or `Resume` (now active). Power's
/// own blackout/resume traffic is unaffected, both senders feed the same
/// pipeline-control channel; first-writer-wins arbitration is acceptable
/// for v1.
#[cfg(all(feature = "wayland", feature = "mqtt", feature = "tray"))]
fn spawn_tray(
    shutdown: &mut Shutdown,
    config_path: PathBuf,
    stats_rx: watch::Receiver<Stats>,
    pipeline_ctrl_tx: mpsc::Sender<PipelineCommand>,
    brightness_gain: crate::runtime::BrightnessGain,
) {
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<TrayCommand>(8);
    let cancel = shutdown.token();

    let brightness_gain_for_tray = brightness_gain.clone();
    shutdown.spawn(
        "tray",
        tray::run(
            Box::new(ProductionTrayServiceFactory::new()),
            stats_rx,
            cmd_tx,
            brightness_gain_for_tray,
            cancel.clone(),
        ),
    );

    shutdown.spawn("tray-commands", async move {
        let mut running = true;
        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                TrayCommand::Quit => {
                    tracing::info!("tray: quit requested");
                    cancel.cancel();
                }
                TrayCommand::ToggleRun => {
                    running = !running;
                    let pipeline_cmd = if running {
                        PipelineCommand::Resume
                    } else {
                        PipelineCommand::Blackout
                    };
                    tracing::info!(?pipeline_cmd, "tray: toggle requested");
                    let _ = pipeline_ctrl_tx.send(pipeline_cmd).await;
                }
                TrayCommand::OpenConfig => {
                    let path = config_path.clone();
                    if let Err(e) = std::process::Command::new("xdg-open").arg(&path).spawn() {
                        tracing::warn!(error = ?e, path = %path.display(), "xdg-open failed");
                    }
                }
                TrayCommand::SetBrightnessGain(value) => {
                    tracing::info!(value, "tray: brightness_gain set");
                    brightness_gain.store(value);
                    let path = config_path.clone();
                    // Persistence is best-effort: if the YAML write fails we log
                    // and leave the in-memory atomic updated. Next launch falls
                    // back to whatever is still on disk.
                    if let Err(e) = crate::config::update_brightness_gain(&path, value) {
                        tracing::warn!(error = ?e, path = %path.display(), "persisting brightness_gain failed");
                    }
                }
            }
        }
        Ok(())
    });
}

/// Resolve `$XDG_STATE_HOME` with the spec-defined fallback to `$HOME/.local/state`.
/// Returns an explicit error if neither is set rather than silently picking a default.
#[cfg(all(feature = "wayland", feature = "mqtt"))]
fn xdg_state_home() -> anyhow::Result<PathBuf> {
    if let Some(v) = std::env::var_os("XDG_STATE_HOME") {
        if !v.is_empty() {
            return Ok(PathBuf::from(v));
        }
    }
    let home = std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .ok_or_else(|| anyhow::anyhow!("neither XDG_STATE_HOME nor HOME is set"))?;
    Ok(PathBuf::from(home).join(".local").join("state"))
}
