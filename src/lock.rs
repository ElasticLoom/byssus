//! Exclusive lock on the state directory.
//!
//! `byssusd` holds the lock for its lifetime; `byssus reconcile` takes it for
//! one pass. Two writers therefore never race on the state file.

use std::io;
use std::os::fd::{AsFd, OwnedFd};

use rustix::fs::{FlockOperation, Mode, OFlags};

/// Lock file name within the state directory.
pub const LOCK_FILE_NAME: &str = "lock";

/// A held lock; released when dropped.
#[derive(Debug)]
pub struct StateLock {
    _fd: OwnedFd,
}

/// Why the lock could not be taken.
#[derive(Debug, thiserror::Error)]
pub enum LockError {
    /// Another process holds the lock.
    #[error("another Byssus process holds the state lock")]
    Held,
    /// A system call failed.
    #[error("cannot lock state directory: {0}")]
    Io(#[from] io::Error),
}

impl StateLock {
    /// Takes the lock without blocking. `state_dir` must be a descriptor for
    /// the state directory.
    pub fn acquire(state_dir: impl AsFd) -> Result<Self, LockError> {
        let fd = rustix::fs::openat(
            state_dir,
            LOCK_FILE_NAME,
            OFlags::RDWR | OFlags::CREATE | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::from_raw_mode(0o640),
        )
        .map_err(io::Error::from)?;
        match rustix::fs::flock(&fd, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => Ok(Self { _fd: fd }),
            Err(rustix::io::Errno::WOULDBLOCK) => Err(LockError::Held),
            Err(e) => Err(LockError::Io(e.into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir_fd(dir: &tempfile::TempDir) -> OwnedFd {
        rustix::fs::open(
            dir.path(),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .unwrap()
    }

    #[test]
    fn exclusive_and_released_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let fd = dir_fd(&dir);
        let first = StateLock::acquire(&fd).unwrap();
        assert!(matches!(StateLock::acquire(&fd), Err(LockError::Held)));
        drop(first);
        assert!(StateLock::acquire(&fd).is_ok());
    }

    #[test]
    fn refuses_symlinked_lock_file() {
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("/etc/passwd", dir.path().join(LOCK_FILE_NAME)).unwrap();
        let fd = dir_fd(&dir);
        assert!(matches!(StateLock::acquire(&fd), Err(LockError::Io(_))));
    }
}
