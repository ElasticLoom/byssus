//! inotify watches on membership directories.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

use rustix::fs::inotify::{self, CreateFlags, ReadFlags, WatchFlags};

use crate::name::{GroupId, Name};
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
    /// Some membership directory (or group set membership root) changed.
    pub membership_changed: bool,
    /// The kernel event queue overflowed; changes may have been lost.
    pub overflow: bool,
    /// Statically configured groups whose membership directory was deleted,
    /// moved or unmounted.
    pub lost: BTreeSet<GroupId>,
    /// Group sets whose membership root was deleted, moved or unmounted.
    pub lost_sets: BTreeSet<Name>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Watched {
    /// A group's membership directory (static or set group).
    Group(GroupId),
    /// A group set's membership root.
    SetRoot(Name),
}

/// An inotify instance watching every membership directory and group set
/// membership root.
#[derive(Debug)]
pub struct Watcher {
    fd: OwnedFd,
    watches: BTreeMap<i32, Watched>,
    set_groups: BTreeMap<GroupId, i32>,
}

impl Watcher {
    /// Creates a non-blocking inotify instance watching each static group's
    /// membership directory, each group set's membership root, and the set
    /// groups currently in `runtime`.
    pub fn new(runtime: &Runtime) -> io::Result<Self> {
        let fd = inotify::init(CreateFlags::CLOEXEC | CreateFlags::NONBLOCK)?;
        let mut watcher = Self {
            fd,
            watches: BTreeMap::new(),
            set_groups: BTreeMap::new(),
        };
        for (id, group) in runtime.groups.iter().filter(|(id, _)| id.set().is_none()) {
            let wd = watcher
                .add(group.config.membership.as_path(), MEMBERSHIP_EVENTS)
                .map_err(|e| {
                    io::Error::new(
                        e.kind(),
                        format!(
                            "cannot watch membership directory {} of group '{id}': {e}",
                            group.config.membership
                        ),
                    )
                })?;
            watcher.watches.insert(wd, Watched::Group(id.clone()));
        }
        for (name, set) in &runtime.sets {
            let wd = watcher
                .add(set.config.membership_root.as_path(), MEMBERSHIP_EVENTS)
                .map_err(|e| {
                    io::Error::new(
                        e.kind(),
                        format!(
                            "cannot watch membership_root {} of group set '{name}': {e}",
                            set.config.membership_root
                        ),
                    )
                })?;
            watcher.watches.insert(wd, Watched::SetRoot(name.clone()));
        }
        watcher.sync(runtime);
        Ok(watcher)
    }

    fn add(&self, path: &std::path::Path, flags: WatchFlags) -> io::Result<i32> {
        Ok(inotify::add_watch(&self.fd, path, flags)?)
    }

    /// Watches the membership directories of set groups in `runtime` that are
    /// not watched yet, and stops watching set groups that are gone. Call after
    /// each discovery, before membership is read.
    pub fn sync(&mut self, runtime: &Runtime) {
        let current: BTreeSet<&GroupId> = runtime
            .groups
            .keys()
            .filter(|id| id.set().is_some())
            .collect();
        let gone: Vec<GroupId> = self
            .set_groups
            .keys()
            .filter(|id| !current.contains(id))
            .cloned()
            .collect();
        for id in gone {
            if let Some(wd) = self.set_groups.remove(&id) {
                self.watches.remove(&wd);
                // The watch may already be gone (directory deleted).
                let _ = inotify::remove_watch(&self.fd, wd);
            }
        }
        for id in current {
            if self.set_groups.contains_key(id) {
                continue;
            }
            let group = &runtime.groups[id];
            match self.add(
                group.config.membership.as_path(),
                MEMBERSHIP_EVENTS | WatchFlags::DONT_FOLLOW,
            ) {
                Ok(wd) => {
                    self.watches.insert(wd, Watched::Group(id.clone()));
                    self.set_groups.insert(id.clone(), wd);
                }
                // Removed since discovery; the next pass notices.
                Err(e) if e.raw_os_error() == Some(libc::ENOENT) => {}
                Err(e) => tracing::warn!(
                    group = %id,
                    msg = "cannot watch group membership directory; changes are picked up at the next resync",
                    error = %e,
                ),
            }
        }
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
            let self_event = flags
                .intersects(ReadFlags::DELETE_SELF | ReadFlags::MOVE_SELF | ReadFlags::IGNORED)
                || flags.bits() & libc::IN_UNMOUNT != 0;
            match self.watches.get(&event.wd()) {
                None => {}
                Some(Watched::Group(id)) if self_event && id.set().is_none() => {
                    changes.lost.insert(id.clone());
                }
                Some(Watched::SetRoot(set)) if self_event => {
                    changes.lost_sets.insert(set.clone());
                }
                // For a set group, its directory going away is a membership
                // change: the group is removed.
                Some(_) => changes.membership_changed = true,
            }
        }
        for lost in &changes.lost {
            self.watches
                .retain(|_, w| *w != Watched::Group(lost.clone()));
        }
        for lost in &changes.lost_sets {
            self.watches
                .retain(|_, w| *w != Watched::SetRoot(lost.clone()));
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
        assert_eq!(changes.lost.iter().next().unwrap().to_string(), "g");

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

    #[test]
    fn follows_set_groups() {
        let dir = tempfile::tempdir().unwrap();
        for d in ["orgs", "membership/research"] {
            fs::create_dir_all(dir.path().join(d)).unwrap();
        }
        let p = |s: &str| dir.path().join(s).display().to_string();
        let text = format!(
            "[group_sets.research]\nmembership_root = \"{}\"\nsource_root = \"{}\"\nsource = \"{{group}}/{{name}}\"\ntarget_root = \"{}\"\ntarget = \"{{group}}/{{name}}\"\n",
            p("membership/research"),
            p("orgs"),
            p("orgs")
        );
        let config = load_from_strs(&[(Path::new("/x.toml"), &text, true)])
            .unwrap()
            .config;
        let mut runtime = Runtime::open(&config).unwrap();
        let mut watcher = Watcher::new(&runtime).unwrap();
        let root = dir.path().join("membership/research");

        // A new group directory is a change in the set root.
        fs::create_dir(root.join("acme")).unwrap();
        assert!(watcher.drain().unwrap().membership_changed);

        // After discovery and sync, the group's own membership is watched.
        runtime.discover(&BTreeSet::new());
        watcher.sync(&runtime);
        fs::write(root.join("acme/libcurl"), "").unwrap();
        assert!(watcher.drain().unwrap().membership_changed);

        // Deleting a set group is a change, not a loss.
        fs::remove_file(root.join("acme/libcurl")).unwrap();
        fs::remove_dir(root.join("acme")).unwrap();
        let changes = watcher.drain().unwrap();
        assert!(changes.membership_changed);
        assert!(changes.lost.is_empty() && changes.lost_sets.is_empty());
        runtime.discover(&BTreeSet::new());
        watcher.sync(&runtime);
        assert!(watcher.set_groups.is_empty());

        // Moving the set root away loses the set.
        fs::rename(&root, dir.path().join("moved")).unwrap();
        let changes = watcher.drain().unwrap();
        assert_eq!(
            changes
                .lost_sets
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            ["research"]
        );
    }
}
