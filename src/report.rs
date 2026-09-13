//! Human- and machine-readable reports for `byssus status` and
//! `byssus dry-run`.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::Serialize;

use crate::name::Name;
use crate::reconcile::observe::{Note, Observed};
use crate::reconcile::plan::{Action, ConflictKind, Finding, Plan, SourceState, UnmountReason};
use crate::state::{RecordKey, State};

/// Severity of a check or group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    /// Fine.
    Ok,
    /// Works, but something needs attention.
    Warn,
    /// Does not work.
    Error,
}

impl Level {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

/// One environment or configuration check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Check {
    /// Dotted check name, e.g. `kernel.openat2`.
    pub name: String,
    /// Result.
    pub level: Level,
    /// Explanation.
    pub detail: String,
}

impl Check {
    /// A new check.
    pub fn new(name: impl Into<String>, level: Level, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            level,
            detail: detail.into(),
        }
    }
}

/// The state of one member.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberState {
    /// Mounted and verified as ours.
    Mounted,
    /// Would be (or will be) mounted.
    WouldMount,
    /// Would be unmounted and mounted again.
    WouldRemount,
    /// Would be unmounted.
    WouldUnmount,
    /// A stale state record would be removed.
    StaleRecord,
    /// A conflicting mount is at the target.
    Conflict,
    /// Something is mounted at the target but ownership cannot be verified
    /// because the state file is unreadable.
    MountedOwnershipUnknown,
    /// The source cannot be resolved.
    SourceUnavailable,
    /// The target cannot be inspected.
    TargetUnavailable,
    /// Several members resolve to the same target.
    Collision,
    /// A location change is blocked.
    RelocationBlocked,
    /// The group could not be inspected.
    GroupUnavailable,
    /// A membership entry was rejected (bad name, not an empty regular
    /// file, or an unusable template interpolation).
    Rejected,
}

impl MemberState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Mounted => "mounted",
            Self::WouldMount => "would_mount",
            Self::WouldRemount => "would_remount",
            Self::WouldUnmount => "would_unmount",
            Self::StaleRecord => "stale_record",
            Self::Conflict => "conflict",
            Self::MountedOwnershipUnknown => "mounted_ownership_unknown",
            Self::SourceUnavailable => "source_unavailable",
            Self::TargetUnavailable => "target_unavailable",
            Self::Collision => "collision",
            Self::RelocationBlocked => "relocation_blocked",
            Self::GroupUnavailable => "group_unavailable",
            Self::Rejected => "rejected",
        }
    }

    const fn level(self) -> Level {
        match self {
            Self::Mounted
            | Self::WouldMount
            | Self::WouldRemount
            | Self::WouldUnmount
            | Self::StaleRecord
            | Self::MountedOwnershipUnknown => Level::Ok,
            Self::SourceUnavailable | Self::RelocationBlocked | Self::Rejected => Level::Warn,
            Self::Conflict | Self::TargetUnavailable | Self::Collision | Self::GroupUnavailable => {
                Level::Error
            }
        }
    }
}

/// One member's status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MemberStatus {
    /// Group.
    pub group: String,
    /// Member.
    pub name: String,
    /// Target path (absent for rejected membership entries).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// State.
    pub state: MemberState,
    /// Extra detail.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Derives member statuses from a planned (not executed) pass.
#[must_use]
pub fn member_statuses(
    observed: &Observed,
    plan: &Plan,
    state: &State,
    ownership_known: bool,
) -> Vec<MemberStatus> {
    let mut map: BTreeMap<RecordKey, MemberStatus> = BTreeMap::new();

    for d in &observed.desired {
        set(
            &mut map,
            &d.key,
            d.target.to_string(),
            MemberState::Mounted,
            None,
        );
    }
    for record in state.records() {
        if observed.frozen.contains(&record.group) {
            set(
                &mut map,
                &record.key(),
                record.target_location().to_string(),
                MemberState::GroupUnavailable,
                None,
            );
        }
    }

    let mut unmounting: Vec<RecordKey> = Vec::new();
    for step in &plan.steps {
        match &step.action {
            Action::Unmount { record, reason } => {
                unmounting.push(record.key());
                let detail = match reason {
                    UnmountReason::SourceGone(e) => Some(e.clone()),
                    _ => None,
                };
                set(
                    &mut map,
                    &record.key(),
                    record.target_location().to_string(),
                    MemberState::WouldUnmount,
                    detail,
                );
            }
            Action::DropRecord { record, .. } => set(
                &mut map,
                &record.key(),
                record.target_location().to_string(),
                MemberState::StaleRecord,
                None,
            ),
            Action::Mount { desired } => {
                let state = if unmounting.contains(&desired.key) {
                    MemberState::WouldRemount
                } else {
                    MemberState::WouldMount
                };
                set(
                    &mut map,
                    &desired.key,
                    desired.target.to_string(),
                    state,
                    None,
                );
            }
        }
    }

    apply_findings(&mut map, &plan.findings, ownership_known);

    let mut statuses: Vec<MemberStatus> = map.into_values().collect();
    for note in &observed.notes {
        let (group, name, detail) = match note {
            Note::Rejected { group, rejection } => (
                group.to_string(),
                rejection.display_name.clone(),
                rejection.reason.to_string(),
            ),
            Note::InterpolationFailed {
                group,
                name,
                field,
                error,
            } => (
                group.to_string(),
                name.to_string(),
                format!("{field} template: {error}"),
            ),
            Note::MembershipDeleted { .. } | Note::MembershipUnreadable { .. } => continue,
        };
        statuses.push(MemberStatus {
            group,
            name,
            target: None,
            state: MemberState::Rejected,
            detail: Some(detail),
        });
    }
    statuses.sort_by(|a, b| (&a.group, &a.name).cmp(&(&b.group, &b.name)));
    statuses
}

fn apply_findings(
    map: &mut BTreeMap<RecordKey, MemberStatus>,
    findings: &[Finding],
    ownership_known: bool,
) {
    for finding in findings {
        match finding {
            Finding::Conflict { key, target, kind } => {
                let state = if !ownership_known && *kind == ConflictKind::ForeignMount {
                    MemberState::MountedOwnershipUnknown
                } else {
                    MemberState::Conflict
                };
                set(map, key, target.to_string(), state, None);
            }
            Finding::SourceUnavailable {
                key,
                source,
                reason,
            } => {
                let target = map
                    .get(key)
                    .and_then(|m| m.target.clone())
                    .unwrap_or_else(|| source.to_string());
                set(
                    map,
                    key,
                    target,
                    MemberState::SourceUnavailable,
                    Some(reason.clone()),
                );
            }
            Finding::TargetUnavailable {
                key,
                target,
                reason,
            } => set(
                map,
                key,
                target.to_string(),
                MemberState::TargetUnavailable,
                Some(reason.clone()),
            ),
            Finding::TargetCollision {
                target,
                members,
                kept,
            } => {
                for key in members.iter().filter(|k| Some(*k) != kept.as_ref()) {
                    set(
                        map,
                        key,
                        target.to_string(),
                        MemberState::Collision,
                        kept.as_ref().map(|k| format!("target kept by {k}")),
                    );
                }
            }
            Finding::RelocationBlocked { key, old_target } => set(
                map,
                key,
                old_target.to_string(),
                MemberState::RelocationBlocked,
                None,
            ),
        }
    }
}

fn set(
    map: &mut BTreeMap<RecordKey, MemberStatus>,
    key: &RecordKey,
    target: String,
    state: MemberState,
    detail: Option<String>,
) {
    map.insert(
        key.clone(),
        MemberStatus {
            group: key.group.to_string(),
            name: key.name.to_string(),
            target: Some(target),
            state,
            detail,
        },
    );
}

/// Per-group summary for `dry-run`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GroupReport {
    /// Group name.
    pub name: String,
    /// Overall level.
    pub status: Level,
    /// Membership directory path and whether it is readable.
    pub membership_dir: String,
    /// Target root propagation.
    pub propagation: String,
    /// Valid members.
    pub members: usize,
    /// Rejected membership entries.
    pub rejected: usize,
    /// Hidden membership entries ignored.
    pub ignored: usize,
    /// Members whose source resolves.
    pub sources_valid: usize,
    /// Members whose source does not resolve.
    pub sources_missing: usize,
    /// Verified existing mounts.
    pub existing_mounts: usize,
    /// Mounts a reconcile would create (including remounts).
    pub would_create: usize,
    /// Mounts a reconcile would remove (including remounts).
    pub would_remove: usize,
    /// Conflicts and collisions.
    pub conflicts: usize,
    /// Group-level problems.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub problems: Vec<String>,
}

/// Builds per-group summaries. `propagation` and `problems` are supplied by
/// the caller per group, with their levels.
#[must_use]
pub fn group_reports(
    groups: &[(Name, String)],
    observed: &Observed,
    plan: &Plan,
    statuses: &[MemberStatus],
    extra: &BTreeMap<Name, (String, Level, Vec<String>)>,
) -> Vec<GroupReport> {
    groups
        .iter()
        .map(|(name, membership_path)| {
            let in_group = |g: &Name| g == name;
            let members = observed.members.get(name).map_or(0, std::collections::BTreeSet::len);
            let rejected = observed
                .notes
                .iter()
                .filter(|n| matches!(n, Note::Rejected { group, .. } | Note::InterpolationFailed { group, .. } if in_group(group)))
                .count();
            let unreadable = observed
                .notes
                .iter()
                .find_map(|n| match n {
                    Note::MembershipUnreadable { group, error } if in_group(group) => Some(error.clone()),
                    Note::MembershipDeleted { group } if in_group(group) => Some("directory has been deleted".to_owned()),
                    _ => None,
                });
            let mut sources_valid = 0;
            let mut sources_missing = 0;
            for d in observed.desired.iter().filter(|d| in_group(&d.key.group)) {
                match observed.observations.sources.get(&d.key) {
                    Some(SourceState::Resolved(_)) => sources_valid += 1,
                    _ => sources_missing += 1,
                }
            }
            let mut would_create = 0;
            let mut would_remove = 0;
            for step in &plan.steps {
                match &step.action {
                    Action::Mount { desired } if in_group(&desired.key.group) => would_create += 1,
                    Action::Unmount { record, .. } if in_group(&record.group) => would_remove += 1,
                    _ => {}
                }
            }
            let group_statuses = statuses.iter().filter(|s| s.group == name.as_str());
            let existing_mounts = group_statuses
                .clone()
                .filter(|s| matches!(s.state, MemberState::Mounted | MemberState::WouldRemount))
                .count();
            let conflicts = group_statuses
                .clone()
                .filter(|s| matches!(s.state, MemberState::Conflict | MemberState::Collision))
                .count();
            let member_level = group_statuses.map(|s| s.state.level()).max().unwrap_or(Level::Ok);

            let (propagation, prop_level, mut problems) = extra
                .get(name)
                .cloned()
                .unwrap_or_else(|| ("unknown".into(), Level::Warn, Vec::new()));
            let membership_dir = match &unreadable {
                None if observed.members.contains_key(name) => format!("{membership_path} (readable)"),
                None => format!("{membership_path} (not inspected)"),
                Some(error) => {
                    problems.push(format!("membership directory unreadable: {error}"));
                    format!("{membership_path} (unreadable)")
                }
            };
            let mut status = member_level.max(prop_level);
            if unreadable.is_some() || !observed.members.contains_key(name) {
                status = Level::Error;
            }
            if rejected > 0 {
                status = status.max(Level::Warn);
            }
            GroupReport {
                name: name.to_string(),
                status,
                membership_dir,
                propagation,
                members,
                rejected,
                ignored: observed.ignored.get(name).map_or(0, Vec::len),
                sources_valid,
                sources_missing,
                existing_mounts,
                would_create,
                would_remove,
                conflicts,
                problems,
            }
        })
        .collect()
}

/// The `dry-run` report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DryRunReport {
    /// Environment and configuration checks.
    pub checks: Vec<Check>,
    /// Per-group summaries.
    pub groups: Vec<GroupReport>,
    /// Per-member detail.
    pub members: Vec<MemberStatus>,
}

impl DryRunReport {
    /// The worst level in the report.
    #[must_use]
    pub fn level(&self) -> Level {
        self.checks
            .iter()
            .map(|c| c.level)
            .chain(self.groups.iter().map(|g| g.status))
            .max()
            .unwrap_or(Level::Ok)
    }

    /// Renders as aligned `key = value` text.
    #[must_use]
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        let width = self.checks.iter().map(|c| c.name.len()).max().unwrap_or(0);
        for check in &self.checks {
            let _ = write!(out, "{:width$} = {}", check.name, check.level.as_str());
            if !check.detail.is_empty() {
                let _ = write!(out, " ({})", check.detail);
            }
            out.push('\n');
        }
        for group in &self.groups {
            let _ = writeln!(
                out,
                "\ngroup={}  status={}",
                group.name,
                group.status.as_str()
            );
            let rows: [(&str, String); 12] = [
                ("membership_dir", group.membership_dir.clone()),
                ("propagation", group.propagation.clone()),
                ("members", group.members.to_string()),
                ("rejected", group.rejected.to_string()),
                ("ignored", group.ignored.to_string()),
                ("sources_valid", group.sources_valid.to_string()),
                ("sources_missing", group.sources_missing.to_string()),
                ("existing_mounts", group.existing_mounts.to_string()),
                ("would_create", group.would_create.to_string()),
                ("would_remove", group.would_remove.to_string()),
                ("conflicts", group.conflicts.to_string()),
                ("problems", group.problems.join("; ")),
            ];
            for (key, value) in rows {
                if key == "problems" && value.is_empty() {
                    continue;
                }
                let _ = writeln!(out, "  {key:16} = {value}");
            }
        }
        render_members(&mut out, &self.members, |s| s.state != MemberState::Mounted);
        out
    }
}

/// The `status` report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StatusReport {
    /// State file path.
    pub state_file: String,
    /// Whether the state file could be read.
    pub ownership_known: bool,
    /// Notes (for example why the state file could not be read).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// Number of hidden membership entries ignored, per group.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub ignored: BTreeMap<String, usize>,
    /// Every desired or recorded member, and rejected membership entries.
    pub members: Vec<MemberStatus>,
}

impl StatusReport {
    /// The worst member level.
    #[must_use]
    pub fn level(&self) -> Level {
        self.members
            .iter()
            .map(|m| m.state.level())
            .max()
            .unwrap_or(Level::Ok)
    }

    /// Renders as text.
    #[must_use]
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "state_file = {}{}",
            self.state_file,
            if self.ownership_known {
                ""
            } else {
                " (unreadable: ownership unknown)"
            }
        );
        for note in &self.notes {
            let _ = writeln!(out, "note: {note}");
        }
        for (group, count) in &self.ignored {
            let _ = writeln!(
                out,
                "ignored: group '{group}': {count} hidden entr{}",
                if *count == 1 { "y" } else { "ies" }
            );
        }
        if self.members.is_empty() {
            out.push_str("\nno members\n");
        }
        render_members(&mut out, &self.members, |_| true);
        out
    }
}

fn render_members(
    out: &mut String,
    members: &[MemberStatus],
    include: impl Fn(&MemberStatus) -> bool,
) {
    let shown: Vec<&MemberStatus> = members.iter().filter(|m| include(m)).collect();
    if shown.is_empty() {
        return;
    }
    out.push('\n');
    for m in shown {
        let _ = write!(out, "{}/{}  state={}", m.group, m.name, m.state.as_str());
        if let Some(target) = &m.target {
            let _ = write!(out, "  target={}", crate::logging::quote(target));
        }
        if let Some(detail) = &m.detail {
            let _ = write!(out, "  detail={}", crate::logging::quote(detail));
        }
        out.push('\n');
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::config::{AbsPath, MountAttrs};
    use crate::identity::{DevIno, MountIdentity};
    use crate::reconcile::plan::{
        self, DesiredMount, Location, Observations, PlanInput, TargetState,
    };
    use crate::state::MountRecord;

    fn key(g: &str, n: &str) -> RecordKey {
        RecordKey {
            group: Name::new(g).unwrap(),
            name: Name::new(n).unwrap(),
        }
    }

    fn desired(g: &str, n: &str) -> DesiredMount {
        DesiredMount {
            key: key(g, n),
            source: Location {
                root: AbsPath::new("/src").unwrap(),
                path: format!("{n}/workspace"),
            },
            target: Location {
                root: AbsPath::new("/view").unwrap(),
                path: n.into(),
            },
            attrs: MountAttrs::default(),
        }
    }

    fn record(d: &DesiredMount, mnt_id: u64) -> MountRecord {
        MountRecord {
            group: d.key.group.clone(),
            name: d.key.name.clone(),
            source_root: d.source.root.clone(),
            source: d.source.path.clone(),
            target_root: d.target.root.clone(),
            target: d.target.path.clone(),
            mnt_id,
            mnt_id_unique: None,
            root_dev_major: 8,
            root_dev_minor: 1,
            root_ino: 1,
            read_only: true,
            noexec: true,
            nosymfollow: false,
            created_at: "2026-09-12T00:00:00Z".parse().unwrap(),
        }
    }

    fn identity(mnt_id: u64) -> MountIdentity {
        MountIdentity {
            mnt_id,
            mnt_id_unique: None,
            root: DevIno {
                dev_major: 8,
                dev_minor: 1,
                ino: 1,
            },
        }
    }

    /// a: mounted; b: would mount; c: foreign conflict; d: source missing;
    /// gone: recorded but not desired.
    fn scenario() -> (Observed, State) {
        let a = desired("g", "a");
        let b = desired("g", "b");
        let c = desired("g", "c");
        let d = desired("g", "d");
        let gone = desired("g", "gone");
        let mut state = State::default();
        state.insert(record(&a, 1));
        state.insert(record(&gone, 2));
        let mut obs = Observations::default();
        let resolved = SourceState::Resolved(DevIno {
            dev_major: 8,
            dev_minor: 1,
            ino: 1,
        });
        for x in [&a, &b, &c] {
            obs.sources.insert(x.key.clone(), resolved.clone());
        }
        obs.sources
            .insert(d.key.clone(), SourceState::Unavailable("ENOENT".into()));
        let mounted = |id| TargetState::Mounted {
            identity: identity(id),
            attrs: None,
        };
        obs.targets.insert(a.target.clone(), mounted(1));
        obs.targets
            .insert(b.target.clone(), TargetState::NotMounted);
        obs.targets.insert(c.target.clone(), mounted(9));
        obs.targets
            .insert(d.target.clone(), TargetState::NotMounted);
        obs.targets.insert(gone.target.clone(), mounted(2));
        let mut members = BTreeMap::new();
        members.insert(
            Name::new("g").unwrap(),
            ["a", "b", "c", "d"]
                .iter()
                .map(|n| Name::new(n).unwrap())
                .collect::<BTreeSet<_>>(),
        );
        let observed = Observed {
            desired: vec![a, b, c, d],
            observations: obs,
            frozen: BTreeSet::new(),
            members,
            ignored: BTreeMap::new(),
            notes: vec![],
        };
        (observed, state)
    }

    fn planned(observed: &Observed, state: &State) -> Plan {
        plan::plan(PlanInput {
            desired: &observed.desired,
            state,
            frozen_groups: &observed.frozen,
            observations: &observed.observations,
        })
    }

    #[test]
    fn statuses_cover_each_case() {
        let (observed, state) = scenario();
        let p = planned(&observed, &state);
        let statuses = member_statuses(&observed, &p, &state, true);
        let by_name: BTreeMap<_, _> = statuses
            .iter()
            .map(|s| (s.name.as_str(), s.state))
            .collect();
        assert_eq!(by_name["a"], MemberState::Mounted);
        assert_eq!(by_name["b"], MemberState::WouldMount);
        assert_eq!(by_name["c"], MemberState::Conflict);
        assert_eq!(by_name["d"], MemberState::SourceUnavailable);
        assert_eq!(by_name["gone"], MemberState::WouldUnmount);

        let unknown = member_statuses(&observed, &p, &state, false);
        let c = unknown.iter().find(|s| s.name == "c").unwrap();
        assert_eq!(c.state, MemberState::MountedOwnershipUnknown);
    }

    #[test]
    fn group_report_counts() {
        let (observed, state) = scenario();
        let p = planned(&observed, &state);
        let statuses = member_statuses(&observed, &p, &state, true);
        let g = Name::new("g").unwrap();
        let mut extra = BTreeMap::new();
        extra.insert(g.clone(), ("shared".to_owned(), Level::Ok, vec![]));
        let reports = group_reports(&[(g, "/m".into())], &observed, &p, &statuses, &extra);
        let r = &reports[0];
        assert_eq!(r.members, 4);
        assert_eq!(r.sources_valid, 3);
        assert_eq!(r.sources_missing, 1);
        assert_eq!(r.existing_mounts, 1);
        assert_eq!(r.would_create, 1);
        assert_eq!(r.would_remove, 1);
        assert_eq!(r.conflicts, 1);
        assert_eq!(r.status, Level::Error);
        assert_eq!(r.membership_dir, "/m (readable)");
    }

    #[test]
    fn dry_run_text_and_level() {
        let (observed, state) = scenario();
        let p = planned(&observed, &state);
        let statuses = member_statuses(&observed, &p, &state, true);
        let g = Name::new("g").unwrap();
        let mut extra = BTreeMap::new();
        extra.insert(g.clone(), ("shared".to_owned(), Level::Ok, vec![]));
        let report = DryRunReport {
            checks: vec![
                Check::new("kernel.openat2", Level::Ok, ""),
                Check::new("procfs", Level::Ok, "/proc"),
            ],
            groups: group_reports(&[(g, "/m".into())], &observed, &p, &statuses, &extra),
            members: statuses,
        };
        assert_eq!(report.level(), Level::Error);
        let text = report.to_text();
        assert!(
            text.starts_with("kernel.openat2 = ok\nprocfs         = ok (/proc)\n"),
            "{text}"
        );
        assert!(text.contains("group=g  status=error"));
        assert!(text.contains("  would_create     = 1"));
        assert!(text.contains("g/c  state=conflict  target=/view/c"));
        assert!(
            !text.contains("g/a  state=mounted"),
            "mounted members are omitted: {text}"
        );
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["groups"][0]["would_remove"], 1);
        assert_eq!(json["members"][0]["state"], "mounted");
    }

    #[test]
    fn status_text() {
        let (observed, state) = scenario();
        let p = planned(&observed, &state);
        let report = StatusReport {
            state_file: "/var/lib/byssus/state.json".into(),
            ownership_known: false,
            notes: vec!["permission denied; join the byssus group".into()],
            ignored: BTreeMap::new(),
            members: member_statuses(&observed, &p, &state, false),
        };
        let text = report.to_text();
        assert!(text.starts_with(
            "state_file = /var/lib/byssus/state.json (unreadable: ownership unknown)\n"
        ));
        assert!(text.contains("note: permission denied"));
        assert!(text.contains("g/a  state=mounted  target=/view/a"));
        assert!(text.contains("g/d  state=source_unavailable  target=/view/d  detail=ENOENT"));
        assert_eq!(report.level(), Level::Warn);
    }

    #[test]
    fn rejected_and_ignored_entries_are_reported() {
        use crate::membership::{EntryKind, RejectReason, Rejection};
        let (mut observed, state) = scenario();
        let g = Name::new("g").unwrap();
        observed.notes.push(Note::Rejected {
            group: g.clone(),
            rejection: Rejection {
                display_name: "link".into(),
                reason: RejectReason::NotRegularFile(EntryKind::Symlink),
            },
        });
        observed
            .ignored
            .insert(g.clone(), vec![".gitkeep".into(), ".tmp-x".into()]);
        let p = planned(&observed, &state);
        let members = member_statuses(&observed, &p, &state, true);
        let link = members.iter().find(|m| m.name == "link").unwrap();
        assert_eq!(link.state, MemberState::Rejected);
        assert_eq!(link.target, None);
        assert_eq!(link.state.level(), Level::Warn);

        let mut ignored = BTreeMap::new();
        ignored.insert("g".to_owned(), 2);
        let report = StatusReport {
            state_file: "/s".into(),
            ownership_known: true,
            notes: vec![],
            ignored,
            members: members.clone(),
        };
        let text = report.to_text();
        assert!(
            text.contains("ignored: group 'g': 2 hidden entries"),
            "{text}"
        );
        assert!(
            text.contains("g/link  state=rejected  detail=\"not a regular file (symbolic link)\""),
            "{text}"
        );
        let json = serde_json::to_value(&report).unwrap();
        let link_json = json["members"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["name"] == "link")
            .unwrap();
        assert!(link_json.get("target").is_none());

        let mut extra = BTreeMap::new();
        extra.insert(g.clone(), ("shared".to_owned(), Level::Ok, vec![]));
        let groups = group_reports(&[(g, "/m".into())], &observed, &p, &members, &extra);
        assert_eq!(groups[0].rejected, 1);
        assert_eq!(groups[0].ignored, 2);
    }
}
