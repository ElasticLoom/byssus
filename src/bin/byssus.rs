//! `byssus` — the Byssus command-line tool.

// Doc comments double as `--help` text, where Markdown backticks would show.
#![allow(clippy::print_stdout, clippy::print_stderr, clippy::doc_markdown)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context as _;
use byssus::app::{self, ConfigSource};
use byssus::config::{self, Config, OwnershipPolicy, Severity};
use byssus::logging::{self, LogLevel};
use byssus::mountinfo::MountTable;
use byssus::name::Name;
use byssus::privileges::plan::Goal;
use byssus::probe::{self, Feature, PropagationCheck};
use byssus::reconcile::{self, Trigger, observe, plan};
use byssus::report::{self, Check, DryRunReport, Level, StatusReport};
use byssus::runtime::Runtime;
use byssus::state::{LoadError, LoadOutcome, State, StateStore};
use clap::{Args, Parser, Subcommand, ValueEnum};
use tracing::Level as TracingLevel;

/// Byssus: live, read-only filesystem attachments between isolated workspaces.
#[derive(Debug, Parser)]
#[command(name = "byssus", version, about, long_about = None)]
struct Cli {
    #[command(flatten)]
    config: ConfigArgs,

    /// Log level for diagnostics on stderr: error, warn, info, debug, trace.
    #[arg(long, global = true, value_name = "LEVEL")]
    log_level: Option<LogLevel>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Args)]
struct ConfigArgs {
    /// Main configuration file [default: /etc/byssus/byssus.toml].
    #[arg(long, global = true, value_name = "FILE")]
    config: Option<PathBuf>,

    /// Drop-in configuration directory [default: /etc/byssus/conf.d].
    #[arg(long, global = true, value_name = "DIR")]
    config_dir: Option<PathBuf>,
}

impl ConfigArgs {
    fn source(&self) -> ConfigSource {
        ConfigSource {
            file: self.config.clone(),
            dir: self.config_dir.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Format {
    Text,
    Json,
}

#[derive(Debug, Subcommand)]
enum Command {
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
    /// Print the version.
    Version,
}

#[derive(Debug, Args)]
struct PrivilegeArgs {
    /// Service user to switch to when started as root (overrides daemon.user).
    #[arg(long, value_name = "USER")]
    user: Option<String>,

    /// Permit running as UID 0 without a service user (development and tests).
    #[arg(long)]
    allow_root: bool,

    /// Permit target roots on slave-only mounts.
    #[arg(long)]
    allow_slave_namespace: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let default_level = match cli.command {
        Command::Reconcile { .. } => TracingLevel::INFO,
        _ => TracingLevel::WARN,
    };
    logging::init(cli.log_level.unwrap_or(LogLevel(default_level)));

    let result = match cli.command {
        Command::Version => {
            println!("byssus {}", byssus::VERSION);
            Ok(ExitCode::SUCCESS)
        }
        Command::Status { format } => status(&cli.config.source(), format),
        Command::DryRun { format } => dry_run(&cli.config.source(), format),
        Command::Reconcile { privileges } => reconcile_once(&cli.config.source(), &privileges),
    };
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("byssus: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn exit_for(level: Level) -> ExitCode {
    if level == Level::Error {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn print_report<T: serde::Serialize>(
    format: Format,
    text: impl FnOnce() -> String,
    value: &T,
) -> anyhow::Result<()> {
    match format {
        Format::Text => print!("{}", text()),
        Format::Json => println!("{}", serde_json::to_string_pretty(value)?),
    }
    Ok(())
}

/// Loads state without writing, explaining failures. Returns the state, whether
/// ownership is known, and notes.
fn load_state_read_only(config: &Config) -> (State, bool, Vec<String>, String) {
    let dir = config.daemon.state_dir.as_path();
    let path = dir
        .join(byssus::state::STATE_FILE_NAME)
        .display()
        .to_string();
    let store = match StateStore::open(dir) {
        Ok(store) => store,
        Err(e) => {
            let hint = if e.kind() == std::io::ErrorKind::PermissionDenied {
                " (join the byssus group or run as root to read state)"
            } else {
                ""
            };
            return (
                State::default(),
                false,
                vec![format!(
                    "cannot open state directory {}: {e}{hint}",
                    dir.display()
                )],
                path,
            );
        }
    };
    match store.load_read_only() {
        Ok(LoadOutcome::Loaded(state)) => (state, true, vec![], path),
        Ok(LoadOutcome::Missing) => (
            State::default(),
            true,
            vec!["no state file yet".into()],
            path,
        ),
        Ok(LoadOutcome::Corrupt { reason, .. }) => (
            State::default(),
            false,
            vec![format!("state file is corrupt: {reason}")],
            path,
        ),
        Err(LoadError::Io { source, .. })
            if source.kind() == std::io::ErrorKind::PermissionDenied =>
        {
            (
                State::default(),
                false,
                vec![format!(
                    "cannot read state file: {source} (join the byssus group or run as root)"
                )],
                path,
            )
        }
        Err(e) => (State::default(), false, vec![e.to_string()], path),
    }
}

fn status(source: &ConfigSource, format: Format) -> anyhow::Result<ExitCode> {
    let loaded = app::load_config(source, OwnershipPolicy::Warn)?;
    let config = loaded.config;
    let (state, ownership_known, mut notes, state_file) = load_state_read_only(&config);

    let (mut runtime, open_errors) = Runtime::open_lenient(&config);
    let degraded: BTreeSet<Name> = open_errors.iter().map(|e| e.group.clone()).collect();
    notes.extend(open_errors.iter().map(ToString::to_string));

    let unique = probe::probe_kernel().unique_mount_ids();
    let observed = observe::observe(&mut runtime, &state, &degraded, unique);
    notes.extend(observed.notes.iter().filter_map(|n| match n {
        observe::Note::MembershipDeleted { group } => Some(format!(
            "group '{group}': membership directory has been deleted"
        )),
        observe::Note::MembershipUnreadable { group, error } => Some(format!(
            "group '{group}': membership directory unreadable: {error}"
        )),
        _ => None,
    }));
    let plan = plan::plan(plan::PlanInput {
        desired: &observed.desired,
        state: &state,
        frozen_groups: &observed.frozen,
        observations: &observed.observations,
    });
    let report = StatusReport {
        state_file,
        ownership_known,
        notes,
        ignored: observed
            .ignored
            .iter()
            .map(|(group, names)| (group.to_string(), names.len()))
            .collect(),
        members: report::member_statuses(&observed, &plan, &state, ownership_known),
    };
    print_report(format, || report.to_text(), &report)?;
    Ok(exit_for(report.level()))
}

fn feature_check(name: &str, feature: Feature, required: bool) -> Check {
    match (feature, required) {
        (Feature::Available, _) => Check::new(format!("kernel.{name}"), Level::Ok, ""),
        (Feature::Missing, true) => Check::new(
            format!("kernel.{name}"),
            Level::Error,
            "missing; Linux 5.12 or newer is required",
        ),
        (Feature::Missing, false) => Check::new(
            format!("kernel.{name}"),
            Level::Ok,
            "not supported; reusable mount IDs plus device and inode are used",
        ),
    }
}

fn level_of(severity: Severity) -> Level {
    match severity {
        Severity::Error => Level::Error,
        Severity::Warning => Level::Warn,
    }
}

#[allow(clippy::too_many_lines)]
fn dry_run(source: &ConfigSource, format: Format) -> anyhow::Result<ExitCode> {
    let mut checks = Vec::new();

    let loaded = match config::load(&source.options(OwnershipPolicy::Warn)) {
        Ok(loaded) => loaded,
        Err(e) => {
            for issue in &e.issues {
                checks.push(Check::new(
                    "config",
                    level_of(issue.severity),
                    issue.to_string(),
                ));
            }
            let report = DryRunReport {
                checks,
                groups: vec![],
                members: vec![],
            };
            print_report(format, || report.to_text(), &report)?;
            return Ok(ExitCode::FAILURE);
        }
    };
    let config = loaded.config;
    if loaded.warnings.is_empty() {
        checks.push(Check::new(
            "config",
            Level::Ok,
            format!(
                "{} file(s), {} group(s)",
                config.files.len(),
                config.groups.len()
            ),
        ));
    }
    for issue in &loaded.warnings {
        checks.push(Check::new("config", Level::Warn, issue.to_string()));
    }

    match app::normalize_privileges(Goal::DropAll, config.daemon.user.as_deref(), true) {
        Ok(plan) => checks.push(Check::new(
            "privileges",
            Level::Ok,
            format!("checks run as uid {} with no capabilities", plan.final_uid),
        )),
        Err(e) => {
            checks.push(Check::new("privileges", Level::Error, format!("{e:#}")));
            // Still drop every capability before inspecting anything.
            app::normalize_privileges(Goal::DropAll, None, true)
                .context("cannot drop capabilities")?;
        }
    }

    let features = probe::probe_kernel();
    for (name, feature) in features.required() {
        checks.push(feature_check(name, feature, true));
    }
    checks.push(feature_check(
        "statx_mnt_id_unique",
        features.statx_mnt_id_unique,
        false,
    ));

    checks.push(match probe::verify_procfs(std::path::Path::new("/proc")) {
        Ok(_) => Check::new("procfs", Level::Ok, "/proc"),
        Err(e) => Check::new("procfs", Level::Error, e.to_string()),
    });

    let path_issues = config::check_paths(&config);
    if path_issues.is_empty() {
        checks.push(Check::new("config.paths", Level::Ok, ""));
    }
    for issue in path_issues {
        checks.push(Check::new(
            "config.paths",
            level_of(issue.severity),
            issue.to_string(),
        ));
    }

    let (state, ownership_known, state_notes, state_file) = load_state_read_only(&config);
    checks.push(Check::new(
        "state",
        if ownership_known {
            Level::Ok
        } else {
            Level::Warn
        },
        if state_notes.is_empty() {
            format!("{state_file} ({} record(s))", state.len())
        } else {
            format!("{state_file}: {}", state_notes.join("; "))
        },
    ));

    let (mut runtime, open_errors) = Runtime::open_lenient(&config);
    let mut extra: BTreeMap<Name, (String, Level, Vec<String>)> = BTreeMap::new();
    for e in &open_errors {
        extra.insert(
            e.group.clone(),
            ("unknown".into(), Level::Error, vec![e.to_string()]),
        );
    }
    let table = MountTable::read_self().context("cannot read /proc/self/mountinfo")?;
    let names: Vec<(Name, byssus::config::AbsPath)> = runtime
        .groups
        .values()
        .map(|g| (g.config.name.clone(), g.config.target_root.clone()))
        .collect();
    for (name, target_root) in names {
        let check = match runtime.roots.get(&target_root) {
            Ok(fd) => probe::check_propagation(fd, &table),
            Err(e) => PropagationCheck::Unknown(e),
        };
        let level = match check {
            PropagationCheck::Shared => Level::Ok,
            PropagationCheck::Private
            | PropagationCheck::Unbindable
            | PropagationCheck::Unknown(_) => Level::Warn,
            PropagationCheck::SlaveOnly => Level::Error,
        };
        extra.insert(name, (check.describe(), level, vec![]));
    }

    let degraded: BTreeSet<Name> = open_errors.iter().map(|e| e.group.clone()).collect();
    let observed = observe::observe(&mut runtime, &state, &degraded, features.unique_mount_ids());
    reconcile::NoteLog::default().report(&observed, Trigger::Cli);
    let plan = plan::plan(plan::PlanInput {
        desired: &observed.desired,
        state: &state,
        frozen_groups: &observed.frozen,
        observations: &observed.observations,
    });
    let members = report::member_statuses(&observed, &plan, &state, ownership_known);
    let groups: Vec<(Name, String)> = config
        .groups
        .values()
        .map(|g| (g.name.clone(), g.membership.to_string()))
        .collect();
    let report = DryRunReport {
        checks,
        groups: report::group_reports(&groups, &observed, &plan, &members, &extra),
        members,
    };
    print_report(format, || report.to_text(), &report)?;
    Ok(exit_for(report.level()))
}

fn reconcile_once(source: &ConfigSource, args: &PrivilegeArgs) -> anyhow::Result<ExitCode> {
    let loaded = app::load_config(source, OwnershipPolicy::Enforce)?;
    let config = loaded.config;
    app::set_umask();
    let user = args.user.as_deref().or(config.daemon.user.as_deref());
    app::normalize_privileges(Goal::KeepSysAdmin, user, args.allow_root)?;
    let environment = app::check_environment()?;
    app::verify_config_paths(&config)?;
    let mut writer = app::open_state_for_write(&config)?;
    let mut runtime = Runtime::open(&config)?;
    app::check_propagation(&mut runtime, args.allow_slave_namespace)?;

    let pass = reconcile::run_pass(
        &mut runtime,
        &mut writer.state,
        &writer.store,
        &BTreeSet::new(),
        environment.features.unique_mount_ids(),
        Trigger::Cli,
        &mut reconcile::NoteLog::default(),
    );
    // Persist even when nothing changed, as on daemon shutdown.
    writer
        .store
        .save(&writer.state)
        .context("cannot write state file")?;
    let failures = pass.execution.failures();
    tracing::info!(
        msg = "reconcile complete",
        steps = pass.plan.steps.len(),
        completed = pass.execution.completed(),
        failed = failures,
        findings = pass.plan.findings.len(),
    );
    drop(environment);
    Ok(if failures == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}
