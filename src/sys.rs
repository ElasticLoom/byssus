//! Raw system calls that `rustix` does not wrap.
//!
//! This is one of only two modules containing `unsafe` code (the other is
//! [`crate::signals`]). Keep it minimal and audited.

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd};

/// `MOUNT_ATTR_RDONLY`.
pub const MOUNT_ATTR_RDONLY: u64 = 0x0000_0001;
/// `MOUNT_ATTR_NOSUID`.
pub const MOUNT_ATTR_NOSUID: u64 = 0x0000_0002;
/// `MOUNT_ATTR_NODEV`.
pub const MOUNT_ATTR_NODEV: u64 = 0x0000_0004;
/// `MOUNT_ATTR_NOEXEC`.
pub const MOUNT_ATTR_NOEXEC: u64 = 0x0000_0008;
/// `MOUNT_ATTR_NOSYMFOLLOW`.
pub const MOUNT_ATTR_NOSYMFOLLOW: u64 = 0x0020_0000;

/// `STATX_MNT_ID_UNIQUE` (Linux 6.8).
pub const STATX_MNT_ID_UNIQUE: u32 = 0x0000_4000;
/// `ST_NOSYMFOLLOW` in `statfs` flags (Linux 5.10).
pub const ST_NOSYMFOLLOW: u64 = 0x2000;
/// `PROC_SUPER_MAGIC`.
pub const PROC_SUPER_MAGIC: u64 = 0x9fa0;

/// `struct mount_attr` (`MOUNT_ATTR_SIZE_VER0`).
#[repr(C)]
#[derive(Debug, Default)]
struct MountAttr {
    attr_set: u64,
    attr_clr: u64,
    propagation: u64,
    userns_fd: u64,
}

const AT_EMPTY_PATH: libc::c_uint = 0x1000;

/// `mount_setattr(fd, "", AT_EMPTY_PATH, {attr_set: set})`: adds attributes to
/// the mount referred to by `fd`. Attributes are never cleared.
pub fn mount_setattr_add(fd: BorrowedFd<'_>, set: u64) -> io::Result<()> {
    let attr = MountAttr {
        attr_set: set,
        ..MountAttr::default()
    };
    // SAFETY: `fd` is a valid open descriptor for the duration of the call,
    // the path is a NUL-terminated empty string with static lifetime, `attr`
    // is a live `#[repr(C)]` `struct mount_attr` and its exact size is passed,
    // so the kernel reads only initialized memory we own.
    let ret = unsafe {
        libc::syscall(
            libc::SYS_mount_setattr,
            fd.as_raw_fd(),
            c"".as_ptr(),
            AT_EMPTY_PATH,
            std::ptr::from_ref(&attr),
            std::mem::size_of::<MountAttr>(),
        )
    };
    if ret == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Calls `mount_setattr` with arguments the kernel must reject, to learn
/// whether the syscall exists. Returns the resulting error (never succeeds).
#[must_use]
pub fn probe_mount_setattr() -> io::Error {
    let attr = MountAttr::default();
    // SAFETY: fd -1 and an invalid flag bit make the kernel return an error
    // without touching any descriptor; the path and `attr` pointers are valid
    // for reads as in `mount_setattr_add`.
    let ret = unsafe {
        libc::syscall(
            libc::SYS_mount_setattr,
            -1,
            c"".as_ptr(),
            0x8000_0000u32,
            std::ptr::from_ref(&attr),
            std::mem::size_of::<MountAttr>(),
        )
    };
    debug_assert!(ret != 0, "mount_setattr probe unexpectedly succeeded");
    io::Error::last_os_error()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_attr_has_kernel_layout() {
        assert_eq!(std::mem::size_of::<MountAttr>(), 32);
    }

    #[test]
    fn probe_reports_non_enosys_on_supported_kernels() {
        // Every supported kernel has mount_setattr; the probe must fail with
        // something other than ENOSYS (EBADF/EINVAL, or EPERM under seccomp).
        let err = probe_mount_setattr();
        assert_ne!(err.raw_os_error(), Some(libc::ENOSYS));
    }
}
