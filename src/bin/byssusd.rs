//! `byssusd` — the Byssus mount reconciliation daemon.

// Doc comments double as `--help` text, where Markdown backticks would show.
#![allow(clippy::doc_markdown)]

use std::path::PathBuf;
use std::process::ExitCode;

use byssus::app::ConfigSource;
use byssus::daemon::{self, Options};
use byssus::logging::{self, LogLevel};
use clap::Parser;

/// Byssus daemon: watches membership directories and reconciles read-only
/// bind mounts. Send SIGHUP to reload configuration; SIGTERM or SIGINT to
/// stop (mounts are preserved).
#[derive(Debug, Parser)]
#[command(name = "byssusd", version, about, long_about = None)]
struct Cli {
    /// Main configuration file [default: /etc/byssus/byssus.toml].
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,

    /// Drop-in configuration directory [default: /etc/byssus/conf.d].
    #[arg(long, value_name = "DIR")]
    config_dir: Option<PathBuf>,

    /// Log level: error, warn, info, debug, trace.
    #[arg(long, value_name = "LEVEL", default_value = "info")]
    log_level: LogLevel,

    /// Service user to switch to when started as root (overrides daemon.user).
    #[arg(long, value_name = "USER")]
    user: Option<String>,

    /// Permit running as UID 0 without a service user (development and tests).
    #[arg(long)]
    allow_root: bool,

    /// Permit target roots on slave mounts (normally a sign that the
    /// daemon runs in a non-host mount namespace).
    #[arg(long)]
    allow_slave_namespace: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    logging::init(cli.log_level);
    let options = Options {
        config: ConfigSource {
            file: cli.config,
            dir: cli.config_dir,
        },
        user: cli.user,
        allow_root: cli.allow_root,
        allow_slave_namespace: cli.allow_slave_namespace,
    };
    match daemon::run(options) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(msg = "byssusd failed", error = %format!("{e:#}"));
            ExitCode::FAILURE
        }
    }
}
