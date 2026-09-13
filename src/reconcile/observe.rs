//! Gathering desired membership and kernel observations for planning.

use std::collections::{BTreeMap, BTreeSet};
use std::os::fd::AsFd;

use crate::fsops;
use crate::membership::Rejection;
use crate::mount;
use crate::name::{GroupId, Name};
use crate::reconcile::plan::{DesiredMount, Location, Observations, SourceState, TargetState};
use crate::runtime::Runtime;
use crate::state::{RecordKey, State};
use crate::template::InterpolateError;

/// Something noticed while observing that is reported but not planned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Note {
    /// A membership entry was rejected.
    Rejected {
        /// Group.
        group: GroupId,
        /// The rejection.
        rejection: Rejection,
    },
    /// A member's name could not be interpolated into a template.
    InterpolationFailed {
        /// Group.
        group: GroupId,
        /// Member.
        name: Name,
        /// `source` or `target`.
        field: &'static str,
        /// Why.
        error: InterpolateError,
    },
    /// An entry in a group set's membership root is not a valid group.
    SetEntryRejected {
        /// Set.
        set: Name,
        /// The rejection.
        rejection: Rejection,
    },
    /// A group set's membership root has been deleted; the set is frozen.
    SetRootDeleted {
        /// Set.
        set: Name,
    },
    /// A group set's membership root could not be read; the set is left
    /// unchanged this pass.
    SetRootUnreadable {
        /// Set.
        set: Name,
        /// Why.
        error: String,
    },
    /// The membership directory has been deleted; the group is frozen.
    MembershipDeleted {
        /// Group.
        group: GroupId,
    },
    /// The membership directory could not be read; the group is frozen for
    /// this pass.
    MembershipUnreadable {
        /// Group.
        group: GroupId,
        /// Why.
        error: String,
    },
}

/// Everything the planner needs, plus notes for reporting.
#[derive(Debug, Default)]
pub struct Observed {
    /// Desired mounts of groups that were scanned successfully.
    pub desired: Vec<DesiredMount>,
    /// Kernel observations.
    pub observations: Observations,
    /// Groups whose records must not be touched this pass.
    pub frozen: BTreeSet<GroupId>,
    /// Valid members per scanned group.
    pub members: BTreeMap<GroupId, BTreeSet<Name>>,
    /// Group sets whose membership root was scanned successfully.
    pub scanned_sets: BTreeSet<Name>,
    /// Hidden membership entries ignored per scanned group.
    pub ignored: BTreeMap<GroupId, Vec<String>>,
    /// Notes to report.
    pub notes: Vec<Note>,
}

/// Scans membership, resolves sources and inspects every relevant target.
///
/// `degraded` groups are not scanned and are frozen.
pub fn observe(
    runtime: &mut Runtime,
    state: &State,
    degraded: &BTreeSet<GroupId>,
    unique_supported: bool,
) -> Observed {
    let mut out = Observed {
        frozen: degraded.clone(),
        ..Observed::default()
    };

    for (group_name, group) in &runtime.groups {
        if degraded.contains(group_name) {
            continue;
        }
        if fsops::is_deleted(group.membership_dir.as_fd()).unwrap_or(false) {
            out.frozen.insert(group_name.clone());
            out.notes.push(Note::MembershipDeleted {
                group: group_name.clone(),
            });
            continue;
        }
        let mut membership = match fsops::scan_membership(group.membership_dir.as_fd()) {
            Ok(m) => m,
            Err(e) => {
                out.frozen.insert(group_name.clone());
                out.notes.push(Note::MembershipUnreadable {
                    group: group_name.clone(),
                    error: e.to_string(),
                });
                continue;
            }
        };
        if !membership.ignored.is_empty() {
            out.ignored
                .insert(group_name.clone(), std::mem::take(&mut membership.ignored));
        }
        out.notes.extend(
            membership
                .rejected
                .into_iter()
                .map(|rejection| Note::Rejected {
                    group: group_name.clone(),
                    rejection,
                }),
        );

        let cfg = &group.config;
        for name in &membership.members {
            let interpolate = |field, template: &crate::template::Template| {
                template
                    .interpolate(name)
                    .map_err(|error| Note::InterpolationFailed {
                        group: group_name.clone(),
                        name: name.clone(),
                        field,
                        error,
                    })
            };
            let (source, target) = match (
                interpolate("source", &cfg.source),
                interpolate("target", &cfg.target),
            ) {
                (Ok(s), Ok(t)) => (s, t),
                (Err(note), _) | (_, Err(note)) => {
                    out.notes.push(note);
                    continue;
                }
            };
            out.desired.push(DesiredMount {
                key: RecordKey {
                    group: group_name.clone(),
                    name: name.clone(),
                },
                source: Location {
                    root: cfg.source_root.clone(),
                    path: source,
                },
                target: Location {
                    root: cfg.target_root.clone(),
                    path: target,
                },
                attrs: cfg.attrs,
            });
        }
        out.members.insert(group_name.clone(), membership.members);
    }

    observe_kernel(runtime, state, unique_supported, &mut out);

    out
}

/// Resolves every desired source and inspects every desired and recorded
/// target.
fn observe_kernel(
    runtime: &mut Runtime,
    state: &State,
    unique_supported: bool,
    out: &mut Observed,
) {
    for d in &out.desired {
        let source = match runtime.roots.get(&d.source.root) {
            Ok(root) => match fsops::resolve_dir(root, &d.source.path) {
                Ok(fd) => match fsops::dev_ino(fd.as_fd()) {
                    Ok(id) => SourceState::Resolved(id),
                    Err(e) => SourceState::Unavailable(format!("{}: {e}", d.source)),
                },
                Err(e) => SourceState::Unavailable(format!("{}: {e}", d.source)),
            },
            Err(e) => SourceState::Unavailable(format!("source root {}: {e}", d.source.root)),
        };
        out.observations.sources.insert(d.key.clone(), source);
    }

    let mut targets: BTreeSet<Location> = out.desired.iter().map(|d| d.target.clone()).collect();
    targets.extend(
        state
            .records()
            .filter(|r| !out.frozen.contains(&r.group))
            .map(crate::state::MountRecord::target_location),
    );
    for location in targets {
        let observed = match runtime.roots.get(&location.root) {
            Ok(root) => mount::inspect(root, &location.path, unique_supported),
            Err(e) => TargetState::Unavailable(format!("target root {}: {e}", location.root)),
        };
        out.observations.targets.insert(location, observed);
    }
}
