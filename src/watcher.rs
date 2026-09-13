//! inotify watches on membership directories.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

use rustix::fs::inotify::{self, CreateFlags, ReadFlags, WatchFlags};

use crate::name::Name;
use crate::runtime::Runtime;

/// Events that change membership.
const MEMBERSHIP_EVENTS: WatchFlags = WatchFlags::CREATE
    .union(WatchFlags::DELETE)
    .union(WatchFlags::MOVED_FROM)
    .union(WatchFlags::MOVED_TO)
    .union(WatchFlags::CLOSE_WRITE)
    .union(WatchFlags::ATTRIB)
    .union(WatchFlags::MODIFY)
    .union(WatchFlags::DELETE_SELF)
    .union(WatchFlags::MOVE_SELF)
    .union(WatchFlags::ONLYDIR);

/// What a batch of inotify events means for reconciliation.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Changes {
    /// Some membership directory changed.
    pub membership_changed: bool,
    /// The kernel event queue overflowed; changes may have been lost.
    pub overflow: bool,
    /// Groups whose membership directory was deleted, moved or unmounted.
    pub lost: BTreeSet<Name>,
}

/// An inotify instance watching every configured membership directory.
#[derive(Debug)]
pub struct Watcher {
    fd: OwnedFd,
    groups: BTreeMap<i32, Name>,
}

impl Watcher {
    /// Creates a non-blocking inotify instance with a watch on each group's
    /// membership directory.
    pub fn new(runtime: &Runtime) -> io::Result<Self> {
        let fd = inotify::init(CreateFlags::CLOEXEC | CreateFlags::NONBLOCK)?;
        let mut groups = BTreeMap::new();
        for (name, group) in &runtime.groups {
            let wd = inotify::add_watch(&fd, group.config.membership.as_path(), MEMBERSHIP_EVENTS)
                .map_err(|e| {
                    io::Error::new(
                        io::Error::from(e).kind(),
                        format!(
                            "cannot watch membership directory {} of group '{name}': {e}",
                            group.config.membership
                        ),
                    )
                })?;
            groups.insert(wd, name.clone());
        }
        Ok(Self { fd, groups })
    }

    /// Reads and classifies all pending events without blocking.
    pub fn drain(&mut self) -> io::Result<Changes> {
        let mut changes = Changes::default();
        let mut buf = [MaybeUninit::<u8>::uninit(); 8192];
        let mut reader = inotify::Reader::new(&self.fd, &mut buf);
        loop {
            let event = match reader.next() {
                Ok(event) => event,
                Err(rustix::io::Errno::AGAIN) => break,
                Err(rustix::io::Errno::INTR) => continue,
                Err(e) => return Err(e.into()),
            };
            let flags = event.events();
            if flags.contains(ReadFlags::QUEUE_OVERFLOW) {
                changes.overflow = true;
                continue;
            }
            let group = self.groups.get(&event.wd()).cloned();
            if flags.intersects(ReadFlags::DELETE_SELF | ReadFlags::MOVE_SELF | ReadFlags::IGNORED)
                || flags.bits() & libc::IN_UNMOUNT != 0
            {
                if let Some(group) = group {
                    changes.lost.insert(group);
                }
                continue;
            }
            if group.is_some() {
                changes.membership_changed = true;
            }
        }
        for lost in &changes.lost {
            self.groups.retain(|_, g| g != lost);
        }
        Ok(changes)
    }
}

impl AsFd for Watcher {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::*;
    use crate::config::load_from_strs;

    fn runtime_for(dir: &Path) -> Runtime {
        for d in ["src", "view", "members"] {
            fs::create_dir_all(dir.join(d)).unwrap();
        }
        let p = |s: &str| dir.join(s).display().to_string();
        let text = format!(
            "[groups.g]\nsource_root = \"{}\"\nsource = \"{{name}}\"\ntarget_root = \"{}\"\ntarget = \"{{name}}\"\nmembership = \"{}\"\n",
            p("src"),
            p("view"),
            p("members")
        );
        let config = load_from_strs(&[(Path::new("/x.toml"), &text, true)])
            .unwrap()
            .config;
        Runtime::open(&config).unwrap()
    }

    #[test]
    fn reports_membership_changes() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = runtime_for(dir.path());
        let mut watcher = Watcher::new(&runtime).unwrap();
        assert_eq!(watcher.drain().unwrap(), Changes::default());

        fs::write(dir.path().join("members/a"), "").unwrap();
        let changes = watcher.drain().unwrap();
        assert!(changes.membership_changed);
        assert!(!changes.overflow && changes.lost.is_empty());

        fs::remove_file(dir.path().join("members/a")).unwrap();
        assert!(watcher.drain().unwrap().membership_changed);

        // Writing into an existing file is also a change (it becomes invalid).
        fs::write(dir.path().join("members/b"), "").unwrap();
        watcher.drain().unwrap();
        fs::write(dir.path().join("members/b"), "x").unwrap();
        assert!(watcher.drain().unwrap().membership_changed);
    }

    #[test]
    fn reports_lost_directory() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = runtime_for(dir.path());
        let mut watcher = Watcher::new(&runtime).unwrap();
        fs::rename(dir.path().join("members"), dir.path().join("moved")).unwrap();
        let changes = watcher.drain().unwrap();
        assert_eq!(changes.lost.len(), 1);
        assert_eq!(changes.lost.iter().next().unwrap().as_str(), "g");

        // After the watch is gone, further changes are not attributed.
        fs::write(dir.path().join("moved/x"), "").unwrap();
        assert!(!watcher.drain().unwrap().membership_changed);
    }

    #[test]
    fn missing_directory_fails() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = runtime_for(dir.path());
        fs::remove_dir(dir.path().join("members")).unwrap();
        let err = Watcher::new(&runtime).unwrap_err();
        assert!(
            err.to_string()
                .contains("cannot watch membership directory")
        );
    }
}
