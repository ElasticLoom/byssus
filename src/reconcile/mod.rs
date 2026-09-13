//! Reconciliation of desired membership against state and the kernel.
//!
//! A pass is: [`observe`] → [`plan`] → report findings → [`execute`].

pub mod execute;
pub mod observe;
pub mod plan;

use std::collections::BTreeSet;
use std::fmt;

use crate::name::Name;
use crate::runtime::Runtime;
use crate::state::State;

use self::execute::{Execution, Executor, StateSink};
use self::observe::{Note, Observed};
use self::plan::{ConflictKind, Finding, Plan, PlanInput};

/// Why a reconciliation pass is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// Daemon startup.
    Startup,
    /// A membership directory changed.
    Inotify,
    /// Periodic resynchronization.
    Resync,
    /// Configuration reload.
    Reload,
    /// `byssus reconcile`.
    Cli,
}

impl fmt::Display for Trigger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Startup => "startup",
            Self::Inotify => "inotify",
            Self::Resync => "resync",
            Self::Reload => "reload",
            Self::Cli => "cli",
        })
    }
}

/// The outcome of a pass.
#[derive(Debug)]
pub struct Pass {
    /// What was observed.
    pub observed: Observed,
    /// What was planned.
    pub plan: Plan,
    /// What happened.
    pub execution: Execution,
}

/// Observes, plans, logs findings and executes one reconciliation pass.
pub fn run_pass<S: StateSink>(
    runtime: &mut Runtime,
    state: &mut State,
    sink: &S,
    degraded: &BTreeSet<Name>,
    unique_supported: bool,
    trigger: Trigger,
) -> Pass {
    let observed = observe::observe(runtime, state, degraded, unique_supported);
    log_notes(&observed.notes, trigger);
    let plan = plan::plan(PlanInput {
        desired: &observed.desired,
        state,
        frozen_groups: &observed.frozen,
        observations: &observed.observations,
    });
    log_findings(&plan.findings, trigger);
    let execution = Executor {
        runtime,
        state,
        sink,
        unique_supported,
        trigger,
    }
    .execute(&plan);
    Pass {
        observed,
        plan,
        execution,
    }
}

/// Logs observation notes.
pub fn log_notes(notes: &[Note], trigger: Trigger) {
    for note in notes {
        match note {
            Note::Rejected { group, rejection } => tracing::warn!(
                op = "reject",
                group = %group,
                name = %rejection.display_name,
                trigger = %trigger,
                reason = %rejection.reason,
            ),
            Note::InterpolationFailed {
                group,
                name,
                field,
                error,
            } => tracing::warn!(
                op = "reject",
                group = %group,
                name = %name,
                trigger = %trigger,
                reason = %format!("{field} template: {error}"),
            ),
            Note::MembershipUnreadable { group, error } => tracing::error!(
                op = "scan",
                group = %group,
                trigger = %trigger,
                result = "failed",
                error = %error,
                msg = "membership directory unreadable; group left unchanged this pass",
            ),
        }
    }
}

/// Logs planning findings.
pub fn log_findings(findings: &[Finding], trigger: Trigger) {
    for finding in findings {
        match finding {
            Finding::Conflict { key, target, kind } => tracing::warn!(
                op = "conflict",
                group = %key.group,
                name = %key.name,
                target = %target,
                trigger = %trigger,
                reason = match kind {
                    ConflictKind::ForeignMount =>
                        "mount present at target but not recorded in state; not touching foreign mount",
                    ConflictKind::TargetReplaced =>
                        "mount at target is not the recorded mount; not touching it",
                },
            ),
            Finding::SourceUnavailable {
                key,
                source,
                reason,
            } => tracing::warn!(
                op = "skip",
                group = %key.group,
                name = %key.name,
                source = %source,
                trigger = %trigger,
                reason = %reason,
            ),
            Finding::TargetUnavailable {
                key,
                target,
                reason,
            } => tracing::warn!(
                op = "skip",
                group = %key.group,
                name = %key.name,
                target = %target,
                trigger = %trigger,
                reason = %format!("target cannot be inspected: {reason}"),
            ),
            Finding::TargetCollision {
                target,
                members,
                kept,
            } => tracing::warn!(
                op = "conflict",
                target = %target,
                trigger = %trigger,
                members = %members.iter().map(ToString::to_string).collect::<Vec<_>>().join(","),
                kept = %kept.as_ref().map_or_else(|| "none".to_owned(), ToString::to_string),
                reason = "several members resolve to the same target",
            ),
            Finding::RelocationBlocked { key, old_target } => tracing::warn!(
                op = "skip",
                group = %key.group,
                name = %key.name,
                target = %old_target,
                trigger = %trigger,
                reason = "member moved but its existing mount cannot be verified; not relocating",
            ),
        }
    }
}
