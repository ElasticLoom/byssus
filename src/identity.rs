//! Kernel identity of a mount, used to prove Byssus created it.
//!
//! See `docs/DESIGN.md`, "Mount identity".

/// Device and inode of a directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DevIno {
    /// Device major number.
    pub dev_major: u32,
    /// Device minor number.
    pub dev_minor: u32,
    /// Inode number.
    pub ino: u64,
}

/// The identity of an attached mount, as read with `statx` on a descriptor
/// referring to its root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MountIdentity {
    /// `STATX_MNT_ID`: unique while mounted; may be reused afterwards.
    pub mnt_id: u64,
    /// `STATX_MNT_ID_UNIQUE`: never reused. `None` if the kernel does not
    /// support it.
    pub mnt_id_unique: Option<u64>,
    /// Device and inode of the mount's root directory. For a bind mount these
    /// equal the source directory's.
    pub root: DevIno,
}

impl MountIdentity {
    /// Whether an observed mount (`self`) is the mount described by a
    /// `recorded` identity.
    ///
    /// When both sides carry a unique mount ID it is authoritative (together
    /// with the root device and inode). Otherwise the reusable mount ID, root
    /// device and root inode must all match.
    #[must_use]
    pub fn matches(&self, recorded: &Self) -> bool {
        if self.root != recorded.root {
            return false;
        }
        match (self.mnt_id_unique, recorded.mnt_id_unique) {
            (Some(observed), Some(expected)) => observed == expected,
            _ => self.mnt_id == recorded.mnt_id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: DevIno = DevIno {
        dev_major: 8,
        dev_minor: 1,
        ino: 42,
    };

    fn id(mnt_id: u64, unique: Option<u64>, root: DevIno) -> MountIdentity {
        MountIdentity {
            mnt_id,
            mnt_id_unique: unique,
            root,
        }
    }

    #[test]
    fn identical_matches() {
        assert!(id(5, Some(100), ROOT).matches(&id(5, Some(100), ROOT)));
        assert!(id(5, None, ROOT).matches(&id(5, None, ROOT)));
    }

    #[test]
    fn unique_id_is_authoritative_when_both_present() {
        // Reused short ID but different unique ID: a different mount.
        assert!(!id(5, Some(101), ROOT).matches(&id(5, Some(100), ROOT)));
        // Short IDs differ but unique IDs match: cannot happen for one mount,
        // but the unique ID decides.
        assert!(id(6, Some(100), ROOT).matches(&id(5, Some(100), ROOT)));
    }

    #[test]
    fn falls_back_to_short_id_when_either_lacks_unique() {
        assert!(id(5, None, ROOT).matches(&id(5, Some(100), ROOT)));
        assert!(id(5, Some(100), ROOT).matches(&id(5, None, ROOT)));
        assert!(!id(6, None, ROOT).matches(&id(5, Some(100), ROOT)));
    }

    #[test]
    fn root_must_match() {
        let other_ino = DevIno { ino: 43, ..ROOT };
        let other_dev = DevIno {
            dev_minor: 2,
            ..ROOT
        };
        assert!(!id(5, Some(100), other_ino).matches(&id(5, Some(100), ROOT)));
        assert!(!id(5, None, other_dev).matches(&id(5, None, ROOT)));
    }
}
