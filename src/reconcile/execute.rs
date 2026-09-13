//! Executing a reconciliation plan against the kernel and the state file.

use std::os::fd::AsFd;

use jiff::Timestamp;

use crate::fsops;
use crate::mount::{self, VerifiedOpError};
use crate::reconcile::Trigger;
use crate::reconcile::plan::{Action, DesiredMount, DropReason, Location, Plan, UnmountReason};
use crate::runtime::Runtime;
use crate::state::{MountRecord, State, StateStore};

/// What happened to one step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepResult {
    /// The step completed.
    Done,
    /// The step was not attempted because a dependency did not complete.
    Skipped,
    /// The step failed.
    Failed(String),
}

/// Results of executing a plan, index-aligned with `plan.steps`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Execution {
    /// Per-step results.
    pub results: Vec<StepResult>,
}

impl Execution {
    /// Number of failed steps.
    #[must_use]
    pub fn failures(&self) -> usize {
        self.results
            .iter()
            .filter(|r| matches!(r, StepResult::Failed(_)))
            .count()
    }

    /// Number of completed steps.
    #[must_use]
    pub fn completed(&self) -> usize {
        self.results
            .iter()
            .filter(|r| **r == StepResult::Done)
            .count()
    }
}

/// Where state changes are persisted.
pub trait StateSink {
    /// Persists `state`.
    fn save(&self, state: &State) -> std::io::Result<()>;
}

impl StateSink for StateStore {
    fn save(&self, state: &State) -> std::io::Result<()> {
        StateStore::save(self, state)
    }
}

/// Executes plans.
#[derive(Debug)]
pub struct Executor<'a, S: StateSink> {
    /// Opened descriptors.
    pub runtime: &'a mut Runtime,
    /// In-memory state, updated as steps complete.
    pub state: &'a mut State,
    /// Persistence for `state`.
    pub sink: &'a S,
    /// Whether unique mount IDs are available.
    pub unique_supported: bool,
    /// Why this pass is running.
    pub trigger: Trigger,
}

impl<S: StateSink> Executor<'_, S> {
    /// Executes every step of `plan` in order.
    pub fn execute(&mut self, plan: &Plan) -> Execution {
        let mut results: Vec<StepResult> = Vec::with_capacity(plan.steps.len());
        for step in &plan.steps {
            let blocked = step
                .depends_on
                .iter()
                .any(|&d| results.get(d) != Some(&StepResult::Done));
            let result = if blocked {
                log_skipped(&step.action, self.trigger);
                StepResult::Skipped
            } else {
                self.run(&step.action)
            };
            results.push(result);
        }
        Execution { results }
    }

    fn run(&mut self, action: &Action) -> StepResult {
        match action {
            Action::Mount { desired } => self.mount(desired),
            Action::Unmount { record, reason } => self.unmount(record, reason),
            Action::DropRecord { record, reason } => self.drop_record(record, reason),
        }
    }

    fn persist(&self) -> Result<(), String> {
        self.sink
            .save(self.state)
            .map_err(|e| format!("cannot write state file: {e}"))
    }

    fn mount(&mut self, d: &DesiredMount) -> StepResult {
        let result = self.try_mount(d);
        let trigger = self.trigger;
        match &result {
            Ok(()) => tracing::info!(
                op = "mount",
                group = %d.key.group,
                name = %d.key.name,
                source = %d.source,
                target = %d.target,
                trigger = %trigger,
                result = "ok",
            ),
            Err(error) => tracing::error!(
                op = "mount",
                group = %d.key.group,
                name = %d.key.name,
                source = %d.source,
                target = %d.target,
                trigger = %trigger,
                result = "failed",
                error = %error,
            ),
        }
        into_result(result)
    }

    fn try_mount(&mut self, d: &DesiredMount) -> Result<(), String> {
        let unique = self.unique_supported;
        // Resolved again, not reused from observation, so confinement is
        // checked at use.
        let source = {
            let root = self.runtime.roots.get(&d.source.root)?;
            fsops::resolve_dir(root, &d.source.path)
                .map_err(|e| format!("source {}: {e}", d.source))?
        };
        let components: Vec<String> = d.target.path.split('/').map(str::to_owned).collect();
        let root = self.runtime.roots.get(&d.target.root)?;
        let target = fsops::ensure_dirs_beneath(root, &components)
            .map_err(|e| format!("target {}: {e}", d.target))?;
        if mount::is_mount_root(target.as_fd()).map_err(|e| format!("target {}: {e}", d.target))? {
            return Err(format!(
                "target {} became occupied by another mount",
                d.target
            ));
        }
        let identity = mount::create_bind(source.as_fd(), target.as_fd(), &d.attrs, unique)
            .map_err(|e| e.to_string())?;

        let record = MountRecord {
            group: d.key.group.clone(),
            name: d.key.name.clone(),
            source_root: d.source.root.clone(),
            source: d.source.path.clone(),
            target_root: d.target.root.clone(),
            target: d.target.path.clone(),
            mnt_id: identity.mnt_id,
            mnt_id_unique: identity.mnt_id_unique,
            root_dev_major: identity.root.dev_major,
            root_dev_minor: identity.root.dev_minor,
            root_ino: identity.root.ino,
            read_only: d.attrs.read_only,
            noexec: d.attrs.noexec,
            nosymfollow: d.attrs.nosymfollow,
            created_at: Timestamp::now(),
        };
        let previous = self.state.insert(record);
        if let Err(save_error) = self.persist() {
            // Never leave a mount we could not record: it would look foreign
            // after a restart. Roll back.
            let key = d.key.clone();
            match previous {
                Some(prev) => {
                    self.state.insert(prev);
                }
                None => {
                    self.state.remove(&key);
                }
            }
            let root = self.runtime.roots.get(&d.target.root)?;
            let rollback = mount::unmount_verified(root, &d.target.path, &identity, unique);
            return Err(match rollback {
                Ok(()) => format!("{save_error}; mount rolled back"),
                Err(e) => format!("{save_error}; rollback unmount also failed: {e}"),
            });
        }
        Ok(())
    }

    fn unmount(&mut self, record: &MountRecord, reason: &UnmountReason) -> StepResult {
        let result = self.try_unmount(record);
        let trigger = self.trigger;
        let target = record.target_location();
        let reason = describe_unmount(reason);
        match &result {
            Ok(()) => tracing::info!(
                op = "unmount",
                group = %record.group,
                name = %record.name,
                source = %record.source_location(),
                target = %target,
                trigger = %trigger,
                result = "ok",
                reason = %reason,
            ),
            Err(error) => tracing::error!(
                op = "unmount",
                group = %record.group,
                name = %record.name,
                target = %target,
                trigger = %trigger,
                result = "failed",
                reason = %reason,
                error = %error,
            ),
        }
        into_result(result)
    }

    fn try_unmount(&mut self, record: &MountRecord) -> Result<(), String> {
        let unique = self.unique_supported;
        let target = record.target_location();
        {
            let root = self.runtime.roots.get(&target.root)?;
            mount::unmount_verified(root, &target.path, &record.identity(), unique).map_err(
                |e| match e {
                    VerifiedOpError::IdentityMismatch { .. } | VerifiedOpError::NotMounted => {
                        format!("refusing to unmount: {e}")
                    }
                    VerifiedOpError::Io(io) => io.to_string(),
                },
            )?;
        }
        self.state.remove(&record.key());
        let persisted = self.persist();
        remove_target_dir(self.runtime, &target);
        persisted
    }

    fn drop_record(&mut self, record: &MountRecord, reason: &DropReason) -> StepResult {
        self.state.remove(&record.key());
        let result = self.persist();
        let trigger = self.trigger;
        let reason = match reason {
            DropReason::MountGone => "nothing mounted at recorded target".to_owned(),
            DropReason::ForeignMountAtTarget => {
                "a different mount is at the recorded target; not touching it".to_owned()
            }
            DropReason::SourceUnavailable(e) => format!("source unavailable: {e}"),
        };
        match &result {
            Ok(()) => tracing::warn!(
                op = "drop_record",
                group = %record.group,
                name = %record.name,
                target = %record.target_location(),
                trigger = %trigger,
                result = "ok",
                reason = %reason,
            ),
            Err(error) => tracing::error!(
                op = "drop_record",
                group = %record.group,
                name = %record.name,
                target = %record.target_location(),
                trigger = %trigger,
                result = "failed",
                reason = %reason,
                error = %error,
            ),
        }
        into_result(result)
    }
}

fn into_result(result: Result<(), String>) -> StepResult {
    match result {
        Ok(()) => StepResult::Done,
        Err(e) => StepResult::Failed(e),
    }
}

fn remove_target_dir(runtime: &mut Runtime, target: &Location) {
    let (parent, leaf) = match target.path.rsplit_once('/') {
        Some((parent, leaf)) => (Some(parent), leaf),
        None => (None, target.path.as_str()),
    };
    let Ok(root) = runtime.roots.get(&target.root) else {
        return;
    };
    if let Err(e) = fsops::remove_empty_dir(root, parent, leaf) {
        tracing::warn!(target = %target, error = %e, msg = "could not remove target directory");
    }
}

fn describe_unmount(reason: &UnmountReason) -> String {
    match reason {
        UnmountReason::NotMember => "member removed".into(),
        UnmountReason::Relocated => "member location changed".into(),
        UnmountReason::SourceGone(e) => format!("source unavailable: {e}"),
        UnmountReason::SourceChanged => "source directory replaced".into(),
        UnmountReason::TargetCollision => "target collides with another member".into(),
        UnmountReason::AttributesChanged => "configured mount attributes changed".into(),
        UnmountReason::AttributesDrifted => "mount no longer enforces configured attributes".into(),
    }
}

fn log_skipped(action: &Action, trigger: Trigger) {
    let (op, key, target) = match action {
        Action::Mount { desired } => ("mount", desired.key.clone(), desired.target.clone()),
        Action::Unmount { record, .. } => ("unmount", record.key(), record.target_location()),
        Action::DropRecord { record, .. } => {
            ("drop_record", record.key(), record.target_location())
        }
    };
    tracing::warn!(
        op = op,
        group = %key.group,
        name = %key.name,
        target = %target,
        trigger = %trigger,
        result = "skipped",
        reason = "a prerequisite step did not complete",
    );
}
