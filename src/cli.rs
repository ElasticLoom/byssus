//! Command-line definitions for `byssusd` and `byssus`.
//!
//! Kept in the library so the binaries and the man page generator share one
//! source: the manual pages in `contrib/man/` are generated from these
//! definitions and checked against them by a unit test.

// Doc comments double as `--help` text, where Markdown backticks would show.
#![allow(clippy::doc_markdown)]

/// `byssusd`, the daemon.
pub mod byssusd {
    use std::path::PathBuf;

    use clap::Parser;

    use crate::logging::LogLevel;

    /// Byssus daemon: reconciles bind mounts from membership directories.
    ///
    /// byssusd watches the membership directories of every configured group
    /// and group set, and creates and removes bind mounts (read-only by
    /// default) beneath each target root, so consumers attached to a view see
    /// members appear and disappear. It keeps only CAP_SYS_ADMIN, and switches
    /// to the service user (daemon.user) when started as root.
    ///
    /// Signals: SIGHUP reloads the configuration, keeping the previous one if
    /// the new configuration is invalid. SIGTERM or SIGINT stops the daemon;
    /// mounts are preserved.
    ///
    /// Files (defaults): /etc/byssus/byssus.toml and /etc/byssus/conf.d/*.toml
    /// (configuration); /var/lib/byssus/state.json (state).
    ///
    /// Exit status: 0 after a clean shutdown, 1 if startup fails.
    ///
    /// See also byssus(8), and REFERENCE.md and OPERATIONS.md in
    /// /usr/share/doc/byssus.
    #[derive(Debug, Parser)]
    #[command(name = "byssusd", version)]
    pub struct Cli {
        /// Main configuration file [default: /etc/byssus/byssus.toml].
        #[arg(long, value_name = "FILE")]
        pub config: Option<PathBuf>,

        /// Drop-in configuration directory [default: /etc/byssus/conf.d].
        #[arg(long, value_name = "DIR")]
        pub config_dir: Option<PathBuf>,

        /// Log level: error, warn, info, debug, trace.
        #[arg(long, value_name = "LEVEL", default_value = "info")]
        pub log_level: LogLevel,

        /// Service user to switch to when started as root (overrides daemon.user).
        #[arg(long, value_name = "USER")]
        pub user: Option<String>,

        /// Permit running as UID 0 without a service user (development and tests).
        #[arg(long)]
        pub allow_root: bool,

        /// Permit target roots on slave mounts (normally a sign that the
        /// daemon runs in a non-host mount namespace).
        #[arg(long)]
        pub allow_slave_namespace: bool,
    }
}

/// `byssus`, the command-line tool.
pub mod byssus {
    use std::path::PathBuf;

    use clap::{Args, Parser, Subcommand, ValueEnum};

    use crate::app::ConfigSource;
    use crate::logging::LogLevel;

    /// Byssus command-line tool: inspect, validate and reconcile mounts.
    ///
    /// status, dry-run and check never mount anything; run as root with
    /// daemon.user configured, they look as that user, as the daemon would.
    /// reconcile performs one reconciliation pass and needs CAP_SYS_ADMIN.
    ///
    /// See also byssusd(8), and REFERENCE.md and OPERATIONS.md in
    /// /usr/share/doc/byssus.
    #[derive(Debug, Parser)]
    #[command(name = "byssus", version)]
    pub struct Cli {
        #[command(flatten)]
        pub config: ConfigArgs,

        /// Log level for diagnostics on stderr: error, warn, info, debug, trace.
        #[arg(long, global = true, value_name = "LEVEL")]
        pub log_level: Option<LogLevel>,

        #[command(subcommand)]
        pub command: Command,
    }

    #[derive(Debug, Args)]
    pub struct ConfigArgs {
        /// Main configuration file [default: /etc/byssus/byssus.toml].
        #[arg(long, global = true, value_name = "FILE")]
        pub config: Option<PathBuf>,

        /// Drop-in configuration directory [default: /etc/byssus/conf.d].
        #[arg(long, global = true, value_name = "DIR")]
        pub config_dir: Option<PathBuf>,
    }

    impl ConfigArgs {
        /// The configuration locations these arguments select.
        #[must_use]
        pub fn source(&self) -> ConfigSource {
            ConfigSource {
                file: self.config.clone(),
                dir: self.config_dir.clone(),
            }
        }
    }

    #[derive(Debug, Clone, Copy, ValueEnum)]
    pub enum Format {
        Text,
        Json,
    }

    #[derive(Debug, Subcommand)]
    pub enum Command {
        /// Show every desired or recorded member and its mount state.
        ///
        /// Exits 1 if any member is in an error state.
        Status {
            /// Output format.
            #[arg(long, value_enum, default_value_t = Format::Text)]
            format: Format,
        },
        /// Validate configuration, kernel, permissions and propagation, and show
        /// what a reconcile would do, without mounting anything.
        ///
        /// When run as root with a service user configured, checks run as that
        /// user. Exits 1 if any check fails.
        DryRun {
            /// Output format.
            #[arg(long, value_enum, default_value_t = Format::Text)]
            format: Format,
        },
        /// Perform one reconciliation pass and exit. Requires CAP_SYS_ADMIN and
        /// refuses to run while byssusd holds the state lock.
        Reconcile {
            #[command(flatten)]
            privileges: PrivilegeArgs,
        },
        /// Check whether the configuration would be accepted by a reload,
        /// optionally with drop-in fragments added, replaced or removed, without
        /// installing anything or signaling the daemon.
        ///
        /// Runs the same validation as a SIGHUP reload: parsing, ownership and
        /// modes, cross-group rules, paths, opening every root and membership
        /// directory, watches and propagation. When run as root with a service
        /// user configured, access is checked as that user. Exits 0 if the
        /// configuration would be accepted, 1 otherwise.
        Check {
            /// Candidate drop-in fragment, treated as installed in the drop-in
            /// directory under its file name (replacing a fragment of that name).
            /// May be repeated.
            #[arg(long = "add", value_name = "FILE")]
            add: Vec<PathBuf>,

            /// File name of an installed drop-in fragment to leave out. May be
            /// repeated.
            #[arg(long = "remove", value_name = "NAME")]
            remove: Vec<String>,

            /// Treat target roots on slave mounts as acceptable (as byssusd
            /// --allow-slave-namespace would).
            #[arg(long)]
            allow_slave_namespace: bool,

            /// Output format.
            #[arg(long, value_enum, default_value_t = Format::Text)]
            format: Format,
        },
        /// Print the version.
        Version,
    }

    #[derive(Debug, Args)]
    pub struct PrivilegeArgs {
        /// Service user to switch to when started as root (overrides daemon.user).
        #[arg(long, value_name = "USER")]
        pub user: Option<String>,

        /// Permit running as UID 0 without a service user (development and tests).
        #[arg(long)]
        pub allow_root: bool,

        /// Permit target roots on slave mounts.
        #[arg(long)]
        pub allow_slave_namespace: bool,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use clap::CommandFactory as _;

    const UPDATE_HINT: &str = "regenerate with: BYSSUS_UPDATE_MAN=1 cargo test --lib cli::";

    /// Renders `cmd` and its visible subcommands as section 8 manual pages.
    fn render(cmd: clap::Command, pages: &mut Vec<(String, Vec<u8>)>) {
        for sub in cmd.get_subcommands().filter(|s| !s.is_hide_set()).cloned() {
            render(sub, pages);
        }
        let man = clap_mangen::Man::new(cmd)
            .section("8")
            .source(format!("byssus {}", crate::VERSION))
            .manual("Byssus Manual");
        let mut page = Vec::new();
        man.render(&mut page).expect("render man page");
        pages.push((man.get_filename(), page));
    }

    fn pages() -> Vec<(String, Vec<u8>)> {
        let mut pages = Vec::new();
        for cmd in [
            super::byssusd::Cli::command(),
            super::byssus::Cli::command(),
        ] {
            let mut cmd = cmd.disable_help_subcommand(true);
            cmd.build();
            render(cmd, &mut pages);
        }
        pages.sort();
        pages
    }

    fn pages_on_disk(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .map(|entries| {
                entries
                    .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                    .filter(|name| name.ends_with(".8"))
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    #[test]
    fn man_pages_match_cli_definitions() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("contrib/man");
        let pages = pages();
        if std::env::var_os("BYSSUS_UPDATE_MAN").is_some() {
            fs::create_dir_all(&dir).unwrap();
            for stale in pages_on_disk(&dir) {
                fs::remove_file(dir.join(stale)).unwrap();
            }
            for (name, page) in &pages {
                fs::write(dir.join(name), page).unwrap();
            }
        }
        let expected: Vec<String> = pages.iter().map(|(name, _)| name.clone()).collect();
        assert_eq!(pages_on_disk(&dir), expected, "{UPDATE_HINT}");
        for (name, page) in &pages {
            assert!(
                fs::read(dir.join(name)).unwrap() == *page,
                "contrib/man/{name} is out of date; {UPDATE_HINT}"
            );
        }
    }
}
