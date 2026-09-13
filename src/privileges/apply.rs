//! Applying and verifying a privilege-normalization plan.

use std::io;
use std::os::fd::BorrowedFd;

use rustix::fs::{Mode, OFlags, ResolveFlags};
use rustix::process::{Gid, Uid};
use rustix::thread::{CapabilitySet, CapabilitySets};

use super::plan::{Credentials, Op, PrivilegePlan};
use crate::fsops;

/// The calling process's credential state.
///
/// Credentials come from system calls rather than `/proc/self/status`, so a
/// file mounted over procfs cannot misreport them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProcessState {
    /// Real, effective and saved UIDs.
    pub uids: [u32; 3],
    /// Real, effective and saved GIDs.
    pub gids: [u32; 3],
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

impl ProcessState {
    /// Reads the calling process's state. `proc` must be a verified procfs
    /// descriptor (see [`crate::probe::verify_procfs`]); it is used only for
    /// the thread count, which no system call reports.
    pub fn read_self(proc: BorrowedFd<'_>) -> io::Result<Self> {
        let caps = rustix::thread::capabilities(None)?;
        Ok(Self {
            uids: res_ids(libc::getresuid)?,
            gids: res_ids(libc::getresgid)?,
            threads: thread_count(proc)?,
            cap_inheritable: caps.inheritable.bits(),
            cap_permitted: caps.permitted.bits(),
            cap_effective: caps.effective.bits(),
            cap_bounding: read_set(rustix::thread::capability_is_in_bounding_set)?,
            cap_ambient: read_set(rustix::thread::capability_is_in_ambient_set)?,
            no_new_privs: rustix::thread::no_new_privs()?,
        })
    }

    /// Current credentials in the form the planner uses.
    #[must_use]
    pub fn credentials(&self) -> Credentials {
        Credentials {
            uids: self.uids,
            caps: CapabilitySets {
                effective: CapabilitySet::from_bits_retain(self.cap_effective),
                permitted: CapabilitySet::from_bits_retain(self.cap_permitted),
                inheritable: CapabilitySet::from_bits_retain(self.cap_inheritable),
            },
        }
    }
}

/// Calls `getresuid` or `getresgid`.
fn res_ids(
    get: unsafe extern "C" fn(*mut u32, *mut u32, *mut u32) -> libc::c_int,
) -> io::Result<[u32; 3]> {
    let mut ids = [0u32; 3];
    let [real, effective, saved] = &mut ids;
    // SAFETY: `get` is `getresuid` or `getresgid`, and each pointer is valid
    // for writing one ID.
    let ret = unsafe { get(real, effective, saved) };
    if ret == 0 {
        Ok(ids)
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Reads a capability set one capability at a time, up to the first
/// capability the kernel does not know.
fn read_set(is_set: fn(CapabilitySet) -> rustix::io::Result<bool>) -> io::Result<u64> {
    let mut bits = 0;
    for n in 0..64 {
        match is_set(CapabilitySet::from_bits_retain(1 << n)) {
            Ok(true) => bits |= 1 << n,
            Ok(false) => {}
            Err(rustix::io::Errno::INVAL) if n > 0 => break,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(bits)
}

/// Reads the thread count from `self/status` beneath the verified procfs
/// descriptor, without crossing into anything mounted over procfs.
fn thread_count(proc: BorrowedFd<'_>) -> io::Result<u32> {
    let resolve = ResolveFlags::BENEATH | ResolveFlags::NO_XDEV | ResolveFlags::NO_MAGICLINKS;
    let fd = fsops::retry_eagain(|| {
        rustix::fs::openat2(
            proc,
            "self/status",
            OFlags::RDONLY | OFlags::CLOEXEC,
            Mode::empty(),
            resolve,
        )
    })?;
    let text = io::read_to_string(std::fs::File::from(fd))?;
    text.lines()
        .find_map(|line| line.strip_prefix("Threads:"))
        .and_then(|value| value.trim().parse().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no thread count in status"))
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

/// Applies `plan` to the current process and verifies the result. `proc` must
/// be a verified procfs descriptor.
pub fn apply(plan: &PrivilegePlan, proc: BorrowedFd<'_>) -> Result<ProcessState, ApplyError> {
    let read_state = || {
        ProcessState::read_self(proc).map_err(|source| ApplyError::Op {
            op: "read process state".into(),
            source,
        })
    };
    let before = read_state()?;
    if before.threads != 1 {
        return Err(ApplyError::MultiThreaded(before.threads));
    }
    for op in &plan.ops {
        apply_op(op).map_err(|source| ApplyError::Op {
            op: format!("{op:?}"),
            source,
        })?;
    }
    let after = read_state()?;
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
    let excess = read_set(rustix::thread::capability_is_in_bounding_set)? & !keep.bits();
    for n in (0..64).filter(|n| excess & (1u64 << n) != 0) {
        rustix::thread::remove_capability_from_bounding_set(CapabilitySet::from_bits_retain(
            1u64 << n,
        ))?;
    }
    Ok(())
}

/// Checks that `status` matches what `plan` promised.
pub fn verify(plan: &PrivilegePlan, status: &ProcessState) -> Result<(), String> {
    let mut problems = Vec::new();
    let expected_caps = plan.final_caps.bits();
    if status.uids.iter().any(|&u| u != plan.final_uid) {
        problems.push(format!(
            "UIDs {:?}, expected all {}",
            status.uids, plan.final_uid
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
    use std::os::fd::AsFd;

    use super::*;
    use crate::privileges::plan::{LOCKED_SECURE_BITS, PrivilegePlan};

    fn normalized() -> ProcessState {
        ProcessState {
            uids: [991; 3],
            gids: [991; 3],
            threads: 1,
            cap_inheritable: 0,
            cap_permitted: 1 << 21,
            cap_effective: 1 << 21,
            cap_bounding: 1 << 21,
            cap_ambient: 0,
            no_new_privs: true,
        }
    }

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
    fn credentials_for_planning() {
        let creds = normalized().credentials();
        assert_eq!(creds.uids, [991; 3]);
        assert_eq!(creds.caps.effective, CapabilitySet::SYS_ADMIN);
        assert_eq!(creds.caps.permitted, CapabilitySet::SYS_ADMIN);
        assert_eq!(creds.caps.inheritable, CapabilitySet::empty());
    }

    /// The system-call readings agree with what the kernel reports in
    /// `/proc/self/status`.
    #[test]
    fn reads_own_state() {
        let proc = crate::probe::verify_procfs(std::path::Path::new("/proc")).unwrap();
        let s = ProcessState::read_self(proc.as_fd()).unwrap();
        let status = std::fs::read_to_string("/proc/self/status").unwrap();
        let field = |key: &str| {
            status
                .lines()
                .find_map(|line| line.strip_prefix(key)?.strip_prefix(':'))
                .unwrap()
                .trim()
                .to_owned()
        };
        let ids = |key| -> Vec<u32> {
            field(key)
                .split_whitespace()
                .take(3)
                .map(|id| id.parse().unwrap())
                .collect()
        };
        let hex = |key| u64::from_str_radix(&field(key), 16).unwrap();
        assert_eq!(s.uids.to_vec(), ids("Uid"));
        assert_eq!(s.gids.to_vec(), ids("Gid"));
        assert!(s.threads >= 1);
        assert_eq!(s.cap_inheritable, hex("CapInh"));
        assert_eq!(s.cap_permitted, hex("CapPrm"));
        assert_eq!(s.cap_effective, hex("CapEff"));
        assert_eq!(s.cap_bounding, hex("CapBnd"));
        assert_eq!(s.cap_ambient, hex("CapAmb"));
        assert_eq!(s.no_new_privs, field("NoNewPrivs") == "1");
    }

    #[test]
    fn verify_accepts_expected_state() {
        assert_eq!(verify(&plan(991, true), &normalized()), Ok(()));
    }

    #[test]
    fn verify_reports_each_problem() {
        let base = normalized();
        let check = |mutate: &dyn Fn(&mut ProcessState), with_bounding: bool, needle: &str| {
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
