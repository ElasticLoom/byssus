//! Pure reconciliation planning.
//!
//! The planner compares desired membership, the state file and kernel
//! observations, and returns the actions to take plus findings to report. It
//! performs no I/O, so every row of the decision table in `docs/DESIGN.md`
//! ("Reconciliation") is unit-tested here.
//!
//! Execution contract (implemented by the executor):
//!
//! - Steps run in order. Unmounts and record drops come first, then attribute
//!   re-application, then mounts.
//! - A step whose dependency failed or was skipped is skipped.
//! - `Unmount` re-verifies identity on a pinned descriptor before unmounting,
//!   and removes the record only on success.
//! - `Mount` writes (or replaces) the member's record on success.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::config::{AbsPath, MountAttrs};
use crate::identity::{DevIno, MountIdentity};
use crate::name::Name;
use crate::state::{MountRecord, RecordKey, State};

/// A path beneath a trusted root.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Location {
    /// Trusted root directory.
    pub root: AbsPath,
    /// Interpolated relative path beneath `root`.
    pub path: String,
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.root.as_path() == std::path::Path::new("/") {
            write!(f, "/{}", self.path)
        } else {
            write!(f, "{}/{}", self.root, self.path)
        }
    }
}

impl MountRecord {
    /// The recorded source location.
    #[must_use]
    pub fn source_location(&self) -> Location {
        Location {
            root: self.source_root.clone(),
            path: self.source.clone(),
        }
    }

    /// The recorded target location.
    #[must_use]
    pub fn target_location(&self) -> Location {
        Location {
            root: self.target_root.clone(),
            path: self.target.clone(),
        }
    }
}

/// A member that should be mounted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesiredMount {
    /// Group and member.
    pub key: RecordKey,
    /// Where the source is.
    pub source: Location,
    /// Where the view is mounted.
    pub target: Location,
    /// Configured attributes.
    pub attrs: MountAttrs,
}

/// Observed attributes of an attached mount.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct ObservedAttrs {
    /// `ST_RDONLY`.
    pub read_only: bool,
    /// `ST_NOSUID`.
    pub nosuid: bool,
    /// `ST_NODEV`.
    pub nodev: bool,
    /// `ST_NOEXEC`.
    pub noexec: bool,
    /// `ST_NOSYMFOLLOW`.
    pub nosymfollow: bool,
}

impl ObservedAttrs {
    /// Whether these observed attributes are exactly what `attrs` requires
    /// (with `nosuid` and `nodev` always required).
    #[must_use]
    pub fn satisfy(&self, attrs: &MountAttrs) -> bool {
        self.nosuid
            && self.nodev
            && self.read_only == attrs.read_only
            && self.noexec == attrs.noexec
            && self.nosymfollow == attrs.nosymfollow
    }
}

/// What was observed at a member's source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceState {
    /// The source resolved to a directory with this device and inode.
    Resolved(DevIno),
    /// The source could not be resolved.
    Unavailable(String),
}

/// What was observed at a target location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetState {
    /// No mount root at the target (it may not exist yet).
    NotMounted,
    /// A mount root is at the target.
    Mounted {
        /// Its identity.
        identity: MountIdentity,
        /// Its attributes, if they could be read.
        attrs: Option<ObservedAttrs>,
    },
    /// The target could not be inspected (for example, a symlink is in the
    /// way or access was denied).
    Unavailable(String),
}

/// Kernel observations gathered for planning.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Observations {
    /// Source state for each desired member.
    pub sources: BTreeMap<RecordKey, SourceState>,
    /// Target state for every desired and recorded target location.
    pub targets: BTreeMap<Location, TargetState>,
}

/// Inputs to [`plan`].
#[derive(Debug, Clone, Copy)]
pub struct PlanInput<'a> {
    /// Desired mounts across all non-degraded groups.
    pub desired: &'a [DesiredMount],
    /// Current state records.
    pub state: &'a State,
    /// Degraded groups: their records are left untouched.
    pub frozen_groups: &'a BTreeSet<Name>,
    /// Kernel observations.
    pub observations: &'a Observations,
}

/// Why a mount is being removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnmountReason {
    /// The member is no longer in the group (or the group was removed).
    NotMember,
    /// The member's configured location changed.
    Relocated,
    /// The source can no longer be resolved.
    SourceGone(String),
    /// The source now resolves to a different directory.
    SourceChanged,
    /// The member's target collides with another member's.
    TargetCollision,
}

/// Why a record is being dropped without unmounting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DropReason {
    /// Nothing is mounted at the recorded target any more.
    MountGone,
    /// A different mount is at the recorded target; it is not ours.
    ForeignMountAtTarget,
    /// Nothing is mounted and the source is unavailable, so it cannot be
    /// re-created.
    SourceUnavailable(String),
}

/// An action for the executor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Verify identity and unmount, then remove the record.
    Unmount {
        /// The record being removed.
        record: MountRecord,
        /// Why.
        reason: UnmountReason,
    },
    /// Remove a record without touching the mount table.
    DropRecord {
        /// The record being removed.
        record: MountRecord,
        /// Why.
        reason: DropReason,
    },
    /// Re-apply mount attributes to an existing mount of ours.
    Reattr {
        /// The mount's record.
        record: MountRecord,
        /// Attributes to apply.
        attrs: MountAttrs,
    },
    /// Create a mount and record it.
    Mount {
        /// What to mount.
        desired: DesiredMount,
    },
}

impl Action {
    const fn phase(&self) -> u8 {
        match self {
            Self::Unmount { .. } | Self::DropRecord { .. } => 0,
            Self::Reattr { .. } => 1,
            Self::Mount { .. } => 2,
        }
    }
}

/// A planned action with its dependencies (indices of earlier steps).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// The action.
    pub action: Action,
    /// Steps that must have succeeded first.
    pub depends_on: Vec<usize>,
}

/// Kind of conflict at a target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictKind {
    /// A mount not recorded by Byssus is at the target.
    ForeignMount,
    /// The mount at the target is not the one Byssus recorded.
    TargetReplaced,
}

/// Something to report that does not produce an action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Finding {
    /// A conflicting mount is at a member's target; it is left alone.
    Conflict {
        /// Member.
        key: RecordKey,
        /// Target.
        target: Location,
        /// Kind.
        kind: ConflictKind,
    },
    /// A member's source is unavailable, so it is not mounted.
    SourceUnavailable {
        /// Member.
        key: RecordKey,
        /// Source.
        source: Location,
        /// Reason.
        reason: String,
    },
    /// A member's target could not be inspected, so nothing is done.
    TargetUnavailable {
        /// Member.
        key: RecordKey,
        /// Target.
        target: Location,
        /// Reason.
        reason: String,
    },
    /// Several members resolve to one target; the listed ones are not mounted.
    TargetCollision {
        /// Target.
        target: Location,
        /// All members resolving to it.
        members: Vec<RecordKey>,
        /// The member kept because it is already mounted there, if any.
        kept: Option<RecordKey>,
    },
    /// A member cannot move to its new location because its existing mount
    /// could not be verified and removed.
    RelocationBlocked {
        /// Member.
        key: RecordKey,
        /// Recorded (old) target.
        old_target: Location,
    },
}

/// The result of planning.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Plan {
    /// Ordered steps.
    pub steps: Vec<Step>,
    /// Findings to report.
    pub findings: Vec<Finding>,
}

impl Plan {
    /// Whether the plan changes nothing.
    #[must_use]
    pub fn is_noop(&self) -> bool {
        self.steps.is_empty()
    }
}

fn record_matches_desired(record: &MountRecord, desired: &DesiredMount) -> bool {
    record.source_location() == desired.source && record.target_location() == desired.target
}

struct Builder<'a> {
    input: PlanInput<'a>,
    /// (action, dependencies as builder ids); the id is the vector index.
    steps: Vec<(Action, Vec<usize>)>,
    findings: Vec<Finding>,
}

impl<'a> Builder<'a> {
    fn push(&mut self, action: Action, depends_on: Vec<usize>) -> usize {
        self.steps.push((action, depends_on));
        self.steps.len() - 1
    }

    fn target(&self, location: &Location) -> TargetState {
        self.input
            .observations
            .targets
            .get(location)
            .cloned()
            .unwrap_or_else(|| TargetState::Unavailable("target was not observed".into()))
    }

    fn source(&self, key: &RecordKey) -> SourceState {
        self.input
            .observations
            .sources
            .get(key)
            .cloned()
            .unwrap_or_else(|| SourceState::Unavailable("source was not observed".into()))
    }

    /// Resolves target collisions. Returns the desired mounts to process.
    fn resolve_collisions(&mut self) -> BTreeMap<RecordKey, &'a DesiredMount> {
        let frozen = self.input.frozen_groups;
        let mut by_target: BTreeMap<&Location, Vec<&'a DesiredMount>> = BTreeMap::new();
        for d in self.input.desired {
            if !frozen.contains(&d.key.group) {
                by_target.entry(&d.target).or_default().push(d);
            }
        }
        let mut accepted = BTreeMap::new();
        for (target, mut members) in by_target {
            if members.len() == 1 {
                let d = members.remove(0);
                accepted.insert(d.key.clone(), d);
                continue;
            }
            members.sort_by(|a, b| a.key.cmp(&b.key));
            let observed = self.target(target);
            let kept = members.iter().copied().find(|d| {
                let Some(record) = self.input.state.get(&d.key) else {
                    return false;
                };
                record_matches_desired(record, d)
                    && matches!(&observed, TargetState::Mounted { identity, .. }
                        if identity.matches(&record.identity()))
            });
            if let Some(d) = kept {
                accepted.insert(d.key.clone(), d);
            }
            self.findings.push(Finding::TargetCollision {
                target: target.clone(),
                members: members.iter().map(|d| d.key.clone()).collect(),
                kept: kept.map(|d| d.key.clone()),
            });
        }
        accepted
    }

    fn finish(self) -> Plan {
        // Stable-sort by phase and remap dependency ids to final indices.
        let mut order: Vec<usize> = (0..self.steps.len()).collect();
        order.sort_by_key(|&id| self.steps[id].0.phase());
        let mut index_of = vec![0; self.steps.len()];
        for (index, &id) in order.iter().enumerate() {
            index_of[id] = index;
        }
        let mut slots: Vec<Option<(Action, Vec<usize>)>> =
            self.steps.into_iter().map(Some).collect();
        let steps = order
            .iter()
            .map(|&id| {
                let (action, deps) = slots[id]
                    .take()
                    .unwrap_or_else(|| unreachable!("step taken twice"));
                let mut depends_on: Vec<usize> = deps.into_iter().map(|d| index_of[d]).collect();
                depends_on.sort_unstable();
                depends_on.dedup();
                Step { action, depends_on }
            })
            .collect();
        Plan {
            steps,
            findings: self.findings,
        }
    }
}

/// Effects of phase A that phase B depends on.
#[derive(Default)]
struct Departures {
    /// Member key -> the step removing its old record, or `None` if the old
    /// record must be kept (so the member cannot be re-recorded elsewhere).
    old_record_removal: BTreeMap<RecordKey, Option<usize>>,
    /// Target locations freed by an unmount in this plan: step id and the
    /// identity being removed.
    freed_targets: BTreeMap<Location, (usize, MountIdentity)>,
}

/// Plans reconciliation. See the module documentation for the execution
/// contract.
#[must_use]
pub fn plan(input: PlanInput<'_>) -> Plan {
    let mut b = Builder {
        input,
        steps: Vec::new(),
        findings: Vec::new(),
    };
    let desired = b.resolve_collisions();
    let departures = plan_departing_records(&mut b, &desired);

    for (key, d) in &desired {
        let target = b.target(&d.target);
        let source = b.source(key);
        match input
            .state
            .get(key)
            .filter(|r| record_matches_desired(r, d))
        {
            Some(record) => plan_recorded_member(&mut b, d, record, target, source),
            None => plan_unrecorded_member(&mut b, d, &departures, target, source),
        }
    }

    b.finish()
}

/// Phase A: records not desired at their recorded location — decision-table
/// rows 10-12, which also cover relocations, collisions and removed groups.
fn plan_departing_records(
    b: &mut Builder<'_>,
    desired: &BTreeMap<RecordKey, &DesiredMount>,
) -> Departures {
    let input = b.input;
    let mut departures = Departures::default();
    for record in input.state.records() {
        if input.frozen_groups.contains(&record.group) {
            continue;
        }
        let key = record.key();
        let desired_here = desired.get(&key).copied();
        if desired_here.is_some_and(|d| record_matches_desired(record, d)) {
            continue; // handled in phase B
        }
        let collided = desired_here.is_none()
            && input
                .desired
                .iter()
                .any(|d| d.key == key && !input.frozen_groups.contains(&d.key.group));
        let unmount_reason = if collided {
            UnmountReason::TargetCollision
        } else if desired_here.is_some() {
            UnmountReason::Relocated
        } else {
            UnmountReason::NotMember
        };

        let location = record.target_location();
        let drop_record = |b: &mut Builder<'_>, reason| {
            b.push(
                Action::DropRecord {
                    record: record.clone(),
                    reason,
                },
                vec![],
            )
        };
        let removal = match b.target(&location) {
            TargetState::NotMounted => Some(drop_record(b, DropReason::MountGone)),
            TargetState::Mounted { identity, .. } if identity.matches(&record.identity()) => {
                let id = b.push(
                    Action::Unmount {
                        record: record.clone(),
                        reason: unmount_reason,
                    },
                    vec![],
                );
                departures.freed_targets.insert(location, (id, identity));
                Some(id)
            }
            TargetState::Mounted { .. } => Some(drop_record(b, DropReason::ForeignMountAtTarget)),
            TargetState::Unavailable(reason) => {
                b.findings.push(Finding::TargetUnavailable {
                    key: key.clone(),
                    target: location,
                    reason,
                });
                None
            }
        };
        departures.old_record_removal.insert(key, removal);
    }
    departures
}

/// Rows 1-3: the member has no record at its desired location.
fn plan_unrecorded_member(
    b: &mut Builder<'_>,
    d: &DesiredMount,
    departures: &Departures,
    target: TargetState,
    source: SourceState,
) {
    let mut deps = Vec::new();
    match departures.old_record_removal.get(&d.key) {
        None => {}
        Some(Some(id)) => deps.push(*id),
        Some(None) => {
            let old_target = b
                .input
                .state
                .get(&d.key)
                .map_or_else(|| d.target.clone(), MountRecord::target_location);
            b.findings.push(Finding::RelocationBlocked {
                key: d.key.clone(),
                old_target,
            });
            return;
        }
    }
    match target {
        TargetState::NotMounted => {}
        TargetState::Mounted { identity, .. } => {
            let freed_by = departures
                .freed_targets
                .get(&d.target)
                .filter(|(_, freed)| *freed == identity)
                .map(|(id, _)| *id);
            let Some(id) = freed_by else {
                b.findings.push(Finding::Conflict {
                    key: d.key.clone(),
                    target: d.target.clone(),
                    kind: ConflictKind::ForeignMount,
                });
                return;
            };
            deps.push(id);
        }
        TargetState::Unavailable(reason) => {
            b.findings.push(Finding::TargetUnavailable {
                key: d.key.clone(),
                target: d.target.clone(),
                reason,
            });
            return;
        }
    }
    match source {
        SourceState::Resolved(_) => {
            b.push(Action::Mount { desired: d.clone() }, deps);
        }
        SourceState::Unavailable(reason) => b.findings.push(Finding::SourceUnavailable {
            key: d.key.clone(),
            source: d.source.clone(),
            reason,
        }),
    }
}

/// Rows 4-9: the member has a record at its desired location.
fn plan_recorded_member(
    b: &mut Builder<'_>,
    d: &DesiredMount,
    record: &MountRecord,
    target: TargetState,
    source: SourceState,
) {
    match target {
        TargetState::Mounted { identity, attrs } if identity.matches(&record.identity()) => {
            match source {
                // Row 4: ours; fix attributes if needed.
                SourceState::Resolved(root) if root == record.identity().root => {
                    if attrs.is_some_and(|a| !a.satisfy(&d.attrs)) {
                        b.push(
                            Action::Reattr {
                                record: record.clone(),
                                attrs: d.attrs,
                            },
                            vec![],
                        );
                    }
                }
                // Row 5: source replaced; remount.
                SourceState::Resolved(_) => {
                    let id = b.push(
                        Action::Unmount {
                            record: record.clone(),
                            reason: UnmountReason::SourceChanged,
                        },
                        vec![],
                    );
                    b.push(Action::Mount { desired: d.clone() }, vec![id]);
                }
                // Row 6: source gone; unmount.
                SourceState::Unavailable(reason) => {
                    b.push(
                        Action::Unmount {
                            record: record.clone(),
                            reason: UnmountReason::SourceGone(reason),
                        },
                        vec![],
                    );
                }
            }
        }
        // Row 7: something else is mounted at our target.
        TargetState::Mounted { .. } => b.findings.push(Finding::Conflict {
            key: d.key.clone(),
            target: d.target.clone(),
            kind: ConflictKind::TargetReplaced,
        }),
        TargetState::NotMounted => match source {
            // Row 8: removed externally; re-create.
            SourceState::Resolved(_) => {
                b.push(Action::Mount { desired: d.clone() }, vec![]);
            }
            // Row 9: cannot re-create; drop the stale record.
            SourceState::Unavailable(reason) => {
                b.push(
                    Action::DropRecord {
                        record: record.clone(),
                        reason: DropReason::SourceUnavailable(reason.clone()),
                    },
                    vec![],
                );
                b.findings.push(Finding::SourceUnavailable {
                    key: d.key.clone(),
                    source: d.source.clone(),
                    reason,
                });
            }
        },
        TargetState::Unavailable(reason) => b.findings.push(Finding::TargetUnavailable {
            key: d.key.clone(),
            target: d.target.clone(),
            reason,
        }),
    }
}

#[cfg(test)]
mod tests;
