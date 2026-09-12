use super::*;

const OK_ATTRS: ObservedAttrs = ObservedAttrs {
    read_only: true,
    nosuid: true,
    nodev: true,
    noexec: true,
    nosymfollow: false,
};

fn name(s: &str) -> Name {
    Name::new(s).unwrap()
}

fn key(group: &str, member: &str) -> RecordKey {
    RecordKey {
        group: name(group),
        name: name(member),
    }
}

fn loc(root: &str, path: &str) -> Location {
    Location {
        root: AbsPath::new(root).unwrap(),
        path: path.to_owned(),
    }
}

fn source_loc(member: &str) -> Location {
    loc("/src", &format!("{member}/workspace"))
}

fn target_loc(group: &str, member: &str) -> Location {
    loc(&format!("/view/{group}"), member)
}

fn desired(group: &str, member: &str) -> DesiredMount {
    DesiredMount {
        key: key(group, member),
        source: source_loc(member),
        target: target_loc(group, member),
        attrs: MountAttrs::default(),
    }
}

fn dev_ino(ino: u64) -> DevIno {
    DevIno {
        dev_major: 8,
        dev_minor: 1,
        ino,
    }
}

fn ident(mnt_id: u64, ino: u64) -> MountIdentity {
    MountIdentity {
        mnt_id,
        mnt_id_unique: Some(mnt_id + 10_000),
        root: dev_ino(ino),
    }
}

fn record_for(d: &DesiredMount, id: MountIdentity) -> MountRecord {
    MountRecord {
        group: d.key.group.clone(),
        name: d.key.name.clone(),
        source_root: d.source.root.clone(),
        source: d.source.path.clone(),
        target_root: d.target.root.clone(),
        target: d.target.path.clone(),
        mnt_id: id.mnt_id,
        mnt_id_unique: id.mnt_id_unique,
        root_dev_major: id.root.dev_major,
        root_dev_minor: id.root.dev_minor,
        root_ino: id.root.ino,
        read_only: d.attrs.read_only,
        noexec: d.attrs.noexec,
        nosymfollow: d.attrs.nosymfollow,
        created_at: "2026-09-12T00:00:00Z".parse().unwrap(),
    }
}

fn mounted(id: MountIdentity) -> TargetState {
    TargetState::Mounted {
        identity: id,
        attrs: Some(OK_ATTRS),
    }
}

#[derive(Default)]
struct Scenario {
    desired: Vec<DesiredMount>,
    state: State,
    frozen: BTreeSet<Name>,
    obs: Observations,
}

impl Scenario {
    fn want(mut self, d: DesiredMount) -> Self {
        self.desired.push(d);
        self
    }

    fn record(mut self, r: MountRecord) -> Self {
        self.state.insert(r);
        self
    }

    fn target(mut self, location: Location, state: TargetState) -> Self {
        self.obs.targets.insert(location, state);
        self
    }

    fn source(mut self, k: RecordKey, state: SourceState) -> Self {
        self.obs.sources.insert(k, state);
        self
    }

    fn freeze(mut self, group: &str) -> Self {
        self.frozen.insert(name(group));
        self
    }

    fn plan(&self) -> Plan {
        plan(PlanInput {
            desired: &self.desired,
            state: &self.state,
            frozen_groups: &self.frozen,
            observations: &self.obs,
        })
    }
}

fn actions(p: &Plan) -> Vec<&Action> {
    p.steps.iter().map(|s| &s.action).collect()
}

fn unavailable(reason: &str) -> SourceState {
    SourceState::Unavailable(reason.to_owned())
}

// --- Decision table rows ---------------------------------------------------

#[test]
fn row1_create() {
    let d = desired("g", "a");
    let p = Scenario::default()
        .want(d.clone())
        .target(d.target.clone(), TargetState::NotMounted)
        .source(d.key.clone(), SourceState::Resolved(dev_ino(1)))
        .plan();
    assert_eq!(actions(&p), [&Action::Mount { desired: d }]);
    assert!(p.steps[0].depends_on.is_empty());
    assert!(p.findings.is_empty());
}

#[test]
fn row2_source_missing_skips() {
    let d = desired("g", "a");
    let p = Scenario::default()
        .want(d.clone())
        .target(d.target.clone(), TargetState::NotMounted)
        .source(d.key.clone(), unavailable("ENOENT"))
        .plan();
    assert!(p.is_noop());
    assert_eq!(
        p.findings,
        [Finding::SourceUnavailable {
            key: d.key,
            source: d.source,
            reason: "ENOENT".into()
        }]
    );
}

#[test]
fn row3_foreign_mount_conflict() {
    let d = desired("g", "a");
    let p = Scenario::default()
        .want(d.clone())
        .target(d.target.clone(), mounted(ident(7, 1)))
        .source(d.key.clone(), SourceState::Resolved(dev_ino(1)))
        .plan();
    assert!(p.is_noop());
    assert_eq!(
        p.findings,
        [Finding::Conflict {
            key: d.key,
            target: d.target,
            kind: ConflictKind::ForeignMount
        }]
    );
}

#[test]
fn row4_ours_noop() {
    let d = desired("g", "a");
    let id = ident(7, 1);
    let p = Scenario::default()
        .want(d.clone())
        .record(record_for(&d, id))
        .target(d.target.clone(), mounted(id))
        .source(d.key.clone(), SourceState::Resolved(dev_ino(1)))
        .plan();
    assert!(p.is_noop());
    assert!(p.findings.is_empty());
}

#[test]
fn row4_ours_with_unknown_attrs_is_noop() {
    let d = desired("g", "a");
    let id = ident(7, 1);
    let p = Scenario::default()
        .want(d.clone())
        .record(record_for(&d, id))
        .target(
            d.target.clone(),
            TargetState::Mounted {
                identity: id,
                attrs: None,
            },
        )
        .source(d.key.clone(), SourceState::Resolved(dev_ino(1)))
        .plan();
    assert!(p.is_noop());
}

#[test]
fn row4_ours_attribute_drift_reapplied() {
    let d = desired("g", "a");
    let id = ident(7, 1);
    for drift in [
        ObservedAttrs {
            read_only: false,
            ..OK_ATTRS
        },
        ObservedAttrs {
            nosuid: false,
            ..OK_ATTRS
        },
        ObservedAttrs {
            nodev: false,
            ..OK_ATTRS
        },
        ObservedAttrs {
            noexec: false,
            ..OK_ATTRS
        },
    ] {
        let record = record_for(&d, id);
        let p = Scenario::default()
            .want(d.clone())
            .record(record.clone())
            .target(
                d.target.clone(),
                TargetState::Mounted {
                    identity: id,
                    attrs: Some(drift),
                },
            )
            .source(d.key.clone(), SourceState::Resolved(dev_ino(1)))
            .plan();
        assert_eq!(
            actions(&p),
            [&Action::Reattr {
                record,
                attrs: MountAttrs::default()
            }],
            "{drift:?}"
        );
    }
}

#[test]
fn row4_extra_restrictions_tolerated() {
    let mut d = desired("g", "a");
    d.attrs.read_only = false;
    let id = ident(7, 1);
    // A read-only source mount makes the view read-only too; that is fine.
    let p = Scenario::default()
        .want(d.clone())
        .record(record_for(&d, id))
        .target(
            d.target.clone(),
            TargetState::Mounted {
                identity: id,
                attrs: Some(ObservedAttrs {
                    nosymfollow: true,
                    ..OK_ATTRS
                }),
            },
        )
        .source(d.key.clone(), SourceState::Resolved(dev_ino(1)))
        .plan();
    assert!(p.is_noop());
}

#[test]
fn row4_tightened_configuration_reapplied() {
    let old = desired("g", "a");
    let mut d = old.clone();
    d.attrs.nosymfollow = true;
    let id = ident(7, 1);
    let record = record_for(&old, id);
    let p = Scenario::default()
        .want(d.clone())
        .record(record.clone())
        .target(d.target.clone(), mounted(id))
        .source(d.key.clone(), SourceState::Resolved(dev_ino(1)))
        .plan();
    assert_eq!(
        actions(&p),
        [&Action::Reattr {
            record,
            attrs: d.attrs
        }]
    );
}

#[test]
fn row4_relaxed_configuration_remounts() {
    let old = desired("g", "a");
    let mut d = old.clone();
    d.attrs.read_only = false;
    let id = ident(7, 1);
    let record = record_for(&old, id);
    let p = Scenario::default()
        .want(d.clone())
        .record(record.clone())
        .target(d.target.clone(), mounted(id))
        .source(d.key.clone(), SourceState::Resolved(dev_ino(1)))
        .plan();
    assert_eq!(
        actions(&p),
        [
            &Action::Unmount {
                record,
                reason: UnmountReason::AttributesRelaxed
            },
            &Action::Mount { desired: d }
        ]
    );
    assert_eq!(p.steps[1].depends_on, [0]);
}

#[test]
fn row5_source_changed_remounts() {
    let d = desired("g", "a");
    let id = ident(7, 1);
    let record = record_for(&d, id);
    let p = Scenario::default()
        .want(d.clone())
        .record(record.clone())
        .target(d.target.clone(), mounted(id))
        .source(d.key.clone(), SourceState::Resolved(dev_ino(2)))
        .plan();
    assert_eq!(
        actions(&p),
        [
            &Action::Unmount {
                record,
                reason: UnmountReason::SourceChanged
            },
            &Action::Mount { desired: d }
        ]
    );
    assert_eq!(p.steps[1].depends_on, [0]);
}

#[test]
fn row6_source_gone_unmounts() {
    let d = desired("g", "a");
    let id = ident(7, 1);
    let record = record_for(&d, id);
    let p = Scenario::default()
        .want(d.clone())
        .record(record.clone())
        .target(d.target.clone(), mounted(id))
        .source(d.key.clone(), unavailable("ENOENT"))
        .plan();
    assert_eq!(
        actions(&p),
        [&Action::Unmount {
            record,
            reason: UnmountReason::SourceGone("ENOENT".into())
        }]
    );
}

#[test]
fn row7_target_replaced_conflict_keeps_record() {
    let d = desired("g", "a");
    let p = Scenario::default()
        .want(d.clone())
        .record(record_for(&d, ident(7, 1)))
        .target(d.target.clone(), mounted(ident(8, 1)))
        .source(d.key.clone(), SourceState::Resolved(dev_ino(1)))
        .plan();
    assert!(p.is_noop());
    assert_eq!(
        p.findings,
        [Finding::Conflict {
            key: d.key,
            target: d.target,
            kind: ConflictKind::TargetReplaced
        }]
    );
}

#[test]
fn row7_reused_mount_id_with_different_root_is_conflict() {
    let d = desired("g", "a");
    let p = Scenario::default()
        .want(d.clone())
        .record(record_for(&d, ident(7, 1)))
        .target(
            d.target.clone(),
            mounted(MountIdentity {
                mnt_id: 7,
                mnt_id_unique: None,
                root: dev_ino(99),
            }),
        )
        .source(d.key.clone(), SourceState::Resolved(dev_ino(1)))
        .plan();
    assert!(p.is_noop());
    assert!(matches!(
        p.findings[..],
        [Finding::Conflict {
            kind: ConflictKind::TargetReplaced,
            ..
        }]
    ));
}

#[test]
fn row8_recreate_after_external_removal() {
    let d = desired("g", "a");
    let p = Scenario::default()
        .want(d.clone())
        .record(record_for(&d, ident(7, 1)))
        .target(d.target.clone(), TargetState::NotMounted)
        .source(d.key.clone(), SourceState::Resolved(dev_ino(1)))
        .plan();
    assert_eq!(actions(&p), [&Action::Mount { desired: d }]);
}

#[test]
fn row9_record_dropped_when_source_unavailable() {
    let d = desired("g", "a");
    let record = record_for(&d, ident(7, 1));
    let p = Scenario::default()
        .want(d.clone())
        .record(record.clone())
        .target(d.target.clone(), TargetState::NotMounted)
        .source(d.key.clone(), unavailable("EACCES"))
        .plan();
    assert_eq!(
        actions(&p),
        [&Action::DropRecord {
            record,
            reason: DropReason::SourceUnavailable("EACCES".into())
        }]
    );
    assert!(matches!(
        p.findings[..],
        [Finding::SourceUnavailable { .. }]
    ));
}

#[test]
fn row10_member_removed_unmounts() {
    let d = desired("g", "a");
    let id = ident(7, 1);
    let record = record_for(&d, id);
    let p = Scenario::default()
        .record(record.clone())
        .target(d.target.clone(), mounted(id))
        .plan();
    assert_eq!(
        actions(&p),
        [&Action::Unmount {
            record,
            reason: UnmountReason::NotMember
        }]
    );
}

#[test]
fn row11_foreign_mount_at_recorded_target_drops_record() {
    let d = desired("g", "a");
    let record = record_for(&d, ident(7, 1));
    let p = Scenario::default()
        .record(record.clone())
        .target(d.target.clone(), mounted(ident(8, 1)))
        .plan();
    assert_eq!(
        actions(&p),
        [&Action::DropRecord {
            record,
            reason: DropReason::ForeignMountAtTarget
        }]
    );
}

#[test]
fn row12_stale_record_dropped() {
    let d = desired("g", "a");
    let record = record_for(&d, ident(7, 1));
    let p = Scenario::default()
        .record(record.clone())
        .target(d.target.clone(), TargetState::NotMounted)
        .plan();
    assert_eq!(
        actions(&p),
        [&Action::DropRecord {
            record,
            reason: DropReason::MountGone
        }]
    );
}

#[test]
fn rows13_and_14_nothing_to_do() {
    let d = desired("g", "a");
    // Row 13: an unrelated mount exists but nothing is desired or recorded.
    let p = Scenario::default()
        .target(d.target.clone(), mounted(ident(7, 1)))
        .plan();
    assert!(p.is_noop() && p.findings.is_empty());
    // Row 14.
    let p = Scenario::default().plan();
    assert!(p.is_noop() && p.findings.is_empty());
}

// --- Inaccessible targets and missing observations -----------------------

#[test]
fn desired_target_unavailable() {
    let d = desired("g", "a");
    let p = Scenario::default()
        .want(d.clone())
        .target(d.target.clone(), TargetState::Unavailable("ELOOP".into()))
        .source(d.key.clone(), SourceState::Resolved(dev_ino(1)))
        .plan();
    assert!(p.is_noop());
    assert_eq!(
        p.findings,
        [Finding::TargetUnavailable {
            key: d.key,
            target: d.target,
            reason: "ELOOP".into()
        }]
    );
}

#[test]
fn recorded_target_unavailable_keeps_record() {
    for with_desired in [false, true] {
        let d = desired("g", "a");
        let mut s = Scenario::default()
            .record(record_for(&d, ident(7, 1)))
            .target(d.target.clone(), TargetState::Unavailable("EACCES".into()))
            .source(d.key.clone(), SourceState::Resolved(dev_ino(1)));
        if with_desired {
            s = s.want(d.clone());
        }
        let p = s.plan();
        assert!(p.is_noop());
        assert!(matches!(
            p.findings[..],
            [Finding::TargetUnavailable { .. }]
        ));
    }
}

#[test]
fn missing_observations_are_treated_as_unavailable() {
    let d = desired("g", "a");
    let p = Scenario::default().want(d.clone()).plan();
    assert!(p.is_noop());
    assert!(matches!(
        &p.findings[..],
        [Finding::TargetUnavailable { reason, .. }] if reason.contains("not observed")
    ));

    let p = Scenario::default()
        .want(d.clone())
        .target(d.target.clone(), TargetState::NotMounted)
        .plan();
    assert!(matches!(
        &p.findings[..],
        [Finding::SourceUnavailable { reason, .. }] if reason.contains("not observed")
    ));
}

// --- Degraded groups -------------------------------------------------------

#[test]
fn frozen_group_records_untouched() {
    let d = desired("g", "a");
    let id = ident(7, 1);
    let p = Scenario::default()
        .record(record_for(&d, id))
        .target(d.target.clone(), mounted(id))
        .freeze("g")
        .plan();
    assert!(p.is_noop());
    assert!(p.findings.is_empty());
}

#[test]
fn frozen_group_desired_ignored() {
    let d = desired("g", "a");
    let p = Scenario::default()
        .want(d.clone())
        .target(d.target.clone(), TargetState::NotMounted)
        .source(d.key.clone(), SourceState::Resolved(dev_ino(1)))
        .freeze("g")
        .plan();
    assert!(p.is_noop());
}

#[test]
fn other_groups_unaffected_by_frozen_group() {
    let frozen = desired("frozen", "a");
    let live = desired("live", "a");
    let id = ident(7, 1);
    let p = Scenario::default()
        .record(record_for(&frozen, id))
        .target(frozen.target.clone(), mounted(id))
        .record(record_for(&live, ident(8, 1)))
        .target(live.target.clone(), mounted(ident(8, 1)))
        .freeze("frozen")
        .plan();
    assert_eq!(p.steps.len(), 1);
    assert!(matches!(
        &p.steps[0].action,
        Action::Unmount { record, .. } if record.group.as_str() == "live"
    ));
}

// --- Relocation ------------------------------------------------------------

#[test]
fn template_change_moves_mount() {
    let old = desired("g", "a");
    let mut new = old.clone();
    new.target = loc("/view/g", "a/ro");
    let id = ident(7, 1);
    let old_record = record_for(&old, id);
    let p = Scenario::default()
        .want(new.clone())
        .record(old_record.clone())
        .target(old.target.clone(), mounted(id))
        .target(new.target.clone(), TargetState::NotMounted)
        .source(new.key.clone(), SourceState::Resolved(dev_ino(1)))
        .plan();
    assert_eq!(
        actions(&p),
        [
            &Action::Unmount {
                record: old_record,
                reason: UnmountReason::Relocated
            },
            &Action::Mount { desired: new }
        ]
    );
    assert_eq!(p.steps[1].depends_on, [0]);
}

#[test]
fn root_change_moves_mount_and_drops_stale_record() {
    let old = desired("g", "a");
    let mut new = old.clone();
    new.source = loc("/src2", "a/workspace");
    let old_record = record_for(&old, ident(7, 1));
    let p = Scenario::default()
        .want(new.clone())
        .record(old_record.clone())
        .target(old.target.clone(), TargetState::NotMounted)
        .source(new.key.clone(), SourceState::Resolved(dev_ino(1)))
        .plan();
    // Same target, record's source differs: old record dropped (nothing
    // mounted), then mount at the (same) target.
    assert_eq!(
        actions(&p),
        [
            &Action::DropRecord {
                record: old_record,
                reason: DropReason::MountGone
            },
            &Action::Mount { desired: new }
        ]
    );
    assert_eq!(p.steps[1].depends_on, [0]);
}

#[test]
fn source_root_change_with_existing_mount_remounts_in_place() {
    let old = desired("g", "a");
    let mut new = old.clone();
    new.source = loc("/src2", "a/workspace");
    let id = ident(7, 1);
    let old_record = record_for(&old, id);
    let p = Scenario::default()
        .want(new.clone())
        .record(old_record.clone())
        .target(old.target.clone(), mounted(id))
        .source(new.key.clone(), SourceState::Resolved(dev_ino(5)))
        .plan();
    assert_eq!(
        actions(&p),
        [
            &Action::Unmount {
                record: old_record,
                reason: UnmountReason::Relocated
            },
            &Action::Mount { desired: new }
        ]
    );
    assert_eq!(p.steps[1].depends_on, [0]);
}

#[test]
fn relocation_blocked_when_old_target_unverifiable() {
    let old = desired("g", "a");
    let mut new = old.clone();
    new.target = loc("/view/g", "a/ro");
    let p = Scenario::default()
        .want(new.clone())
        .record(record_for(&old, ident(7, 1)))
        .target(
            old.target.clone(),
            TargetState::Unavailable("EACCES".into()),
        )
        .target(new.target.clone(), TargetState::NotMounted)
        .source(new.key.clone(), SourceState::Resolved(dev_ino(1)))
        .plan();
    assert!(p.is_noop());
    assert_eq!(
        p.findings,
        [
            Finding::TargetUnavailable {
                key: old.key.clone(),
                target: old.target.clone(),
                reason: "EACCES".into()
            },
            Finding::RelocationBlocked {
                key: old.key,
                old_target: old.target
            }
        ]
    );
}

#[test]
fn removed_group_unmounts_all_members() {
    let a = desired("gone", "a");
    let b = desired("gone", "b");
    let p = Scenario::default()
        .record(record_for(&a, ident(1, 1)))
        .record(record_for(&b, ident(2, 2)))
        .target(a.target.clone(), mounted(ident(1, 1)))
        .target(b.target.clone(), mounted(ident(2, 2)))
        .plan();
    assert_eq!(p.steps.len(), 2);
    assert!(p.steps.iter().all(|s| matches!(
        s.action,
        Action::Unmount {
            reason: UnmountReason::NotMember,
            ..
        }
    )));
}

// --- Collisions and freed targets -------------------------------------------

#[test]
fn collision_with_no_existing_mount_mounts_neither() {
    let a = desired("g1", "x");
    let mut b = desired("g2", "x");
    b.target = a.target.clone();
    let p = Scenario::default()
        .want(a.clone())
        .want(b.clone())
        .target(a.target.clone(), TargetState::NotMounted)
        .source(a.key.clone(), SourceState::Resolved(dev_ino(1)))
        .source(b.key.clone(), SourceState::Resolved(dev_ino(1)))
        .plan();
    assert!(p.is_noop());
    assert_eq!(
        p.findings,
        [Finding::TargetCollision {
            target: a.target,
            members: vec![a.key, b.key],
            kept: None
        }]
    );
}

#[test]
fn collision_keeps_existing_owner() {
    let a = desired("g1", "x");
    let mut b = desired("g2", "x");
    b.target = a.target.clone();
    let id = ident(7, 1);
    let p = Scenario::default()
        .want(a.clone())
        .want(b.clone())
        .record(record_for(&b, id))
        .target(b.target.clone(), mounted(id))
        .source(a.key.clone(), SourceState::Resolved(dev_ino(1)))
        .source(b.key.clone(), SourceState::Resolved(dev_ino(1)))
        .plan();
    assert!(p.is_noop(), "{:#?}", p.steps);
    assert_eq!(
        p.findings,
        [Finding::TargetCollision {
            target: a.target,
            members: vec![a.key, b.key.clone()],
            kept: Some(b.key)
        }]
    );
}

#[test]
fn collision_loser_with_record_elsewhere_is_unmounted() {
    let a = desired("g1", "x");
    let old_b = desired("g2", "x");
    let mut new_b = old_b.clone();
    new_b.target = a.target.clone();
    let b_id = ident(9, 1);
    let b_record = record_for(&old_b, b_id);
    let p = Scenario::default()
        .want(a.clone())
        .want(new_b.clone())
        .record(b_record.clone())
        .target(a.target.clone(), TargetState::NotMounted)
        .target(old_b.target.clone(), mounted(b_id))
        .source(a.key.clone(), SourceState::Resolved(dev_ino(1)))
        .source(new_b.key.clone(), SourceState::Resolved(dev_ino(1)))
        .plan();
    assert_eq!(
        actions(&p),
        [&Action::Unmount {
            record: b_record,
            reason: UnmountReason::TargetCollision
        }]
    );
}

#[test]
fn mount_waits_for_target_freed_by_another_member() {
    // Member "old" leaves; member "new" of another group now targets the same
    // location, currently occupied by old's mount.
    let old = desired("g1", "old");
    let mut new = desired("g2", "new");
    new.target = old.target.clone();
    let id = ident(7, 1);
    let old_record = record_for(&old, id);
    let p = Scenario::default()
        .want(new.clone())
        .record(old_record.clone())
        .target(old.target.clone(), mounted(id))
        .source(new.key.clone(), SourceState::Resolved(dev_ino(3)))
        .plan();
    assert_eq!(
        actions(&p),
        [
            &Action::Unmount {
                record: old_record,
                reason: UnmountReason::NotMember
            },
            &Action::Mount { desired: new }
        ]
    );
    assert_eq!(p.steps[1].depends_on, [0]);
    assert!(p.findings.is_empty());
}

// --- Ordering ----------------------------------------------------------------

#[test]
fn steps_ordered_by_phase_with_remapped_dependencies() {
    let create = desired("a", "first"); // sorts first: its Mount is created early
    let drift = desired("b", "drift");
    let changed = desired("c", "changed");
    let removed = desired("z", "removed");
    let drift_id = ident(2, 2);
    let changed_id = ident(3, 3);
    let removed_id = ident(4, 4);
    let p = Scenario::default()
        .want(create.clone())
        .want(drift.clone())
        .want(changed.clone())
        .record(record_for(&drift, drift_id))
        .record(record_for(&changed, changed_id))
        .record(record_for(&removed, removed_id))
        .target(create.target.clone(), TargetState::NotMounted)
        .target(
            drift.target.clone(),
            TargetState::Mounted {
                identity: drift_id,
                attrs: Some(ObservedAttrs {
                    read_only: false,
                    ..OK_ATTRS
                }),
            },
        )
        .target(changed.target.clone(), mounted(changed_id))
        .target(removed.target.clone(), mounted(removed_id))
        .source(create.key.clone(), SourceState::Resolved(dev_ino(1)))
        .source(drift.key.clone(), SourceState::Resolved(dev_ino(2)))
        .source(changed.key.clone(), SourceState::Resolved(dev_ino(30)))
        .plan();

    let phases: Vec<u8> = p.steps.iter().map(|s| s.action.phase()).collect();
    assert_eq!(phases, [0, 0, 1, 2, 2]);
    // The remount of "changed" depends on its unmount.
    let unmount_changed = p
        .steps
        .iter()
        .position(|s| {
            matches!(
                &s.action,
                Action::Unmount {
                    reason: UnmountReason::SourceChanged,
                    ..
                }
            )
        })
        .unwrap();
    let mount_changed = p
        .steps
        .iter()
        .position(|s| matches!(&s.action, Action::Mount { desired } if desired.key == changed.key))
        .unwrap();
    assert!(unmount_changed < mount_changed);
    assert_eq!(p.steps[mount_changed].depends_on, [unmount_changed]);
    for (index, step) in p.steps.iter().enumerate() {
        assert!(step.depends_on.iter().all(|&d| d < index));
    }
}

// --- Helpers -----------------------------------------------------------------

#[test]
fn observed_attrs_enforce() {
    assert!(OK_ATTRS.enforce(&MountAttrs::default()));
    assert!(OK_ATTRS.enforce(&MountAttrs {
        noexec: false,
        read_only: false,
        nosymfollow: false,
    }));
    assert!(!OK_ATTRS.enforce(&MountAttrs {
        nosymfollow: true,
        ..MountAttrs::default()
    }));
    for missing in [
        ObservedAttrs {
            nosuid: false,
            ..OK_ATTRS
        },
        ObservedAttrs {
            nodev: false,
            ..OK_ATTRS
        },
        ObservedAttrs {
            read_only: false,
            ..OK_ATTRS
        },
        ObservedAttrs {
            noexec: false,
            ..OK_ATTRS
        },
    ] {
        assert!(!missing.enforce(&MountAttrs::default()), "{missing:?}");
    }
}

#[test]
fn location_display() {
    assert_eq!(loc("/view/g", "a/b").to_string(), "/view/g/a/b");
    assert_eq!(loc("/", "a").to_string(), "/a");
}
