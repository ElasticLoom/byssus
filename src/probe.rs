//! Kernel feature probes and environment checks.
//!
//! See `docs/DESIGN.md`, "Kernel requirements" and "Propagation and mount
//! namespaces".

use std::fmt;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::Path;

use rustix::fs::{AtFlags, Mode, OFlags, ResolveFlags, StatxAttributes, StatxFlags};
use rustix::mount::{MoveMountFlags, OpenTreeFlags};

use crate::mountinfo::{MountTable, Propagation};
use crate::sys;

/// Result of probing one feature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feature {
    /// Available.
    Available,
    /// The kernel does not implement it.
    Missing,
}

impl Feature {
    /// Classifies the error from a probe call made with deliberately invalid
    /// arguments. Only `ENOSYS` means the syscall is missing; anything else
    /// (`EINVAL`, `EBADF`, or `EPERM` from a capability check that runs before
    /// argument validation) means it exists. A seccomp filter that returns
    /// `EPERM` cannot be told apart and surfaces when the call is used.
    fn from_probe_error(err: &io::Error) -> Self {
        if err.raw_os_error() == Some(libc::ENOSYS) {
            Self::Missing
        } else {
            Self::Available
        }
    }

    /// Whether the feature can be used.
    #[must_use]
    pub fn is_available(self) -> bool {
        matches!(self, Self::Available)
    }
}

impl fmt::Display for Feature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Available => "ok",
            Self::Missing => "missing",
        })
    }
}

/// Kernel features Byssus relies on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelFeatures {
    /// `open_tree` (5.2).
    pub open_tree: Feature,
    /// `move_mount` (5.2).
    pub move_mount: Feature,
    /// `openat2` with `RESOLVE_*` (5.6).
    pub openat2: Feature,
    /// `mount_setattr` (5.12).
    pub mount_setattr: Feature,
    /// `statx` `STATX_MNT_ID` (5.8).
    pub statx_mnt_id: Feature,
    /// `statx` `STATX_ATTR_MOUNT_ROOT` (5.8).
    pub statx_mount_root: Feature,
    /// `statx` `STATX_MNT_ID_UNIQUE` (6.8). Optional.
    pub statx_mnt_id_unique: Feature,
}

impl KernelFeatures {
    /// Required features, by name, that are not available.
    #[must_use]
    pub fn missing_required(&self) -> Vec<(&'static str, Feature)> {
        self.required()
            .into_iter()
            .filter(|(_, f)| !f.is_available())
            .collect()
    }

    /// Required features by name.
    #[must_use]
    pub fn required(&self) -> Vec<(&'static str, Feature)> {
        vec![
            ("open_tree", self.open_tree),
            ("move_mount", self.move_mount),
            ("openat2", self.openat2),
            ("mount_setattr", self.mount_setattr),
            ("statx_mnt_id", self.statx_mnt_id),
            ("statx_mount_root", self.statx_mount_root),
        ]
    }

    /// Whether unique mount IDs can be used.
    #[must_use]
    pub fn unique_mount_ids(&self) -> bool {
        self.statx_mnt_id_unique.is_available()
    }
}

/// Probes kernel features. Needs no privileges and changes nothing.
#[must_use]
pub fn probe_kernel() -> KernelFeatures {
    let invalid_tree_flags = OpenTreeFlags::from_bits_retain(0x8000_0000);
    let open_tree = match rustix::mount::open_tree(rustix::fs::CWD, "", invalid_tree_flags) {
        Ok(_) => Feature::Available,
        Err(e) => Feature::from_probe_error(&e.into()),
    };
    let invalid_move_flags = MoveMountFlags::from_bits_retain(0x8000_0000);
    let move_mount = match rustix::mount::move_mount(
        rustix::fs::CWD,
        "",
        rustix::fs::CWD,
        "",
        invalid_move_flags,
    ) {
        Ok(()) => Feature::Available,
        Err(e) => Feature::from_probe_error(&e.into()),
    };
    let openat2 = match rustix::fs::openat2(
        rustix::fs::CWD,
        "/",
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
        ResolveFlags::NO_MAGICLINKS,
    ) {
        Ok(_) => Feature::Available,
        Err(e) => Feature::from_probe_error(&e.into()),
    };
    let mount_setattr = Feature::from_probe_error(&sys::probe_mount_setattr());

    let (statx_mnt_id, statx_mount_root, statx_mnt_id_unique) = probe_statx();

    KernelFeatures {
        open_tree,
        move_mount,
        openat2,
        mount_setattr,
        statx_mnt_id,
        statx_mount_root,
        statx_mnt_id_unique,
    }
}

fn probe_statx() -> (Feature, Feature, Feature) {
    let flags = StatxFlags::BASIC_STATS | StatxFlags::MNT_ID;
    // statx itself exists on every supported kernel; failing on "/" means
    // these fields cannot be used.
    let Ok(st) = rustix::fs::statx(rustix::fs::CWD, "/", AtFlags::empty(), flags) else {
        return (Feature::Missing, Feature::Missing, Feature::Missing);
    };
    let mnt_id = if st.stx_mask & StatxFlags::MNT_ID.bits() != 0 {
        Feature::Available
    } else {
        Feature::Missing
    };
    let mount_root = if st.stx_attributes_mask.contains(StatxAttributes::MOUNT_ROOT) {
        Feature::Available
    } else {
        Feature::Missing
    };
    let unique_flag = StatxFlags::from_bits_retain(sys::STATX_MNT_ID_UNIQUE);
    let unique = match rustix::fs::statx(rustix::fs::CWD, "/", AtFlags::empty(), unique_flag) {
        Ok(st) if st.stx_mask & sys::STATX_MNT_ID_UNIQUE != 0 => Feature::Available,
        Ok(_) => Feature::Missing,
        Err(e) => Feature::from_probe_error(&e.into()),
    };
    (mnt_id, mount_root, unique)
}

/// Whether this process runs under a seccomp filter (`Seccomp: 2` in
/// `/proc/self/status`). Used to explain a missing feature: a filter can make
/// a syscall return `ENOSYS` on a kernel that supports it.
#[must_use]
pub fn seccomp_filter_active() -> bool {
    std::fs::read_to_string("/proc/self/status").is_ok_and(|status| {
        status.lines().any(|line| {
            line.split_once(':')
                .is_some_and(|(k, v)| k == "Seccomp" && v.trim() == "2")
        })
    })
}

/// Explains why required features are unavailable.
#[must_use]
pub fn missing_features_hint(seccomp_filtered: bool) -> &'static str {
    if seccomp_filtered {
        "blocked by this process's seccomp filter, or not supported by the kernel (Linux 5.12 or newer is required). \
         Under systemd, check for SystemCallFilter= changes and do not use RestrictSUIDSGID=, which blocks openat2"
    } else {
        "Linux 5.12 or newer is required"
    }
}

/// Opens `/proc` and verifies it is procfs, so `/proc/self/fd` links can be
/// trusted for unmounting.
pub fn verify_procfs(proc_path: &Path) -> io::Result<OwnedFd> {
    let fd = rustix::fs::open(
        proc_path,
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )?;
    check_procfs(fd.as_fd())?;
    Ok(fd)
}

fn check_procfs(fd: BorrowedFd<'_>) -> io::Result<()> {
    let statfs = rustix::fs::fstatfs(fd)?;
    // `f_type` is signed or unsigned, 32 or 64 bits, depending on the
    // architecture; magic numbers are small positive values.
    #[allow(
        clippy::useless_conversion,
        clippy::unnecessary_cast,
        clippy::cast_sign_loss
    )]
    let f_type = statfs.f_type as u64;
    if f_type == sys::PROC_SUPER_MAGIC {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("not a procfs filesystem (f_type {f_type:#x})"),
        ))
    }
}

/// Outcome of checking a target root's propagation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PropagationCheck {
    /// The mount is shared: views propagate to `rslave` consumers.
    Shared,
    /// The mount is private: host-only use works; consumers see nothing.
    Private,
    /// The mount is only a slave: almost certainly a non-host mount namespace.
    SlaveOnly,
    /// The mount is unbindable.
    Unbindable,
    /// The mount could not be found or inspected.
    Unknown(String),
}

impl PropagationCheck {
    /// A human-readable description.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Shared => "shared".into(),
            Self::Private => "private (mounts will not reach containers via rslave)".into(),
            Self::SlaveOnly => "slave only (the daemon appears to run in a non-host mount namespace; mounts will be invisible outside it)".into(),
            Self::Unbindable => "unbindable (mounts will not reach containers)".into(),
            Self::Unknown(reason) => format!("unknown ({reason})"),
        }
    }
}

/// Checks the propagation of the mount containing `target_root`.
#[must_use]
pub fn check_propagation(target_root: BorrowedFd<'_>, table: &MountTable) -> PropagationCheck {
    let st = match rustix::fs::statx(target_root, "", AtFlags::EMPTY_PATH, StatxFlags::MNT_ID) {
        Ok(st) if st.stx_mask & StatxFlags::MNT_ID.bits() != 0 => st,
        Ok(_) => return PropagationCheck::Unknown("kernel did not report mount ID".into()),
        Err(e) => return PropagationCheck::Unknown(e.to_string()),
    };
    classify(table, st.stx_mnt_id)
}

fn classify(table: &MountTable, mount_id: u64) -> PropagationCheck {
    match table.by_id(mount_id).map(|m| m.propagation) {
        Some(Propagation::Shared { .. } | Propagation::SharedAndSlave { .. }) => {
            PropagationCheck::Shared
        }
        Some(Propagation::Private) => PropagationCheck::Private,
        Some(Propagation::Slave { .. }) => PropagationCheck::SlaveOnly,
        Some(Propagation::Unbindable) => PropagationCheck::Unbindable,
        None => PropagationCheck::Unknown(format!("mount ID {mount_id} not found in mountinfo")),
    }
}

/// Whether this process shares PID 1's mount namespace, if it can tell.
#[must_use]
pub fn same_mount_namespace_as_init() -> Option<bool> {
    let ino = |path: &str| {
        rustix::fs::statx(rustix::fs::CWD, path, AtFlags::empty(), StatxFlags::INO)
            .ok()
            .map(|st| (st.stx_dev_major, st.stx_dev_minor, st.stx_ino))
    };
    Some(ino("/proc/self/ns/mnt")? == ino("/proc/1/ns/mnt")?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_kernel_supports_required_features() {
        let features = probe_kernel();
        assert!(features.missing_required().is_empty(), "{features:#?}");
    }

    #[test]
    fn feature_classification() {
        let err = |errno| io::Error::from_raw_os_error(errno);
        assert_eq!(
            Feature::from_probe_error(&err(libc::ENOSYS)),
            Feature::Missing
        );
        assert_eq!(
            Feature::from_probe_error(&err(libc::EINVAL)),
            Feature::Available
        );
        assert_eq!(
            Feature::from_probe_error(&err(libc::EBADF)),
            Feature::Available
        );
        assert_eq!(
            Feature::from_probe_error(&err(libc::EPERM)),
            Feature::Available
        );
        assert_eq!(Feature::Available.to_string(), "ok");
    }

    #[test]
    fn missing_feature_hints() {
        assert!(missing_features_hint(false).contains("5.12"));
        assert!(missing_features_hint(true).contains("RestrictSUIDSGID"));
        // Must not panic; the value depends on how tests are run.
        let _ = seccomp_filter_active();
    }

    #[test]
    fn procfs_verification() {
        assert!(verify_procfs(Path::new("/proc")).is_ok());
        assert!(verify_procfs(Path::new("/")).is_err());
    }

    #[test]
    fn propagation_classification() {
        let table = MountTable::parse(
            b"1 0 0:1 / / rw shared:1 - ext4 /dev/a rw\n\
              2 1 0:2 / /p rw - tmpfs t rw\n\
              3 1 0:3 / /s rw master:1 - tmpfs t rw\n\
              4 1 0:4 / /ss rw shared:4 master:1 - tmpfs t rw\n\
              5 1 0:5 / /u rw unbindable - tmpfs t rw\n",
        )
        .unwrap();
        assert_eq!(classify(&table, 1), PropagationCheck::Shared);
        assert_eq!(classify(&table, 2), PropagationCheck::Private);
        assert_eq!(classify(&table, 3), PropagationCheck::SlaveOnly);
        assert_eq!(classify(&table, 4), PropagationCheck::Shared);
        assert_eq!(classify(&table, 5), PropagationCheck::Unbindable);
        assert!(matches!(classify(&table, 9), PropagationCheck::Unknown(_)));
    }

    #[test]
    fn propagation_of_real_mount_is_classified() {
        let table = MountTable::read_self().unwrap();
        let root = crate::fsops::open_root(Path::new("/")).unwrap();
        assert!(!matches!(
            check_propagation(root.as_fd(), &table),
            PropagationCheck::Unknown(_)
        ));
    }

    #[test]
    fn namespace_comparison_does_not_panic() {
        // May be None when /proc/1/ns is not accessible.
        let _ = same_mount_namespace_as_init();
    }
}
