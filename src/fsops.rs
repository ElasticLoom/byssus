//! Descriptor-based filesystem operations confined beneath trusted roots.
//!
//! Every lookup of a path derived from a member name goes through
//! [`resolve_dir`], which uses `openat2` with `RESOLVE_BENEATH |
//! RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS`. See `docs/DESIGN.md`,
//! "Mount creation".

use std::ffi::CStr;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::Path;

use rustix::fs::{AtFlags, FileType, Mode, OFlags, ResolveFlags, StatxFlags};

use crate::identity::DevIno;
use crate::membership::{EntryKind, Membership};

/// Resolution flags for every confined lookup.
pub const CONFINED: ResolveFlags = ResolveFlags::BENEATH
    .union(ResolveFlags::NO_SYMLINKS)
    .union(ResolveFlags::NO_MAGICLINKS);

/// `openat2` may fail with `EAGAIN` when a concurrent rename races with a
/// `RESOLVE_BENEATH` lookup; retry a bounded number of times.
const EAGAIN_RETRIES: usize = 16;

/// Opens a trusted root directory from configuration as an `O_PATH`
/// descriptor. Symlinks in the configured path are followed: configuration is
/// root-owned and trusted.
pub fn open_root(path: &Path) -> io::Result<OwnedFd> {
    Ok(rustix::fs::open(
        path,
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?)
}

/// Opens a directory for reading its entries (not `O_PATH`).
pub fn open_dir_for_reading(path: &Path) -> io::Result<OwnedFd> {
    Ok(rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?)
}

/// Resolves a relative directory path beneath `root` as an `O_PATH`
/// descriptor, refusing symlinks, magic links and escapes.
pub fn resolve_dir(root: BorrowedFd<'_>, relative: &str) -> io::Result<OwnedFd> {
    let mut attempts = 0;
    loop {
        match rustix::fs::openat2(
            root,
            relative,
            OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
            CONFINED,
        ) {
            Err(rustix::io::Errno::AGAIN) if attempts < EAGAIN_RETRIES => attempts += 1,
            other => return Ok(other?),
        }
    }
}

/// Creates (if needed) and opens each component beneath `root`, returning an
/// `O_PATH` descriptor for the final directory. Existing components must be
/// real directories; a symlink anywhere fails with `ELOOP`.
pub fn ensure_dirs_beneath(root: BorrowedFd<'_>, components: &[String]) -> io::Result<OwnedFd> {
    let Some((first, rest)) = components.split_first() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "no path components",
        ));
    };
    let mut current = ensure_one(root, first)?;
    for component in rest {
        current = ensure_one(current.as_fd(), component)?;
    }
    Ok(current)
}

fn ensure_one(parent: BorrowedFd<'_>, component: &str) -> io::Result<OwnedFd> {
    debug_assert!(!component.contains('/') && component != "." && component != "..");
    match rustix::fs::mkdirat(parent, component, Mode::from_raw_mode(0o755)) {
        Ok(()) | Err(rustix::io::Errno::EXIST) => {}
        Err(e) => return Err(e.into()),
    }
    resolve_dir(parent, component)
}

/// Removes the empty directory `leaf` in the directory `relative_parent`
/// beneath `root`. Returns `Ok(false)` if it was not removed because it is
/// not empty, is busy (a mount point) or no longer exists.
pub fn remove_empty_dir(
    root: BorrowedFd<'_>,
    relative_parent: Option<&str>,
    leaf: &str,
) -> io::Result<bool> {
    let parent_fd;
    let parent = match relative_parent {
        Some(rel) => {
            parent_fd = resolve_dir(root, rel)?;
            parent_fd.as_fd()
        }
        None => root,
    };
    match rustix::fs::unlinkat(parent, leaf, AtFlags::REMOVEDIR) {
        Ok(()) => Ok(true),
        Err(rustix::io::Errno::NOTEMPTY | rustix::io::Errno::BUSY | rustix::io::Errno::NOENT) => {
            Ok(false)
        }
        Err(e) => Err(e.into()),
    }
}

/// Device and inode of the file a descriptor refers to.
pub fn dev_ino(fd: BorrowedFd<'_>) -> io::Result<DevIno> {
    let st = rustix::fs::statx(fd, "", AtFlags::EMPTY_PATH, StatxFlags::INO)?;
    Ok(DevIno {
        dev_major: st.stx_dev_major,
        dev_minor: st.stx_dev_minor,
        ino: st.stx_ino,
    })
}

/// Lists a membership directory through `dir` (which must be opened for
/// reading) and classifies each entry without following symlinks.
pub fn scan_membership(dir: BorrowedFd<'_>) -> io::Result<Membership> {
    let mut reader = rustix::fs::Dir::read_from(dir)?;
    let mut entries: Vec<(Vec<u8>, Result<EntryKind, String>)> = Vec::new();
    while let Some(entry) = reader.read() {
        let entry = entry?;
        let name: &CStr = entry.file_name();
        let bytes = name.to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        let kind = rustix::fs::statx(
            dir,
            name,
            AtFlags::SYMLINK_NOFOLLOW,
            StatxFlags::TYPE | StatxFlags::SIZE,
        )
        .map(|st| entry_kind(FileType::from_raw_mode(u32::from(st.stx_mode)), st.stx_size))
        .map_err(|e| e.to_string());
        entries.push((bytes.to_vec(), kind));
    }
    Ok(Membership::from_entries(
        entries
            .iter()
            .map(|(name, kind)| (name.as_slice(), kind.clone())),
    ))
}

fn entry_kind(file_type: FileType, size: u64) -> EntryKind {
    match file_type {
        FileType::RegularFile => EntryKind::Regular { size },
        FileType::Directory => EntryKind::Directory,
        FileType::Symlink => EntryKind::Symlink,
        FileType::Fifo => EntryKind::Fifo,
        FileType::Socket => EntryKind::Socket,
        FileType::CharacterDevice => EntryKind::CharDevice,
        FileType::BlockDevice => EntryKind::BlockDevice,
        FileType::Unknown => EntryKind::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;

    use super::*;

    fn root(dir: &tempfile::TempDir) -> OwnedFd {
        open_root(dir.path()).unwrap()
    }

    #[test]
    fn resolve_dir_confined() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("a/b")).unwrap();
        fs::write(dir.path().join("file"), "").unwrap();
        symlink("a", dir.path().join("link")).unwrap();
        symlink("/", dir.path().join("abs")).unwrap();
        let r = root(&dir);

        assert!(resolve_dir(r.as_fd(), "a/b").is_ok());
        let errno = |rel: &str| {
            resolve_dir(r.as_fd(), rel)
                .unwrap_err()
                .raw_os_error()
                .unwrap()
        };
        assert_eq!(errno("missing"), libc::ENOENT);
        assert_eq!(errno("file"), libc::ENOTDIR);
        assert_eq!(errno("link"), libc::ELOOP);
        assert_eq!(errno("link/b"), libc::ELOOP);
        assert_eq!(errno("abs"), libc::ELOOP);
        assert_eq!(errno(".."), libc::EXDEV);
        assert_eq!(errno("a/../.."), libc::EXDEV);
        assert_eq!(errno("/etc"), libc::EXDEV);
    }

    #[test]
    fn dev_ino_matches_std_metadata() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("x")).unwrap();
        let r = root(&dir);
        let fd = resolve_dir(r.as_fd(), "x").unwrap();
        let id = dev_ino(fd.as_fd()).unwrap();
        let meta = fs::metadata(dir.path().join("x")).unwrap();
        assert_eq!(id.ino, meta.ino());
        assert_eq!(libc::makedev(id.dev_major, id.dev_minor), meta.dev());
    }

    #[test]
    fn ensure_dirs_creates_and_reuses() {
        let dir = tempfile::tempdir().unwrap();
        let r = root(&dir);
        let comps = vec!["one".to_owned(), "two".to_owned()];
        let a = ensure_dirs_beneath(r.as_fd(), &comps).unwrap();
        assert!(dir.path().join("one/two").is_dir());
        let b = ensure_dirs_beneath(r.as_fd(), &comps).unwrap();
        assert_eq!(dev_ino(a.as_fd()).unwrap(), dev_ino(b.as_fd()).unwrap());
        assert!(ensure_dirs_beneath(r.as_fd(), &[]).is_err());
    }

    #[test]
    fn ensure_dirs_refuses_symlinks_and_files() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), dir.path().join("evil")).unwrap();
        fs::write(dir.path().join("file"), "").unwrap();
        let r = root(&dir);

        let err = ensure_dirs_beneath(r.as_fd(), &["evil".into(), "x".into()]).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::ELOOP));
        assert!(!outside.path().join("x").exists());

        let err = ensure_dirs_beneath(r.as_fd(), &["file".into()]).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::ENOTDIR));
    }

    #[test]
    fn remove_empty_dir_behavior() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("p/empty")).unwrap();
        fs::create_dir_all(dir.path().join("p/full/x")).unwrap();
        let r = root(&dir);
        assert!(remove_empty_dir(r.as_fd(), Some("p"), "empty").unwrap());
        assert!(!dir.path().join("p/empty").exists());
        assert!(!remove_empty_dir(r.as_fd(), Some("p"), "full").unwrap());
        assert!(!remove_empty_dir(r.as_fd(), Some("p"), "gone").unwrap());
        fs::create_dir(dir.path().join("top")).unwrap();
        assert!(remove_empty_dir(r.as_fd(), None, "top").unwrap());
    }

    #[test]
    fn scan_membership_classifies_entries() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        fs::write(p.join("libcurl"), "").unwrap();
        fs::write(p.join("openssl"), "").unwrap();
        fs::write(p.join("nonempty"), "x").unwrap();
        fs::write(p.join(".hidden"), "").unwrap();
        fs::create_dir(p.join("subdir")).unwrap();
        symlink("libcurl", p.join("link")).unwrap();
        rustix::fs::mknodat(
            rustix::fs::CWD,
            p.join("fifo").as_path(),
            FileType::Fifo,
            Mode::from_raw_mode(0o644),
            0,
        )
        .unwrap();

        let fd = open_dir_for_reading(p).unwrap();
        let m = scan_membership(fd.as_fd()).unwrap();
        let members: Vec<_> = m.members.iter().map(crate::name::Name::as_str).collect();
        assert_eq!(members, ["libcurl", "openssl"]);
        let mut rejected: Vec<_> = m.rejected.iter().map(|r| r.display_name.as_str()).collect();
        rejected.sort_unstable();
        assert_eq!(rejected, [".hidden", "fifo", "link", "nonempty", "subdir"]);

        // Rescanning through the same descriptor sees changes.
        fs::remove_file(p.join("openssl")).unwrap();
        let m = scan_membership(fd.as_fd()).unwrap();
        assert_eq!(m.members.len(), 1);
    }
}
