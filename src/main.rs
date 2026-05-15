use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use rustiferin::{app, config};

const DEFAULT_LOG_FILTER: &str = "info,rustiferin=debug";

fn main() -> ExitCode {
    let args = app::Args::parse();

    // Config is loaded before tracing init so the config file's `general.log_level`
    // can influence the filter alongside `RUST_LOG` and `--log-level`. We don't
    // have tracing yet, so any load error has to surface via eprintln.
    let (config_path, config) = match load_config(&args) {
        Ok(c) => c,
        Err(err) => {
            eprintln!("error: failed to load config: {err:#}");
            return ExitCode::FAILURE;
        }
    };

    init_logging(&args, &config);

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            tracing::error!(error = ?err, "failed to build tokio runtime");
            return ExitCode::FAILURE;
        }
    };

    let exit = match runtime.block_on(app::run(args, config_path, config)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!(error = ?err, "rustiferin exited with error");
            ExitCode::FAILURE
        }
    };

    runtime.shutdown_timeout(std::time::Duration::from_secs(5));
    exit
}

/// Resolve the config path and load the file. Synchronous; no tracing yet.
fn load_config(args: &app::Args) -> anyhow::Result<(std::path::PathBuf, Arc<config::Config>)> {
    let config_path = match args.config.clone() {
        Some(p) => p,
        None => config::default_path().context("resolving default config path")?,
    };
    let cfg = config::load(&config_path)
        .with_context(|| format!("loading config from {}", config_path.display()))?;
    Ok((config_path, Arc::new(cfg)))
}

/// Build the `EnvFilter` from precedence: `RUST_LOG` > `--log-level` >
/// `config.general.log_level` > built-in default. Lower-priority sources are
/// only consulted when nothing higher is set.
fn init_logging(args: &app::Args, config: &config::Config) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        if let Some(level) = args.log_level.as_deref() {
            return EnvFilter::new(level);
        }
        if let Some(level) = config.general.log_level.as_deref() {
            return EnvFilter::new(level);
        }
        EnvFilter::new(DEFAULT_LOG_FILTER)
    });
    let fmt_layer = fmt::layer().with_target(true);
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt_layer)
        .init();
}
