//! Pure capability-normalization planning.
//!
//! Given the process's current credentials and what the caller needs, decide
//! the exact sequence of credential operations that brings the process to a
//! minimal state. See `docs/DESIGN.md`, "Capability normalization".

use rustix::thread::{CapabilitiesSecureBits, CapabilitySet, CapabilitySets};

/// Current credentials of the process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Credentials {
    /// Real, effective and saved user IDs.
    pub uids: [u32; 3],
    /// Capability sets.
    pub caps: CapabilitySets,
}

impl Credentials {
    fn any_uid_is_root(&self) -> bool {
        self.uids.contains(&0)
    }
}

/// A resolved service user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceUser {
    /// User name.
    pub name: String,
    /// User ID.
    pub uid: u32,
    /// Primary group ID.
    pub gid: u32,
    /// Supplementary group IDs.
    pub groups: Vec<u32>,
}

impl ServiceUser {
    /// The groups a switch to this user sets: its primary group, with the
    /// primary group also among the supplementary groups.
    fn final_groups(&self) -> FinalGroups {
        let mut groups = self.groups.clone();
        if !groups.contains(&self.gid) {
            groups.insert(0, self.gid);
        }
        FinalGroups {
            gid: self.gid,
            groups,
        }
    }
}

/// What the caller needs after normalization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Goal {
    /// Keep exactly `CAP_SYS_ADMIN` (daemon, `byssus reconcile`).
    KeepSysAdmin,
    /// Keep no capabilities (`byssus dry-run` simulating the service user).
    DropAll,
}

/// A normalization request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// Capabilities to keep.
    pub goal: Goal,
    /// Service user to switch to if running as root.
    pub user: Option<ServiceUser>,
    /// Permit remaining UID 0 when no service user is configured.
    pub allow_root: bool,
}

/// One credential operation, applied in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// `PR_CAP_AMBIENT_CLEAR_ALL`.
    ClearAmbient,
    /// `capset` to the given sets.
    SetCaps(CapabilitySets),
    /// `PR_CAPBSET_DROP` for every capability not in the set.
    DropBoundingExcept(CapabilitySet),
    /// `PR_SET_SECUREBITS`.
    SetSecureBits(CapabilitiesSecureBits),
    /// `PR_SET_KEEPCAPS`.
    SetKeepCaps(bool),
    /// `setgroups`.
    SetGroups(Vec<u32>),
    /// `setresgid` with all three IDs equal.
    SetResGid(u32),
    /// `setresuid` with all three IDs equal.
    SetResUid(u32),
    /// `PR_SET_NO_NEW_PRIVS`.
    SetNoNewPrivs,
}

/// A non-fatal observation about the plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Warning {
    /// Remaining UID 0 because `allow_root` was given.
    RunningAsRoot,
    /// A service user is configured but the process is not root, so no switch
    /// happens.
    UserNotApplied {
        /// Configured user name.
        user: String,
        /// The effective UID the process keeps.
        effective_uid: u32,
    },
}

/// Normalization cannot proceed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlanError {
    /// `CAP_SYS_ADMIN` is required but not permitted.
    #[error(
        "CAP_SYS_ADMIN is not permitted; run under systemd with AmbientCapabilities=CAP_SYS_ADMIN, \
         or apply 'setcap cap_sys_admin+ep' to the binary"
    )]
    MissingSysAdmin,
    /// Running as root with no service user and no `allow_root`.
    #[error(
        "refusing to run as root: configure daemon.user (or pass --user) so privileges can be \
         dropped, or pass --allow-root for development and testing"
    )]
    RootWithoutUser,
    /// The configured service user is root.
    #[error("service user '{0}' has UID 0; choose an unprivileged user")]
    ServiceUserIsRoot(String),
    /// Cannot switch user because a needed capability is missing.
    #[error("cannot switch to user '{user}': {missing} is not permitted")]
    CannotSwitchUser {
        /// Configured user name.
        user: String,
        /// The missing capability.
        missing: &'static str,
    },
}

/// The planned operations and the expected final state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivilegePlan {
    /// Operations in order.
    pub ops: Vec<Op>,
    /// Non-fatal warnings.
    pub warnings: Vec<Warning>,
    /// UID expected after applying (all of real, effective, saved).
    pub final_uid: u32,
    /// Capabilities expected in the effective and permitted sets afterwards;
    /// the inheritable and ambient sets are empty.
    pub final_caps: CapabilitySet,
    /// Groups expected after switching to the service user, or `None` when
    /// no switch is planned and groups are left unchanged.
    pub final_groups: Option<FinalGroups>,
}

/// Groups expected after a user switch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinalGroups {
    /// GID expected (all of real, effective, saved).
    pub gid: u32,
    /// Supplementary groups expected, including `gid`.
    pub groups: Vec<u32>,
}

/// Securebits set and locked when `CAP_SETPCAP` is available: never grant
/// capabilities to root on exec, never adjust capabilities on UID changes,
/// never keep capabilities via `PR_SET_KEEPCAPS`, never raise ambient
/// capabilities.
pub const LOCKED_SECURE_BITS: CapabilitiesSecureBits = CapabilitiesSecureBits::NO_ROOT
    .union(CapabilitiesSecureBits::NO_ROOT_LOCKED)
    .union(CapabilitiesSecureBits::NO_SETUID_FIXUP)
    .union(CapabilitiesSecureBits::NO_SETUID_FIXUP_LOCKED)
    .union(CapabilitiesSecureBits::KEEP_CAPS_LOCKED)
    .union(CapabilitiesSecureBits::NO_CAP_AMBIENT_RAISE)
    .union(CapabilitiesSecureBits::NO_CAP_AMBIENT_RAISE_LOCKED);

/// Plans normalization.
pub fn plan(current: &Credentials, request: &Request) -> Result<PrivilegePlan, PlanError> {
    let keep = match request.goal {
        Goal::KeepSysAdmin => CapabilitySet::SYS_ADMIN,
        Goal::DropAll => CapabilitySet::empty(),
    };
    let permitted = current.caps.permitted;
    if request.goal == Goal::KeepSysAdmin && !permitted.contains(CapabilitySet::SYS_ADMIN) {
        return Err(PlanError::MissingSysAdmin);
    }

    let mut warnings = Vec::new();
    let effective_uid = current.uids[1];
    let switch_to = match (current.any_uid_is_root(), &request.user) {
        (true, Some(user)) if user.uid == 0 => {
            return Err(PlanError::ServiceUserIsRoot(user.name.clone()));
        }
        (true, Some(user)) => Some(user),
        (true, None) => {
            if request.goal == Goal::KeepSysAdmin {
                if !request.allow_root {
                    return Err(PlanError::RootWithoutUser);
                }
                warnings.push(Warning::RunningAsRoot);
            }
            None
        }
        (false, Some(user)) => {
            if effective_uid != user.uid {
                warnings.push(Warning::UserNotApplied {
                    user: user.name.clone(),
                    effective_uid,
                });
            }
            None
        }
        (false, None) => None,
    };

    if let Some(user) = switch_to {
        for (cap, label) in [
            (CapabilitySet::SETUID, "CAP_SETUID"),
            (CapabilitySet::SETGID, "CAP_SETGID"),
        ] {
            if !permitted.contains(cap) {
                return Err(PlanError::CannotSwitchUser {
                    user: user.name.clone(),
                    missing: label,
                });
            }
        }
    }

    let has_setpcap = permitted.contains(CapabilitySet::SETPCAP);
    let mut ops = vec![Op::ClearAmbient];

    // Capabilities we will use along the way must be effective first.
    let mut needed = CapabilitySet::empty();
    if has_setpcap {
        needed |= CapabilitySet::SETPCAP;
    }
    if switch_to.is_some() {
        needed |= CapabilitySet::SETUID | CapabilitySet::SETGID;
    }
    if !current.caps.effective.contains(needed) {
        ops.push(Op::SetCaps(CapabilitySets {
            effective: current.caps.effective | needed,
            permitted,
            inheritable: CapabilitySet::empty(),
        }));
    }

    if has_setpcap {
        ops.push(Op::DropBoundingExcept(keep));
        ops.push(Op::SetSecureBits(LOCKED_SECURE_BITS));
    }

    if let Some(user) = switch_to {
        // With NO_SETUID_FIXUP locked, capabilities survive the UID change.
        // Without CAP_SETPCAP we cannot set securebits, so use KEEPCAPS.
        if !has_setpcap {
            ops.push(Op::SetKeepCaps(true));
        }
        let groups = user.final_groups();
        ops.push(Op::SetGroups(groups.groups));
        ops.push(Op::SetResGid(groups.gid));
        ops.push(Op::SetResUid(user.uid));
        if !has_setpcap {
            ops.push(Op::SetKeepCaps(false));
        }
    }

    ops.push(Op::SetCaps(CapabilitySets {
        effective: keep,
        permitted: keep,
        inheritable: CapabilitySet::empty(),
    }));
    ops.push(Op::SetNoNewPrivs);

    Ok(PrivilegePlan {
        ops,
        warnings,
        final_uid: switch_to.map_or(effective_uid, |u| u.uid),
        final_caps: keep,
        final_groups: switch_to.map(ServiceUser::final_groups),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(effective: CapabilitySet, permitted: CapabilitySet) -> CapabilitySets {
        CapabilitySets {
            effective,
            permitted,
            inheritable: CapabilitySet::empty(),
        }
    }

    fn creds(uid: u32, sets: CapabilitySets) -> Credentials {
        Credentials {
            uids: [uid; 3],
            caps: sets,
        }
    }

    fn user() -> ServiceUser {
        ServiceUser {
            name: "byssus".into(),
            uid: 991,
            gid: 991,
            groups: vec![],
        }
    }

    fn request(goal: Goal, user: Option<ServiceUser>, allow_root: bool) -> Request {
        Request {
            goal,
            user,
            allow_root,
        }
    }

    const SYS_ADMIN: CapabilitySet = CapabilitySet::SYS_ADMIN;

    fn keep_final(set: CapabilitySet) -> Op {
        Op::SetCaps(caps(set, set))
    }

    #[test]
    fn systemd_launch_only_sys_admin() {
        let current = creds(991, caps(SYS_ADMIN, SYS_ADMIN));
        let p = plan(&current, &request(Goal::KeepSysAdmin, None, false)).unwrap();
        assert_eq!(
            p.ops,
            [Op::ClearAmbient, keep_final(SYS_ADMIN), Op::SetNoNewPrivs]
        );
        assert_eq!(p.final_uid, 991);
        assert_eq!(p.final_caps, SYS_ADMIN);
        assert!(p.warnings.is_empty());
    }

    #[test]
    fn setcap_launch_with_extra_caps_drops_them() {
        let extra = SYS_ADMIN | CapabilitySet::NET_ADMIN | CapabilitySet::DAC_OVERRIDE;
        let current = creds(1000, caps(extra, extra));
        let p = plan(&current, &request(Goal::KeepSysAdmin, None, false)).unwrap();
        assert_eq!(
            p.ops,
            [Op::ClearAmbient, keep_final(SYS_ADMIN), Op::SetNoNewPrivs]
        );
    }

    #[test]
    fn missing_sys_admin_is_an_error() {
        let current = creds(991, caps(CapabilitySet::empty(), CapabilitySet::empty()));
        assert_eq!(
            plan(&current, &request(Goal::KeepSysAdmin, None, false)),
            Err(PlanError::MissingSysAdmin)
        );
    }

    #[test]
    fn root_without_user_refused() {
        let current = creds(0, caps(CapabilitySet::all(), CapabilitySet::all()));
        assert_eq!(
            plan(&current, &request(Goal::KeepSysAdmin, None, false)),
            Err(PlanError::RootWithoutUser)
        );
    }

    #[test]
    fn partially_root_uids_count_as_root() {
        let current = Credentials {
            uids: [1000, 1000, 0],
            caps: caps(CapabilitySet::all(), CapabilitySet::all()),
        };
        assert_eq!(
            plan(&current, &request(Goal::KeepSysAdmin, None, false)),
            Err(PlanError::RootWithoutUser)
        );
    }

    #[test]
    fn root_with_allow_root_drops_caps_and_locks_bits() {
        let all = CapabilitySet::all();
        let current = creds(0, caps(all, all));
        let p = plan(&current, &request(Goal::KeepSysAdmin, None, true)).unwrap();
        assert_eq!(
            p.ops,
            [
                Op::ClearAmbient,
                Op::DropBoundingExcept(SYS_ADMIN),
                Op::SetSecureBits(LOCKED_SECURE_BITS),
                keep_final(SYS_ADMIN),
                Op::SetNoNewPrivs,
            ]
        );
        assert_eq!(p.final_uid, 0);
        assert_eq!(p.final_groups, None);
        assert_eq!(p.warnings, [Warning::RunningAsRoot]);
    }

    #[test]
    fn root_switches_to_service_user() {
        let all = CapabilitySet::all();
        let current = creds(0, caps(all, all));
        let mut u = user();
        u.groups = vec![5, 7];
        let p = plan(&current, &request(Goal::KeepSysAdmin, Some(u), false)).unwrap();
        assert_eq!(
            p.ops,
            [
                Op::ClearAmbient,
                Op::DropBoundingExcept(SYS_ADMIN),
                Op::SetSecureBits(LOCKED_SECURE_BITS),
                Op::SetGroups(vec![991, 5, 7]),
                Op::SetResGid(991),
                Op::SetResUid(991),
                keep_final(SYS_ADMIN),
                Op::SetNoNewPrivs,
            ]
        );
        assert_eq!(p.final_uid, 991);
        assert_eq!(
            p.final_groups,
            Some(FinalGroups {
                gid: 991,
                groups: vec![991, 5, 7],
            })
        );
        assert!(p.warnings.is_empty());
    }

    #[test]
    fn primary_group_not_duplicated() {
        let all = CapabilitySet::all();
        let current = creds(0, caps(all, all));
        let mut u = user();
        u.groups = vec![3, 991];
        let p = plan(&current, &request(Goal::KeepSysAdmin, Some(u), false)).unwrap();
        assert!(p.ops.contains(&Op::SetGroups(vec![3, 991])));
    }

    #[test]
    fn switch_without_setpcap_uses_keepcaps() {
        let permitted = SYS_ADMIN | CapabilitySet::SETUID | CapabilitySet::SETGID;
        let current = creds(0, caps(SYS_ADMIN, permitted));
        let p = plan(&current, &request(Goal::KeepSysAdmin, Some(user()), false)).unwrap();
        assert_eq!(
            p.ops,
            [
                Op::ClearAmbient,
                Op::SetCaps(caps(permitted, permitted)),
                Op::SetKeepCaps(true),
                Op::SetGroups(vec![991]),
                Op::SetResGid(991),
                Op::SetResUid(991),
                Op::SetKeepCaps(false),
                keep_final(SYS_ADMIN),
                Op::SetNoNewPrivs,
            ]
        );
    }

    #[test]
    fn needed_caps_raised_to_effective_first() {
        let all = CapabilitySet::all();
        let current = creds(0, caps(SYS_ADMIN, all));
        let p = plan(&current, &request(Goal::KeepSysAdmin, Some(user()), false)).unwrap();
        assert_eq!(
            p.ops[1],
            Op::SetCaps(caps(
                SYS_ADMIN | CapabilitySet::SETPCAP | CapabilitySet::SETUID | CapabilitySet::SETGID,
                all
            ))
        );
    }

    #[test]
    fn cannot_switch_without_setuid_or_setgid() {
        let current = creds(0, caps(SYS_ADMIN, SYS_ADMIN | CapabilitySet::SETGID));
        assert_eq!(
            plan(&current, &request(Goal::KeepSysAdmin, Some(user()), false)),
            Err(PlanError::CannotSwitchUser {
                user: "byssus".into(),
                missing: "CAP_SETUID"
            })
        );
        let current = creds(0, caps(SYS_ADMIN, SYS_ADMIN | CapabilitySet::SETUID));
        assert!(matches!(
            plan(&current, &request(Goal::KeepSysAdmin, Some(user()), false)),
            Err(PlanError::CannotSwitchUser {
                missing: "CAP_SETGID",
                ..
            })
        ));
    }

    #[test]
    fn service_user_must_not_be_root() {
        let all = CapabilitySet::all();
        let current = creds(0, caps(all, all));
        let mut u = user();
        u.uid = 0;
        assert_eq!(
            plan(&current, &request(Goal::KeepSysAdmin, Some(u), false)),
            Err(PlanError::ServiceUserIsRoot("byssus".into()))
        );
    }

    #[test]
    fn user_ignored_when_not_root() {
        let current = creds(1000, caps(SYS_ADMIN, SYS_ADMIN));
        let p = plan(&current, &request(Goal::KeepSysAdmin, Some(user()), false)).unwrap();
        assert_eq!(
            p.warnings,
            [Warning::UserNotApplied {
                user: "byssus".into(),
                effective_uid: 1000
            }]
        );
        assert_eq!(p.final_uid, 1000);

        // Already the service user: no warning.
        let current = creds(991, caps(SYS_ADMIN, SYS_ADMIN));
        let p = plan(&current, &request(Goal::KeepSysAdmin, Some(user()), false)).unwrap();
        assert!(p.warnings.is_empty());
    }

    #[test]
    fn drop_all_as_unprivileged_user() {
        let current = creds(1000, caps(CapabilitySet::empty(), CapabilitySet::empty()));
        let p = plan(&current, &request(Goal::DropAll, None, false)).unwrap();
        assert_eq!(
            p.ops,
            [
                Op::ClearAmbient,
                keep_final(CapabilitySet::empty()),
                Op::SetNoNewPrivs
            ]
        );
        assert_eq!(p.final_caps, CapabilitySet::empty());
    }

    #[test]
    fn drop_all_as_root_switches_user_or_stays_root_without_error() {
        let all = CapabilitySet::all();
        let current = creds(0, caps(all, all));
        let p = plan(&current, &request(Goal::DropAll, Some(user()), false)).unwrap();
        assert_eq!(p.final_uid, 991);
        assert!(
            p.ops
                .contains(&Op::DropBoundingExcept(CapabilitySet::empty()))
        );

        let p = plan(&current, &request(Goal::DropAll, None, false)).unwrap();
        assert_eq!(p.final_uid, 0);
        assert!(p.warnings.is_empty());
        assert_eq!(p.ops.last(), Some(&Op::SetNoNewPrivs));
    }

    #[test]
    fn plans_always_end_minimal() {
        let all = CapabilitySet::all();
        for current in [
            creds(0, caps(all, all)),
            creds(991, caps(SYS_ADMIN, SYS_ADMIN)),
            creds(0, caps(SYS_ADMIN, all)),
        ] {
            for user in [None, Some(user())] {
                let Ok(p) = plan(&current, &request(Goal::KeepSysAdmin, user, true)) else {
                    continue;
                };
                assert_eq!(p.ops[0], Op::ClearAmbient);
                assert_eq!(p.ops[p.ops.len() - 2], keep_final(SYS_ADMIN));
                assert_eq!(p.ops[p.ops.len() - 1], Op::SetNoNewPrivs);
            }
        }
    }
}
