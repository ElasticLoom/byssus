//! Applying and verifying a privilege-normalization plan.

use std::io;

use rustix::process::{Gid, Uid};
use rustix::thread::{CapabilitySet, CapabilitySets};

use super::plan::{Credentials, Op, PrivilegePlan};

/// Credential state parsed from `/proc/self/status`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProcStatus {
    /// Real, effective, saved and filesystem UIDs.
    pub uids: [u32; 4],
    /// Real, effective, saved and filesystem GIDs.
    pub gids: [u32; 4],
    /// Number of threads.
    pub threads: u32,
    /// `CapInh`.
    pub cap_inheritable: u64,
    /// `CapPrm`.
    pub cap_permitted: u64,
    /// `CapEff`.
    pub cap_effective: u64,
    /// `CapBnd`.
    pub cap_bounding: u64,
    /// `CapAmb`.
    pub cap_ambient: u64,
    /// `NoNewPrivs`.
    pub no_new_privs: bool,
}

impl ProcStatus {
    /// Parses the relevant fields of `/proc/<pid>/status`.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut status = Self::default();
        let mut seen = 0u16;
        for line in text.lines() {
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim();
            let hex = || u64::from_str_radix(value, 16).map_err(|e| format!("{key}: {e}"));
            let bit = match key {
                "Uid" => {
                    status.uids = four_ids(key, value)?;
                    1
                }
                "Gid" => {
                    status.gids = four_ids(key, value)?;
                    2
                }
                "Threads" => {
                    status.threads = value.parse().map_err(|e| format!("{key}: {e}"))?;
                    4
                }
                "CapInh" => {
                    status.cap_inheritable = hex()?;
                    8
                }
                "CapPrm" => {
                    status.cap_permitted = hex()?;
                    16
                }
                "CapEff" => {
                    status.cap_effective = hex()?;
                    32
                }
                "CapBnd" => {
                    status.cap_bounding = hex()?;
                    64
                }
                "CapAmb" => {
                    status.cap_ambient = hex()?;
                    128
                }
                "NoNewPrivs" => {
                    status.no_new_privs = value == "1";
                    256
                }
                _ => 0,
            };
            seen |= bit;
        }
        if seen == 511 {
            Ok(status)
        } else {
            Err(format!("missing fields in status (seen mask {seen:#b})"))
        }
    }

    /// Reads `/proc/self/status`.
    pub fn read_self() -> io::Result<Self> {
        let text = std::fs::read_to_string("/proc/self/status")?;
        Self::parse(&text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    /// Current credentials in the form the planner uses.
    #[must_use]
    pub fn credentials(&self) -> Credentials {
        Credentials {
            uids: [self.uids[0], self.uids[1], self.uids[2]],
            caps: CapabilitySets {
                effective: CapabilitySet::from_bits_retain(self.cap_effective),
                permitted: CapabilitySet::from_bits_retain(self.cap_permitted),
                inheritable: CapabilitySet::from_bits_retain(self.cap_inheritable),
            },
        }
    }
}

fn four_ids(key: &str, value: &str) -> Result<[u32; 4], String> {
    let ids: Vec<u32> = value
        .split_whitespace()
        .map(str::parse)
        .collect::<Result<_, _>>()
        .map_err(|e| format!("{key}: {e}"))?;
    ids.try_into()
        .map_err(|v: Vec<u32>| format!("{key}: expected 4 IDs, found {}", v.len()))
}

/// Applying the plan failed.
#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    /// The process has more than one thread; per-thread credential syscalls
    /// would leave other threads privileged.
    #[error("cannot normalize privileges: process has {0} threads, expected 1")]
    MultiThreaded(u32),
    /// An operation failed.
    #[error("privilege operation {op} failed: {source}")]
    Op {
        /// Description of the operation.
        op: String,
        /// Underlying error.
        source: io::Error,
    },
    /// The resulting credentials differ from the plan.
    #[error("privileges not as expected after normalization: {0}")]
    Verification(String),
}

/// Applies `plan` to the current process and verifies the result.
pub fn apply(plan: &PrivilegePlan) -> Result<ProcStatus, ApplyError> {
    let before = ProcStatus::read_self().map_err(|source| ApplyError::Op {
        op: "read /proc/self/status".into(),
        source,
    })?;
    if before.threads != 1 {
        return Err(ApplyError::MultiThreaded(before.threads));
    }
    for op in &plan.ops {
        apply_op(op).map_err(|source| ApplyError::Op {
            op: format!("{op:?}"),
            source,
        })?;
    }
    let after = ProcStatus::read_self().map_err(|source| ApplyError::Op {
        op: "read /proc/self/status".into(),
        source,
    })?;
    verify(plan, &after).map_err(ApplyError::Verification)?;
    Ok(after)
}

fn apply_op(op: &Op) -> io::Result<()> {
    match op {
        Op::ClearAmbient => rustix::thread::clear_ambient_capability_set()?,
        Op::SetCaps(sets) => rustix::thread::set_capabilities(None, *sets)?,
        Op::DropBoundingExcept(keep) => drop_bounding_except(*keep)?,
        Op::SetSecureBits(bits) => rustix::thread::set_capabilities_secure_bits(*bits)?,
        Op::SetKeepCaps(enable) => rustix::thread::set_keep_capabilities(*enable)?,
        Op::SetGroups(groups) => {
            let gids: Vec<Gid> = groups.iter().map(|&g| Gid::from_raw(g)).collect();
            rustix::thread::set_thread_groups(&gids)?;
        }
        Op::SetResGid(gid) => {
            let g = Gid::from_raw(*gid);
            rustix::thread::set_thread_res_gid(g, g, g)?;
        }
        Op::SetResUid(uid) => {
            let u = Uid::from_raw(*uid);
            rustix::thread::set_thread_res_uid(u, u, u)?;
        }
        Op::SetNoNewPrivs => rustix::thread::set_no_new_privs(true)?,
    }
    Ok(())
}

fn drop_bounding_except(keep: CapabilitySet) -> io::Result<()> {
    let last = last_capability()?;
    for n in 0..=last {
        let cap = CapabilitySet::from_bits_retain(1u64 << n);
        if !keep.contains(cap) {
            rustix::thread::remove_capability_from_bounding_set(cap)?;
        }
    }
    Ok(())
}

fn last_capability() -> io::Result<u32> {
    let text = std::fs::read_to_string("/proc/sys/kernel/cap_last_cap")?;
    let last: u32 = text
        .trim()
        .parse()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("cap_last_cap: {e}")))?;
    if last >= 64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("cap_last_cap {last} out of range"),
        ));
    }
    Ok(last)
}

/// Checks that `status` matches what `plan` promised.
pub fn verify(plan: &PrivilegePlan, status: &ProcStatus) -> Result<(), String> {
    let mut problems = Vec::new();
    let expected_caps = plan.final_caps.bits();
    if status.uids[..3].iter().any(|&u| u != plan.final_uid) {
        problems.push(format!(
            "UIDs {:?}, expected all {}",
            &status.uids[..3],
            plan.final_uid
        ));
    }
    for (label, actual) in [
        ("effective", status.cap_effective),
        ("permitted", status.cap_permitted),
    ] {
        if actual != expected_caps {
            problems.push(format!(
                "{label} capabilities {actual:#x}, expected {expected_caps:#x}"
            ));
        }
    }
    if status.cap_inheritable != 0 {
        problems.push(format!(
            "inheritable capabilities {:#x}",
            status.cap_inheritable
        ));
    }
    if status.cap_ambient != 0 {
        problems.push(format!("ambient capabilities {:#x}", status.cap_ambient));
    }
    if !status.no_new_privs {
        problems.push("no_new_privs is not set".into());
    }
    if plan
        .ops
        .iter()
        .any(|op| matches!(op, Op::DropBoundingExcept(_)))
        && status.cap_bounding & !expected_caps != 0
    {
        problems.push(format!(
            "bounding set {:#x} exceeds {expected_caps:#x}",
            status.cap_bounding
        ));
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("; "))
    }
}

const CAPABILITY_NAMES: [&str; 41] = [
    "chown",
    "dac_override",
    "dac_read_search",
    "fowner",
    "fsetid",
    "kill",
    "setgid",
    "setuid",
    "setpcap",
    "linux_immutable",
    "net_bind_service",
    "net_broadcast",
    "net_admin",
    "net_raw",
    "ipc_lock",
    "ipc_owner",
    "sys_module",
    "sys_rawio",
    "sys_chroot",
    "sys_ptrace",
    "sys_pacct",
    "sys_admin",
    "sys_boot",
    "sys_nice",
    "sys_resource",
    "sys_time",
    "sys_tty_config",
    "mknod",
    "lease",
    "audit_write",
    "audit_control",
    "setfcap",
    "mac_override",
    "mac_admin",
    "syslog",
    "wake_alarm",
    "block_suspend",
    "audit_read",
    "perfmon",
    "bpf",
    "checkpoint_restore",
];

/// Renders a capability set as names, e.g. `cap_sys_admin`.
#[must_use]
pub fn describe_caps(set: CapabilitySet) -> String {
    if set.is_empty() {
        return "none".into();
    }
    (0..64usize)
        .filter(|n| set.bits() & (1u64 << n) != 0)
        .map(|n| {
            CAPABILITY_NAMES
                .get(n)
                .map_or_else(|| format!("cap_{n}"), |name| format!("cap_{name}"))
        })
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::privileges::plan::{LOCKED_SECURE_BITS, PrivilegePlan};

    const STATUS: &str = "\
Name:\tbyssusd
Umask:\t0022
State:\tS (sleeping)
Uid:\t991\t991\t991\t991
Gid:\t991\t991\t991\t991
Groups:\t500
Threads:\t1
CapInh:\t0000000000000000
CapPrm:\t0000000000200000
CapEff:\t0000000000200000
CapBnd:\t0000000000200000
CapAmb:\t0000000000000000
NoNewPrivs:\t1
Seccomp:\t2
";

    fn plan(final_uid: u32, with_bounding: bool) -> PrivilegePlan {
        let mut ops = vec![Op::ClearAmbient];
        if with_bounding {
            ops.push(Op::DropBoundingExcept(CapabilitySet::SYS_ADMIN));
            ops.push(Op::SetSecureBits(LOCKED_SECURE_BITS));
        }
        PrivilegePlan {
            ops,
            warnings: vec![],
            final_uid,
            final_caps: CapabilitySet::SYS_ADMIN,
        }
    }

    #[test]
    fn parses_status() {
        let s = ProcStatus::parse(STATUS).unwrap();
        assert_eq!(s.uids, [991; 4]);
        assert_eq!(s.threads, 1);
        assert_eq!(s.cap_effective, 1 << 21);
        assert!(s.no_new_privs);
        let creds = s.credentials();
        assert_eq!(creds.uids, [991; 3]);
        assert_eq!(creds.caps.permitted, CapabilitySet::SYS_ADMIN);
    }

    #[test]
    fn parse_errors() {
        assert!(ProcStatus::parse("Uid:\t1\t2\t3\t4\n").is_err());
        assert!(
            ProcStatus::parse(&STATUS.replace("Uid:\t991\t991\t991\t991", "Uid:\t991")).is_err()
        );
        assert!(
            ProcStatus::parse(&STATUS.replace("CapEff:\t0000000000200000", "CapEff:\tzz")).is_err()
        );
    }

    #[test]
    fn reads_own_status() {
        let s = ProcStatus::read_self().unwrap();
        assert!(s.threads >= 1);
    }

    #[test]
    fn verify_accepts_expected_state() {
        let s = ProcStatus::parse(STATUS).unwrap();
        assert_eq!(verify(&plan(991, true), &s), Ok(()));
    }

    #[test]
    fn verify_reports_each_problem() {
        let base = ProcStatus::parse(STATUS).unwrap();
        let check = |mutate: &dyn Fn(&mut ProcStatus), with_bounding: bool, needle: &str| {
            let mut s = base.clone();
            mutate(&mut s);
            let err = verify(&plan(991, with_bounding), &s).unwrap_err();
            assert!(err.contains(needle), "{needle}: {err}");
        };
        check(&|s| s.uids[2] = 0, false, "UIDs");
        check(&|s| s.cap_effective |= 1, false, "effective");
        check(&|s| s.cap_permitted |= 1, false, "permitted");
        check(&|s| s.cap_inheritable = 1, false, "inheritable");
        check(&|s| s.cap_ambient = 1, false, "ambient");
        check(&|s| s.no_new_privs = false, false, "no_new_privs");
        check(&|s| s.cap_bounding = u64::MAX, true, "bounding");

        // Bounding set is not checked when it could not be changed.
        let mut s = base.clone();
        s.cap_bounding = u64::MAX;
        assert_eq!(verify(&plan(991, false), &s), Ok(()));
    }

    #[test]
    fn describes_caps() {
        assert_eq!(describe_caps(CapabilitySet::empty()), "none");
        assert_eq!(describe_caps(CapabilitySet::SYS_ADMIN), "cap_sys_admin");
        assert_eq!(
            describe_caps(CapabilitySet::SYS_ADMIN | CapabilitySet::from_bits_retain(1)),
            "cap_chown,cap_sys_admin"
        );
        assert_eq!(
            describe_caps(CapabilitySet::from_bits_retain(1 << 50)),
            "cap_50"
        );
    }
}
