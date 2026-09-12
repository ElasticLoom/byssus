//! Classification of membership directory entries.
//!
//! An entry makes its name a member only if the name is valid, the entry is a
//! regular file (not followed through symlinks), and it is empty. See
//! `docs/DESIGN.md`, "Membership files".

use std::collections::BTreeSet;
use std::fmt;

use crate::name::{Name, NameError, display_bytes};

/// The type of a directory entry, as reported by `statx` without following
/// symlinks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// A regular file of the given size.
    Regular {
        /// Size in bytes.
        size: u64,
    },
    /// A directory.
    Directory,
    /// A symbolic link.
    Symlink,
    /// A named pipe.
    Fifo,
    /// A Unix domain socket.
    Socket,
    /// A character device.
    CharDevice,
    /// A block device.
    BlockDevice,
    /// Any other type.
    Unknown,
}

impl fmt::Display for EntryKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Regular { .. } => "regular file",
            Self::Directory => "directory",
            Self::Symlink => "symbolic link",
            Self::Fifo => "FIFO",
            Self::Socket => "socket",
            Self::CharDevice => "character device",
            Self::BlockDevice => "block device",
            Self::Unknown => "unknown file type",
        })
    }
}

/// Why an entry was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RejectReason {
    /// The file name fails the name rules.
    #[error("name fails allowlist: {0}")]
    InvalidName(#[from] NameError),
    /// The entry is not a regular file.
    #[error("not a regular file ({0})")]
    NotRegularFile(EntryKind),
    /// The file is not empty.
    #[error("file is not empty ({0} bytes)")]
    NotEmpty(u64),
    /// The entry could not be inspected.
    #[error("cannot inspect entry: {0}")]
    Inaccessible(String),
}

/// A rejected membership entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    /// The entry name rendered safely for logs.
    pub display_name: String,
    /// Why it was rejected.
    pub reason: RejectReason,
}

/// Classifies one directory entry. `kind` is `Err` with a description if the
/// entry could not be inspected. Returns the member name if accepted.
pub fn classify(file_name: &[u8], kind: Result<EntryKind, String>) -> Result<Name, Rejection> {
    let reject = |reason| Rejection {
        display_name: display_bytes(file_name),
        reason,
    };
    // Validate the name first: it is the primary allowlist and must hold even
    // for entries we cannot inspect.
    let name = Name::from_bytes(file_name).map_err(|e| reject(e.into()))?;
    match kind.map_err(|e| reject(RejectReason::Inaccessible(e)))? {
        EntryKind::Regular { size: 0 } => Ok(name),
        EntryKind::Regular { size } => Err(reject(RejectReason::NotEmpty(size))),
        other => Err(reject(RejectReason::NotRegularFile(other))),
    }
}

/// The result of scanning a membership directory.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Membership {
    /// Valid members.
    pub members: BTreeSet<Name>,
    /// Rejected entries, in the order encountered.
    pub rejected: Vec<Rejection>,
}

impl Membership {
    /// Builds membership from `(file_name, kind)` entries. The `.` and `..`
    /// directory entries are skipped silently.
    pub fn from_entries<'a, I>(entries: I) -> Self
    where
        I: IntoIterator<Item = (&'a [u8], Result<EntryKind, String>)>,
    {
        let mut membership = Self::default();
        for (file_name, kind) in entries {
            if file_name == b"." || file_name == b".." {
                continue;
            }
            match classify(file_name, kind) {
                Ok(name) => {
                    membership.members.insert(name);
                }
                Err(rejection) => membership.rejected.push(rejection),
            }
        }
        membership
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_empty_regular_file() {
        assert_eq!(
            classify(b"libcurl", Ok(EntryKind::Regular { size: 0 })).unwrap(),
            Name::new("libcurl").unwrap()
        );
    }

    #[test]
    fn rejects_non_empty_file() {
        let r = classify(b"libcurl", Ok(EntryKind::Regular { size: 3 })).unwrap_err();
        assert_eq!(r.reason, RejectReason::NotEmpty(3));
        assert_eq!(r.reason.to_string(), "file is not empty (3 bytes)");
    }

    #[test]
    fn rejects_other_types() {
        for kind in [
            EntryKind::Directory,
            EntryKind::Symlink,
            EntryKind::Fifo,
            EntryKind::Socket,
            EntryKind::CharDevice,
            EntryKind::BlockDevice,
            EntryKind::Unknown,
        ] {
            let r = classify(b"x", Ok(kind)).unwrap_err();
            assert_eq!(r.reason, RejectReason::NotRegularFile(kind));
        }
        let r = classify(b"x", Ok(EntryKind::Symlink)).unwrap_err();
        assert_eq!(r.reason.to_string(), "not a regular file (symbolic link)");
    }

    #[test]
    fn name_checked_before_type() {
        let r = classify(b".hidden", Ok(EntryKind::Regular { size: 0 })).unwrap_err();
        assert_eq!(r.reason, RejectReason::InvalidName(NameError::LeadingDot));
        assert_eq!(r.display_name, ".hidden");

        let r = classify(b"bad\nname", Err("gone".into())).unwrap_err();
        assert!(matches!(r.reason, RejectReason::InvalidName(_)));
        assert_eq!(r.display_name, "bad\\nname");
    }

    #[test]
    fn inaccessible_entry() {
        let r = classify(b"x", Err("permission denied".into())).unwrap_err();
        assert_eq!(
            r.reason,
            RejectReason::Inaccessible("permission denied".into())
        );
    }

    #[test]
    fn membership_from_entries() {
        let entries: Vec<(&[u8], Result<EntryKind, String>)> = vec![
            (b".", Ok(EntryKind::Directory)),
            (b"..", Ok(EntryKind::Directory)),
            (b"b", Ok(EntryKind::Regular { size: 0 })),
            (b"a", Ok(EntryKind::Regular { size: 0 })),
            (b"link", Ok(EntryKind::Symlink)),
            (b".swp", Ok(EntryKind::Regular { size: 0 })),
            (b"full", Ok(EntryKind::Regular { size: 1 })),
        ];
        let m = Membership::from_entries(entries);
        let names: Vec<_> = m.members.iter().map(Name::as_str).collect();
        assert_eq!(names, ["a", "b"]);
        let rejected: Vec<_> = m.rejected.iter().map(|r| r.display_name.as_str()).collect();
        assert_eq!(rejected, ["link", ".swp", "full"]);
    }
}
