//! `byssusd` — the Byssus mount reconciliation daemon.

use std::process::ExitCode;

use byssus::app::ConfigSource;
use byssus::cli::byssusd::Cli;
use byssus::daemon::{self, Options};
use byssus::logging;
use clap::Parser as _;

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
