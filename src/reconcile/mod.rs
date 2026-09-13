//! Reconciliation of desired membership against state and the kernel.
//!
//! A pass is: [`observe`] → [`plan`] → report findings → [`execute`].

pub mod execute;
pub mod observe;
pub mod plan;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::name::GroupId;
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
///
/// `notes` remembers which observation notes were already logged, so a
/// persistent problem (such as a rejected membership file) is logged once
/// rather than on every pass.
pub fn run_pass<S: StateSink>(
    runtime: &mut Runtime,
    state: &mut State,
    sink: &S,
    degraded: &BTreeSet<GroupId>,
    unique_supported: bool,
    trigger: Trigger,
    notes: &mut NoteLog,
) -> Pass {
    let observed = observe::observe(runtime, state, degraded, unique_supported);
    notes.report(&observed, trigger);
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

/// A change in the set of active observation notes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoteEvent<'a> {
    /// A note that is new, or whose detail changed.
    Raised(&'a Note),
    /// A previously reported note no longer applies.
    Cleared {
        /// Group.
        group: String,
        /// Entry name, or empty for a group-level note.
        subject: String,
    },
}

/// Tracks reported observation notes across passes so each is logged once.
#[derive(Debug, Default)]
pub struct NoteLog {
    active: BTreeMap<(String, String), String>,
}

fn note_identity(note: &Note) -> ((String, String), String) {
    match note {
        Note::Rejected { group, rejection } => (
            (group.to_string(), rejection.display_name.clone()),
            rejection.reason.to_string(),
        ),
        Note::InterpolationFailed {
            group,
            name,
            field,
            error,
        } => (
            (group.to_string(), name.to_string()),
            format!("{field} template: {error}"),
        ),
        Note::MembershipDeleted { group } => ((group.to_string(), String::new()), "deleted".into()),
        Note::MembershipUnreadable { group, error } => {
            ((group.to_string(), String::new()), error.clone())
        }
    }
}

impl NoteLog {
    /// Updates the active set from a pass and returns what changed. Notes of
    /// groups that were not examined in this pass are left as they were.
    pub fn update<'a>(&mut self, observed: &'a Observed) -> Vec<NoteEvent<'a>> {
        let mut events = Vec::new();
        let mut current = BTreeMap::new();
        let mut examined: BTreeSet<String> =
            observed.members.keys().map(ToString::to_string).collect();
        for note in &observed.notes {
            let (key, detail) = note_identity(note);
            examined.insert(key.0.clone());
            if self.active.get(&key) != Some(&detail) {
                events.push(NoteEvent::Raised(note));
            }
            current.insert(key, detail);
        }
        for (key, detail) in std::mem::take(&mut self.active) {
            if current.contains_key(&key) {
                continue;
            }
            if examined.contains(&key.0) {
                events.push(NoteEvent::Cleared {
                    group: key.0,
                    subject: key.1,
                });
            } else {
                current.insert(key, detail);
            }
        }
        self.active = current;
        events
    }

    /// Updates the active set and logs the changes, plus ignored hidden
    /// entries at debug level.
    pub fn report(&mut self, observed: &Observed, trigger: Trigger) {
        for event in self.update(observed) {
            match event {
                NoteEvent::Raised(note) => log_note(note, trigger),
                NoteEvent::Cleared { group, subject } if subject.is_empty() => tracing::info!(
                    op = "scan",
                    group = %group,
                    trigger = %trigger,
                    result = "ok",
                    msg = "membership directory readable again",
                ),
                NoteEvent::Cleared { group, subject } => tracing::info!(
                    op = "reject_cleared",
                    group = %group,
                    name = %subject,
                    trigger = %trigger,
                    msg = "membership entry is no longer rejected",
                ),
            }
        }
        for (group, names) in &observed.ignored {
            tracing::debug!(
                op = "ignore",
                group = %group,
                trigger = %trigger,
                count = names.len(),
                names = %names.join(","),
                msg = "hidden membership entries ignored",
            );
        }
    }
}

fn log_note(note: &Note, trigger: Trigger) {
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
        Note::MembershipDeleted { group } => tracing::error!(
            op = "degrade",
            group = %group,
            trigger = %trigger,
            msg = "membership directory has been deleted; keeping existing mounts and making no changes to this group until configuration is reloaded",
        ),
        Note::MembershipUnreadable { group, error } => tracing::error!(
            op = "scan",
            group = %group,
            trigger = %trigger,
            result = "failed",
            error = %error,
            msg = "membership directory unreadable; group left unchanged",
        ),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::membership::{EntryKind, RejectReason, Rejection};

    fn name(s: &str) -> GroupId {
        GroupId::parse(s).unwrap()
    }

    fn rejected(group: &str, entry: &str, reason: RejectReason) -> Note {
        Note::Rejected {
            group: name(group),
            rejection: Rejection {
                display_name: entry.into(),
                reason,
            },
        }
    }

    fn observed(scanned: &[&str], notes: Vec<Note>) -> Observed {
        Observed {
            members: scanned.iter().map(|g| (name(g), BTreeSet::new())).collect(),
            notes,
            ..Observed::default()
        }
    }

    fn summarize(events: &[NoteEvent<'_>]) -> Vec<String> {
        events
            .iter()
            .map(|e| match e {
                NoteEvent::Raised(n) => format!("raised {}", note_identity(n).0.1),
                NoteEvent::Cleared { subject, .. } => format!("cleared {subject}"),
            })
            .collect()
    }

    #[test]
    fn persistent_notes_are_reported_once() {
        let mut log = NoteLog::default();
        let symlink = || {
            rejected(
                "g",
                "link",
                RejectReason::NotRegularFile(EntryKind::Symlink),
            )
        };

        let first = observed(&["g"], vec![symlink()]);
        assert_eq!(summarize(&log.update(&first)), ["raised link"]);
        let again = observed(&["g"], vec![symlink()]);
        assert!(log.update(&again).is_empty());

        // A changed reason is reported again.
        let changed = observed(
            &["g"],
            vec![rejected("g", "link", RejectReason::NotEmpty(3))],
        );
        assert_eq!(summarize(&log.update(&changed)), ["raised link"]);

        // Fixed: reported as cleared once.
        let fixed = observed(&["g"], vec![]);
        assert_eq!(summarize(&log.update(&fixed)), ["cleared link"]);
        assert!(log.update(&observed(&["g"], vec![])).is_empty());
    }

    #[test]
    fn unexamined_groups_keep_their_notes() {
        let mut log = NoteLog::default();
        let unreadable = || Note::MembershipUnreadable {
            group: name("g"),
            error: "EACCES".into(),
        };
        assert_eq!(log.update(&observed(&[], vec![unreadable()])).len(), 1);
        assert!(log.update(&observed(&[], vec![unreadable()])).is_empty());
        // Group not examined at all (e.g. degraded): nothing cleared.
        assert!(log.update(&observed(&[], vec![])).is_empty());
        // Scanned successfully again: cleared.
        assert_eq!(
            summarize(&log.update(&observed(&["g"], vec![]))),
            ["cleared "]
        );
    }
}
