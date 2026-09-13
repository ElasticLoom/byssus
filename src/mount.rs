//! Mount inspection, creation, attribute enforcement and removal.
//!
//! See `docs/DESIGN.md`, "Mount creation", "Mount identity" and
//! "Unmounting". Operations here require `CAP_SYS_ADMIN` except
//! [`inspect`], [`read_identity`] and [`observed_attrs`].

use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};

use rustix::fs::{AtFlags, StatxAttributes, StatxFlags};
use rustix::mount::{MoveMountFlags, OpenTreeFlags, UnmountFlags};

use crate::config::MountAttrs;
use crate::fsops;
use crate::identity::{DevIno, MountIdentity};
use crate::reconcile::plan::{ObservedAttrs, TargetState};
use crate::sys;

/// Attribute bits to set for the given configuration. `nosuid` and `nodev`
/// are always included.
#[must_use]
pub fn attr_bits(attrs: &MountAttrs) -> u64 {
    let mut bits = sys::MOUNT_ATTR_NOSUID | sys::MOUNT_ATTR_NODEV;
    if attrs.read_only {
        bits |= sys::MOUNT_ATTR_RDONLY;
    }
    if attrs.noexec {
        bits |= sys::MOUNT_ATTR_NOEXEC;
    }
    if attrs.nosymfollow {
        bits |= sys::MOUNT_ATTR_NOSYMFOLLOW;
    }
    bits
}

/// Reads the identity of the mount whose root `fd` refers to.
///
/// `unique_supported` requests `STATX_MNT_ID_UNIQUE`; if the kernel does not
/// return it, `mnt_id_unique` is `None`.
pub fn read_identity(fd: BorrowedFd<'_>, unique_supported: bool) -> io::Result<MountIdentity> {
    let st = rustix::fs::statx(
        fd,
        "",
        AtFlags::EMPTY_PATH,
        StatxFlags::INO | StatxFlags::MNT_ID,
    )?;
    if st.stx_mask & StatxFlags::MNT_ID.bits() == 0 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "kernel did not report STATX_MNT_ID",
        ));
    }
    let mnt_id_unique = if unique_supported {
        let unique = StatxFlags::from_bits_retain(sys::STATX_MNT_ID_UNIQUE);
        let st2 = rustix::fs::statx(fd, "", AtFlags::EMPTY_PATH, unique)?;
        (st2.stx_mask & sys::STATX_MNT_ID_UNIQUE != 0).then_some(st2.stx_mnt_id)
    } else {
        None
    };
    Ok(MountIdentity {
        mnt_id: st.stx_mnt_id,
        mnt_id_unique,
        root: DevIno {
            dev_major: st.stx_dev_major,
            dev_minor: st.stx_dev_minor,
            ino: st.stx_ino,
        },
    })
}

/// Reads the attributes of the mount `fd` is on.
pub fn observed_attrs(fd: BorrowedFd<'_>) -> io::Result<ObservedAttrs> {
    use rustix::fs::StatVfsMountFlags as F;
    let flags = rustix::fs::fstatvfs(fd)?.f_flag;
    Ok(ObservedAttrs {
        read_only: flags.contains(F::RDONLY),
        nosuid: flags.contains(F::NOSUID),
        nodev: flags.contains(F::NODEV),
        noexec: flags.contains(F::NOEXEC),
        nosymfollow: flags.bits() & sys::ST_NOSYMFOLLOW != 0,
    })
}

/// Whether `fd` refers to the root of a mount.
pub fn is_mount_root(fd: BorrowedFd<'_>) -> io::Result<bool> {
    let st = rustix::fs::statx(fd, "", AtFlags::EMPTY_PATH, StatxFlags::BASIC_STATS)?;
    if !st.stx_attributes_mask.contains(StatxAttributes::MOUNT_ROOT) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "kernel did not report STATX_ATTR_MOUNT_ROOT",
        ));
    }
    Ok(st.stx_attributes.contains(StatxAttributes::MOUNT_ROOT))
}

/// Observes what is at `relative` beneath `target_root`.
#[must_use]
pub fn inspect(target_root: BorrowedFd<'_>, relative: &str, unique_supported: bool) -> TargetState {
    let fd = match fsops::resolve_dir(target_root, relative) {
        Ok(fd) => fd,
        Err(e) if e.raw_os_error() == Some(libc::ENOENT) => return TargetState::NotMounted,
        Err(e) => return TargetState::Unavailable(e.to_string()),
    };
    inspect_fd(fd.as_fd(), unique_supported)
        .unwrap_or_else(|e| TargetState::Unavailable(e.to_string()))
}

fn inspect_fd(fd: BorrowedFd<'_>, unique_supported: bool) -> io::Result<TargetState> {
    if !is_mount_root(fd)? {
        return Ok(TargetState::NotMounted);
    }
    Ok(TargetState::Mounted {
        identity: read_identity(fd, unique_supported)?,
        attrs: observed_attrs(fd).ok(),
    })
}

/// Creates a read-only (as configured) bind mount of `source` onto `target`
/// and returns the new mount's identity.
///
/// The clone is non-recursive, and attributes are applied while it is still
/// detached, so the mount is never visible without its restrictions.
pub fn create_bind(
    source: BorrowedFd<'_>,
    target: BorrowedFd<'_>,
    attrs: &MountAttrs,
    unique_supported: bool,
) -> io::Result<MountIdentity> {
    let tree: OwnedFd = rustix::mount::open_tree(
        source,
        "",
        OpenTreeFlags::AT_EMPTY_PATH
            | OpenTreeFlags::OPEN_TREE_CLONE
            | OpenTreeFlags::OPEN_TREE_CLOEXEC,
    )?;
    sys::mount_setattr_add(tree.as_fd(), attr_bits(attrs))?;
    rustix::mount::move_mount(
        tree.as_fd(),
        "",
        target,
        "",
        MoveMountFlags::MOVE_MOUNT_F_EMPTY_PATH | MoveMountFlags::MOVE_MOUNT_T_EMPTY_PATH,
    )?;
    // The tree descriptor now refers to the attached mount.
    read_identity(tree.as_fd(), unique_supported)
}

/// Why a verified operation on an existing mount did not happen.
#[derive(Debug, thiserror::Error)]
pub enum VerifiedOpError {
    /// Nothing is mounted at the target.
    #[error("nothing is mounted at the target")]
    NotMounted,
    /// A different mount is at the target.
    #[error("the mount at the target is not the recorded mount (found mount ID {found})")]
    IdentityMismatch {
        /// Mount ID found.
        found: u64,
    },
    /// A system call failed.
    #[error(transparent)]
    Io(#[from] io::Error),
}

fn open_verified(
    target_root: BorrowedFd<'_>,
    relative: &str,
    expected: &MountIdentity,
    unique_supported: bool,
) -> Result<OwnedFd, VerifiedOpError> {
    let fd = fsops::resolve_dir(target_root, relative)?;
    match inspect_fd(fd.as_fd(), unique_supported)? {
        TargetState::Mounted { identity, .. } if identity.matches(expected) => Ok(fd),
        TargetState::Mounted { identity, .. } => Err(VerifiedOpError::IdentityMismatch {
            found: identity.mnt_id,
        }),
        TargetState::NotMounted | TargetState::Unavailable(_) => Err(VerifiedOpError::NotMounted),
    }
}

/// Lazily unmounts the recorded mount at `relative` beneath `target_root`.
///
/// The target is opened and pinned, its identity verified, and then
/// `umount2("/proc/self/fd/N", MNT_DETACH)` is called: the kernel resolves the
/// magic link to exactly the verified mount. The caller must have verified
/// that `/proc` is procfs (see [`crate::probe::verify_procfs`]).
pub fn unmount_verified(
    target_root: BorrowedFd<'_>,
    relative: &str,
    expected: &MountIdentity,
    unique_supported: bool,
) -> Result<(), VerifiedOpError> {
    let fd = open_verified(target_root, relative, expected, unique_supported)?;
    let link = format!("/proc/self/fd/{}", fd.as_raw_fd());
    rustix::mount::unmount(link.as_str(), UnmountFlags::DETACH).map_err(io::Error::from)?;
    drop(fd);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attr_bits_always_include_nosuid_nodev() {
        let none = MountAttrs {
            read_only: false,
            noexec: false,
            nosymfollow: false,
        };
        assert_eq!(
            attr_bits(&none),
            sys::MOUNT_ATTR_NOSUID | sys::MOUNT_ATTR_NODEV
        );
        let all = MountAttrs {
            read_only: true,
            noexec: true,
            nosymfollow: true,
        };
        assert_eq!(
            attr_bits(&all),
            sys::MOUNT_ATTR_NOSUID
                | sys::MOUNT_ATTR_NODEV
                | sys::MOUNT_ATTR_RDONLY
                | sys::MOUNT_ATTR_NOEXEC
                | sys::MOUNT_ATTR_NOSYMFOLLOW
        );
    }

    #[test]
    fn inspect_unprivileged() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("plain")).unwrap();
        std::os::unix::fs::symlink("plain", dir.path().join("link")).unwrap();
        let root = fsops::open_root(dir.path()).unwrap();
        assert_eq!(
            inspect(root.as_fd(), "plain", true),
            TargetState::NotMounted
        );
        assert_eq!(
            inspect(root.as_fd(), "missing", true),
            TargetState::NotMounted
        );
        assert!(matches!(
            inspect(root.as_fd(), "link", true),
            TargetState::Unavailable(_)
        ));
    }

    #[test]
    fn root_filesystem_is_a_mount_root_with_identity() {
        let root = fsops::open_root(std::path::Path::new("/")).unwrap();
        assert!(is_mount_root(root.as_fd()).unwrap());
        let id = read_identity(root.as_fd(), true).unwrap();
        assert!(id.mnt_id > 0);
        let again = read_identity(root.as_fd(), true).unwrap();
        assert!(again.matches(&id));
        observed_attrs(root.as_fd()).unwrap();
    }
}
