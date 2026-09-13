//! Startup logic shared by `byssusd` and the `byssus` CLI.

use std::collections::BTreeMap;
use std::os::fd::OwnedFd;
use std::path::PathBuf;

use anyhow::{Context as _, anyhow, bail};
use jiff::Timestamp;

use crate::config::{self, Config, Issue, LoadOptions, Loaded, OwnershipPolicy, Severity};
use crate::lock::{LockError, StateLock};
use crate::mountinfo::MountTable;
use crate::privileges::apply::{self, describe_caps};
use crate::privileges::plan::{self as privplan, Goal, PrivilegePlan, Request, Warning};
use crate::probe::{self, KernelFeatures, PropagationCheck};
use crate::runtime::Runtime;
use crate::state::{LoadOutcome, State, StateStore};
use crate::users;

/// Where configuration comes from.
#[derive(Debug, Clone, Default)]
pub struct ConfigSource {
    /// Explicit main file (`--config`).
    pub file: Option<PathBuf>,
    /// Explicit fragment directory (`--config-dir`).
    pub dir: Option<PathBuf>,
}

impl ConfigSource {
    /// Load options for this source.
    #[must_use]
    pub fn options(&self, ownership: OwnershipPolicy) -> LoadOptions {
        let defaults = LoadOptions::default();
        LoadOptions {
            main_file: self.file.clone().unwrap_or(defaults.main_file),
            main_file_required: self.file.is_some(),
            config_dir: self.dir.clone().unwrap_or(defaults.config_dir),
            config_dir_required: self.dir.is_some(),
            ownership,
            trusted_uid: 0,
            check_paths: false,
            fragment_changes: config::FragmentChanges::default(),
        }
    }
}

/// Formats configuration issues, one per line.
#[must_use]
pub fn format_issues(issues: &[Issue]) -> String {
    issues
        .iter()
        .map(|i| {
            let level = match i.severity {
                Severity::Error => "error",
                Severity::Warning => "warning",
            };
            format!("{level}: {i}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Loads configuration, logging warnings. Errors carry every issue.
pub fn load_config(source: &ConfigSource, ownership: OwnershipPolicy) -> anyhow::Result<Loaded> {
    let loaded = config::load(&source.options(ownership))
        .map_err(|e| anyhow!("{e}\n{}", format_issues(&e.issues)))?;
    for issue in &loaded.warnings {
        tracing::warn!(msg = "configuration", issue = %issue);
    }
    Ok(loaded)
}

/// Normalizes privileges and logs the result.
///
/// `user` is the service user name from `--user` or `daemon.user`.
pub fn normalize_privileges(
    goal: Goal,
    user: Option<&str>,
    allow_root: bool,
) -> anyhow::Result<PrivilegePlan> {
    let status = apply::ProcStatus::read_self().context("cannot read /proc/self/status")?;
    let credentials = status.credentials();
    let is_root = credentials.uids.contains(&0);
    let service_user = match user {
        Some(name) => match users::resolve(name) {
            Ok(u) => Some(u),
            Err(e) if is_root => return Err(e).context("cannot resolve service user"),
            Err(e) => {
                tracing::warn!(msg = "cannot resolve configured service user", user = name, error = %e);
                None
            }
        },
        None => None,
    };
    let request = Request {
        goal,
        user: service_user,
        allow_root,
    };
    let plan = privplan::plan(&credentials, &request)?;
    for warning in &plan.warnings {
        match warning {
            Warning::RunningAsRoot => tracing::warn!(
                msg = "running as root because --allow-root was given; configure a service user for production"
            ),
            Warning::UserNotApplied {
                user,
                effective_uid,
            } => tracing::warn!(
                msg = "not running as root, so the configured service user is not applied",
                user = %user,
                uid = effective_uid,
            ),
        }
    }
    apply::apply(&plan)?;
    tracing::info!(
        msg = "privileges normalized",
        uid = plan.final_uid,
        caps = %describe_caps(plan.final_caps),
    );
    Ok(plan)
}

/// Verified environment facts.
#[derive(Debug)]
pub struct Environment {
    /// Kernel features.
    pub features: KernelFeatures,
    /// Verified `/proc` descriptor, held for the process lifetime.
    pub proc: OwnedFd,
}

/// Probes the kernel and verifies `/proc`. Fails if a required feature is
/// missing.
pub fn check_environment() -> anyhow::Result<Environment> {
    let features = probe::probe_kernel();
    let missing = features.missing_required();
    if !missing.is_empty() {
        let names: Vec<&str> = missing.iter().map(|(n, _)| *n).collect();
        bail!(
            "required kernel features are unavailable: {} ({})",
            names.join(", "),
            probe::missing_features_hint(probe::seccomp_filter_active())
        );
    }
    let proc =
        probe::verify_procfs(std::path::Path::new("/proc")).context("/proc is not usable")?;
    tracing::debug!(
        msg = "kernel features",
        unique_mount_ids = features.unique_mount_ids()
    );
    Ok(Environment { features, proc })
}

/// Checks configured paths as the current user; fails on errors.
pub fn verify_config_paths(config: &Config) -> anyhow::Result<()> {
    let issues = config::check_paths(config);
    for issue in issues.iter().filter(|i| i.severity == Severity::Warning) {
        tracing::warn!(msg = "configuration", issue = %issue);
    }
    let errors: Vec<Issue> = issues
        .into_iter()
        .filter(|i| i.severity == Severity::Error)
        .collect();
    if errors.is_empty() {
        Ok(())
    } else {
        bail!("invalid configuration paths\n{}", format_issues(&errors))
    }
}

/// The target roots to check for propagation: one per statically configured
/// group (labeled by group) and one per group set (labeled `set/*`).
#[must_use]
pub fn propagation_targets(runtime: &Runtime) -> Vec<(String, crate::config::AbsPath)> {
    let mut targets: Vec<(String, crate::config::AbsPath)> = runtime
        .groups
        .values()
        .filter(|g| g.config.name.set().is_none())
        .map(|g| (g.config.name.to_string(), g.config.target_root.clone()))
        .collect();
    targets.extend(runtime.sets.values().map(|set| {
        (
            crate::reconcile::set_label(&set.config.name),
            set.config.target_root.clone(),
        )
    }));
    targets
}

/// Checks propagation for every group and group set, logging the outcome.
/// Returns an error if a target root is a slave mount and `allow_slave` is
/// false.
pub fn check_propagation(
    runtime: &mut Runtime,
    allow_slave: bool,
) -> anyhow::Result<BTreeMap<String, PropagationCheck>> {
    let table = MountTable::read_self().context("cannot read /proc/self/mountinfo")?;
    let mut results = BTreeMap::new();
    let groups = propagation_targets(runtime);
    let mut slave_groups = Vec::new();
    for (name, target_root) in groups {
        let check = match runtime.roots.get(&target_root) {
            Ok(fd) => probe::check_propagation(fd, &table),
            Err(e) => PropagationCheck::Unknown(e),
        };
        match &check {
            PropagationCheck::Shared => {
                tracing::debug!(group = %name, target_root = %target_root, propagation = "shared");
            }
            check if check.is_slave() => {
                tracing::error!(group = %name, target_root = %target_root, msg = %check.describe());
                slave_groups.push(name.to_string());
            }
            _ => tracing::warn!(group = %name, target_root = %target_root, msg = %check.describe()),
        }
        results.insert(name, check);
    }
    if probe::same_mount_namespace_as_init() == Some(false) {
        tracing::error!(
            msg = "this process is not in PID 1's mount namespace; mounts may not be visible to containers"
        );
    }
    if !slave_groups.is_empty() && !allow_slave {
        bail!(
            "target roots of groups {} are slave mounts, so mounts would not propagate back to the host; refusing to start (is byssusd in a private mount namespace? Use --allow-slave-namespace to override)",
            slave_groups.join(", ")
        );
    }
    Ok(results)
}

/// Everything a mounting process holds after startup.
#[derive(Debug)]
pub struct Writer {
    /// State store.
    pub store: StateStore,
    /// Exclusive state lock.
    pub lock: StateLock,
    /// Loaded state.
    pub state: State,
}

/// Opens the state directory, takes the lock and loads state for writing.
pub fn open_state_for_write(config: &Config) -> anyhow::Result<Writer> {
    let dir = config.daemon.state_dir.as_path();
    let store = StateStore::open(dir)
        .with_context(|| format!("cannot open state directory {}", dir.display()))?;
    let lock = match StateLock::acquire(store.dir_fd()) {
        Ok(lock) => lock,
        Err(LockError::Held) => bail!(
            "another Byssus process holds the state lock in {} (is byssusd running? send it SIGHUP instead)",
            dir.display()
        ),
        Err(e) => return Err(e.into()),
    };
    let state = match store.load_for_write(Timestamp::now())? {
        LoadOutcome::Loaded(state) => state,
        LoadOutcome::Missing => {
            tracing::info!(msg = "no state file; starting with empty state", path = %store.path().display());
            State::default()
        }
        LoadOutcome::Corrupt {
            reason,
            preserved_as,
        } => {
            tracing::error!(
                msg = "state file is corrupt; starting with empty state (existing mounts will be reported as conflicts)",
                reason = %reason,
                preserved_as = %preserved_as.as_ref().map_or_else(String::new, |p| p.display().to_string()),
            );
            State::default()
        }
    };
    Ok(Writer { store, lock, state })
}

/// Sets the process umask so created directories and files get exact modes.
pub fn set_umask() {
    rustix::process::umask(rustix::fs::Mode::from_raw_mode(0o022));
}
