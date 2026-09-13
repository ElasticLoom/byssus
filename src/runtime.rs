//! Opened runtime resources: root and membership directory descriptors.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CStr;
use std::fmt;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

use rustix::fs::{AtFlags, FileType, Mode, OFlags, StatxFlags};

use crate::config::{AbsPath, Config, GroupConfig, GroupSetConfig};
use crate::fsops;
use crate::membership::{EntryKind, RejectReason, Rejection};
use crate::name::{GroupId, Name, display_bytes};
use crate::reconcile::observe::Note;

/// Descriptors for one configured group.
#[derive(Debug)]
pub struct GroupRuntime {
    /// The group's configuration.
    pub config: GroupConfig,
    /// Membership directory, opened for reading.
    pub membership_dir: OwnedFd,
}

/// What an [`OpenGroupError`] concerns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Subject {
    /// A statically configured group.
    Group(GroupId),
    /// A group set.
    Set(Name),
}

impl fmt::Display for Subject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Group(group) => write!(f, "group '{group}'"),
            Self::Set(set) => write!(f, "group set '{set}'"),
        }
    }
}

/// Why a group's or group set's directories could not be opened.
#[derive(Debug, thiserror::Error)]
#[error("{subject}: cannot open {field} {path}: {source}")]
pub struct OpenGroupError {
    /// The group or set.
    pub subject: Subject,
    /// Which configured directory.
    pub field: &'static str,
    /// Its path.
    pub path: AbsPath,
    /// Underlying error.
    pub source: io::Error,
}

/// Opens trusted roots by path, caching descriptors.
///
/// Configured roots are opened up front; roots recorded in state for groups
/// that have since been removed or relocated are opened on demand. Failed
/// opens are not cached, so a root that appears later is picked up.
#[derive(Debug, Default)]
pub struct Roots {
    fds: BTreeMap<AbsPath, OwnedFd>,
}

impl Roots {
    /// Returns a descriptor for `root`, opening it if necessary.
    pub fn get(&mut self, root: &AbsPath) -> Result<BorrowedFd<'_>, String> {
        if !self.fds.contains_key(root) {
            let fd = fsops::open_root(root.as_path()).map_err(|e| e.to_string())?;
            self.fds.insert(root.clone(), fd);
        }
        self.fds
            .get(root)
            .map(AsFd::as_fd)
            .ok_or_else(|| "root descriptor missing".to_owned())
    }

    /// Inserts an already-opened root.
    pub fn insert(&mut self, root: AbsPath, fd: OwnedFd) {
        self.fds.insert(root, fd);
    }
}

/// A group set: its configuration and its membership root, opened for
/// reading.
#[derive(Debug)]
pub struct SetRuntime {
    /// The set's configuration.
    pub config: GroupSetConfig,
    /// The membership root, whose subdirectories are the set's groups.
    pub root_dir: OwnedFd,
}

/// The outcome of discovering group sets' groups.
#[derive(Debug, Default)]
pub struct Discovery {
    /// Problems to report.
    pub notes: Vec<Note>,
    /// Groups that exist but could not be opened this pass; their records
    /// must be left untouched.
    pub unavailable: BTreeSet<GroupId>,
    /// Sets whose membership root could not be read this pass; all their
    /// groups' records must be left untouched.
    pub unavailable_sets: BTreeSet<Name>,
    /// Sets whose membership root was scanned successfully.
    pub scanned_sets: BTreeSet<Name>,
}

/// All opened groups plus the root cache.
#[derive(Debug, Default)]
pub struct Runtime {
    /// Statically configured groups and the groups discovered in sets by the
    /// most recent [`Runtime::discover`].
    pub groups: BTreeMap<GroupId, GroupRuntime>,
    /// Group sets by name.
    pub sets: BTreeMap<Name, SetRuntime>,
    /// Root descriptors.
    pub roots: Roots,
}

impl Runtime {
    /// Opens every configured group's source root, target root and membership
    /// directory. Fails on the first error so a configuration is never
    /// partially applied.
    pub fn open(config: &Config) -> Result<Self, OpenGroupError> {
        let mut runtime = Self::default();
        for group in config.groups.values() {
            runtime.open_group(group)?;
        }
        for set in config.group_sets.values() {
            runtime.open_set(set)?;
        }
        Ok(runtime)
    }

    /// Opens every group it can, returning the errors for those it cannot.
    /// Used by read-only commands, which report unavailable groups instead of
    /// failing.
    #[must_use]
    pub fn open_lenient(config: &Config) -> (Self, Vec<OpenGroupError>) {
        let mut runtime = Self::default();
        let mut errors: Vec<OpenGroupError> = config
            .groups
            .values()
            .filter_map(|group| runtime.open_group(group).err())
            .collect();
        errors.extend(
            config
                .group_sets
                .values()
                .filter_map(|set| runtime.open_set(set).err()),
        );
        (runtime, errors)
    }

    fn open_set(&mut self, set: &GroupSetConfig) -> Result<(), OpenGroupError> {
        let err = |field, path: &AbsPath, source| OpenGroupError {
            subject: Subject::Set(set.name.clone()),
            field,
            path: path.clone(),
            source,
        };
        let source_root = fsops::open_root(set.source_root.as_path())
            .map_err(|e| err("source_root", &set.source_root, e))?;
        let target_root = fsops::open_root(set.target_root.as_path())
            .map_err(|e| err("target_root", &set.target_root, e))?;
        let root_dir = fsops::open_dir_for_reading(set.membership_root.as_path())
            .map_err(|e| err("membership_root", &set.membership_root, e))?;
        self.roots.insert(set.source_root.clone(), source_root);
        self.roots.insert(set.target_root.clone(), target_root);
        self.sets.insert(
            set.name.clone(),
            SetRuntime {
                config: set.clone(),
                root_dir,
            },
        );
        Ok(())
    }

    /// Replaces the set groups in [`Runtime::groups`] with the groups found
    /// in each set's membership root now. Sets in `skip` (degraded) are not
    /// scanned. Each group's membership directory is opened afresh, beneath
    /// the membership root, without following symlinks.
    pub fn discover(&mut self, skip: &BTreeSet<Name>) -> Discovery {
        self.groups.retain(|id, _| id.set().is_none());
        let mut discovery = Discovery::default();
        for (set_name, set) in &self.sets {
            if skip.contains(set_name) {
                continue;
            }
            match discover_set(set) {
                Ok((groups, notes, unavailable)) => {
                    discovery.scanned_sets.insert(set_name.clone());
                    for group in groups {
                        self.groups.insert(group.config.name.clone(), group);
                    }
                    discovery.notes.extend(notes);
                    discovery.unavailable.extend(unavailable);
                }
                Err(note) => {
                    discovery.notes.push(note);
                    discovery.unavailable_sets.insert(set_name.clone());
                }
            }
        }
        discovery
    }

    fn open_group(&mut self, group: &GroupConfig) -> Result<(), OpenGroupError> {
        let err = |field, path: &AbsPath, source| OpenGroupError {
            subject: Subject::Group(group.name.clone()),
            field,
            path: path.clone(),
            source,
        };
        let source_root = fsops::open_root(group.source_root.as_path())
            .map_err(|e| err("source_root", &group.source_root, e))?;
        let target_root = fsops::open_root(group.target_root.as_path())
            .map_err(|e| err("target_root", &group.target_root, e))?;
        let membership_dir = fsops::open_dir_for_reading(group.membership.as_path())
            .map_err(|e| err("membership", &group.membership, e))?;
        self.roots.insert(group.source_root.clone(), source_root);
        self.roots.insert(group.target_root.clone(), target_root);
        self.groups.insert(
            group.name.clone(),
            GroupRuntime {
                config: group.clone(),
                membership_dir,
            },
        );
        Ok(())
    }
}

type SetScan = (Vec<GroupRuntime>, Vec<Note>, Vec<GroupId>);

fn discover_set(set: &SetRuntime) -> Result<SetScan, Note> {
    let set_name = &set.config.name;
    if fsops::is_deleted(set.root_dir.as_fd()).unwrap_or(false) {
        return Err(Note::SetRootDeleted {
            set: set_name.clone(),
        });
    }
    let unreadable = |e: io::Error| Note::SetRootUnreadable {
        set: set_name.clone(),
        error: e.to_string(),
    };
    let mut reader =
        rustix::fs::Dir::read_from(set.root_dir.as_fd()).map_err(|e| unreadable(e.into()))?;
    let mut groups = Vec::new();
    let mut notes = Vec::new();
    let mut unavailable = Vec::new();
    while let Some(entry) = reader.read() {
        let entry = entry.map_err(|e| unreadable(e.into()))?;
        let file_name: &CStr = entry.file_name();
        let bytes = file_name.to_bytes();
        if bytes.starts_with(b".") {
            continue;
        }
        let reject = |reason| Note::SetEntryRejected {
            set: set_name.clone(),
            rejection: Rejection {
                display_name: display_bytes(bytes),
                reason,
            },
        };
        let name = match Name::from_bytes(bytes) {
            Ok(name) => name,
            Err(e) => {
                notes.push(reject(RejectReason::InvalidName(e)));
                continue;
            }
        };
        let kind = rustix::fs::statx(
            set.root_dir.as_fd(),
            file_name,
            AtFlags::SYMLINK_NOFOLLOW,
            StatxFlags::TYPE,
        )
        .map(|st| FileType::from_raw_mode(u32::from(st.stx_mode)));
        match kind {
            Ok(FileType::Directory) => {}
            // Vanished since listing.
            Err(rustix::io::Errno::NOENT) => continue,
            Ok(other) => {
                let kind = match other {
                    FileType::RegularFile => EntryKind::Regular { size: 0 },
                    FileType::Symlink => EntryKind::Symlink,
                    FileType::Fifo => EntryKind::Fifo,
                    FileType::Socket => EntryKind::Socket,
                    FileType::CharacterDevice => EntryKind::CharDevice,
                    FileType::BlockDevice => EntryKind::BlockDevice,
                    FileType::Directory | FileType::Unknown => EntryKind::Unknown,
                };
                notes.push(reject(RejectReason::NotDirectory(kind)));
                continue;
            }
            Err(e) => {
                notes.push(reject(RejectReason::Inaccessible(e.to_string())));
                continue;
            }
        }
        let config = match set.config.group(&name) {
            Ok(config) => config,
            Err(e) => {
                notes.push(reject(RejectReason::Inaccessible(format!("template: {e}"))));
                continue;
            }
        };
        match rustix::fs::openat2(
            set.root_dir.as_fd(),
            file_name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
            fsops::CONFINED,
        ) {
            Ok(membership_dir) => groups.push(GroupRuntime {
                config,
                membership_dir,
            }),
            Err(rustix::io::Errno::NOENT) => {}
            Err(e) => {
                notes.push(Note::MembershipUnreadable {
                    group: config.name.clone(),
                    error: e.to_string(),
                });
                unavailable.push(config.name);
            }
        }
    }
    Ok((groups, notes, unavailable))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::config::load_from_strs;

    fn config_for(dir: &Path, membership: &str) -> Config {
        let p = |s: &str| dir.join(s).display().to_string();
        let text = format!(
            "[groups.g]\nsource_root = \"{}\"\nsource = \"{{name}}\"\ntarget_root = \"{}\"\ntarget = \"{{name}}\"\nmembership = \"{}\"\n",
            p("src"),
            p("view"),
            p(membership)
        );
        load_from_strs(&[(Path::new("/x.toml"), &text, true)])
            .unwrap()
            .config
    }

    #[test]
    fn opens_groups_and_caches_roots() {
        let dir = tempfile::tempdir().unwrap();
        for d in ["src", "view", "members"] {
            std::fs::create_dir(dir.path().join(d)).unwrap();
        }
        let config = config_for(dir.path(), "members");
        let mut rt = Runtime::open(&config).unwrap();
        assert_eq!(rt.groups.len(), 1);
        let src = AbsPath::new(&dir.path().join("src").display().to_string()).unwrap();
        assert!(rt.roots.get(&src).is_ok());

        let later = AbsPath::new(&dir.path().join("later").display().to_string()).unwrap();
        assert!(rt.roots.get(&later).is_err());
        // Failures are not cached: once the directory exists it opens.
        std::fs::create_dir(dir.path().join("later")).unwrap();
        assert!(rt.roots.get(&later).is_ok());
    }

    #[test]
    fn open_fails_on_missing_membership_dir() {
        let dir = tempfile::tempdir().unwrap();
        for d in ["src", "view"] {
            std::fs::create_dir(dir.path().join(d)).unwrap();
        }
        let config = config_for(dir.path(), "absent");
        let err = Runtime::open(&config).unwrap_err();
        assert_eq!(err.field, "membership");
        let (rt, errors) = Runtime::open_lenient(&config);
        assert!(rt.groups.is_empty());
        assert_eq!(errors.len(), 1);
    }

    fn set_config(dir: &Path) -> Config {
        for d in ["orgs", "membership/research"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
        }
        let p = |s: &str| dir.join(s).display().to_string();
        let text = format!(
            "[group_sets.research]\nmembership_root = \"{}\"\nsource_root = \"{}\"\nsource = \"{{group}}/projects/{{name}}\"\ntarget_root = \"{}\"\ntarget = \"{{group}}/view/{{name}}\"\n",
            p("membership/research"),
            p("orgs"),
            p("orgs")
        );
        load_from_strs(&[(Path::new("/x.toml"), &text, true)])
            .unwrap()
            .config
    }

    fn ids(rt: &Runtime) -> Vec<String> {
        rt.groups.keys().map(ToString::to_string).collect()
    }

    #[test]
    fn discovers_set_groups_and_rejects_other_entries() {
        let dir = tempfile::tempdir().unwrap();
        let config = set_config(dir.path());
        let root = dir.path().join("membership/research");
        for g in ["acme", "beta", "bad:name"] {
            std::fs::create_dir(root.join(g)).unwrap();
        }
        std::fs::create_dir(root.join(".hidden")).unwrap();
        std::fs::write(root.join("file"), "").unwrap();
        std::os::unix::fs::symlink(root.join("acme"), root.join("link")).unwrap();

        let mut rt = Runtime::open(&config).unwrap();
        assert!(rt.groups.is_empty(), "no groups before discovery");
        let discovery = rt.discover(&BTreeSet::new());
        assert_eq!(ids(&rt), ["research/acme", "research/beta"]);
        assert!(discovery.unavailable.is_empty() && discovery.unavailable_sets.is_empty());
        assert_eq!(discovery.scanned_sets.len(), 1);
        let rejected: BTreeSet<String> = discovery
            .notes
            .iter()
            .filter_map(|n| match n {
                Note::SetEntryRejected { rejection, .. } => Some(rejection.display_name.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            rejected,
            ["bad:name", "file", "link"]
                .iter()
                .map(|s| (*s).to_owned())
                .collect()
        );

        let acme = &rt.groups[&GroupId::parse("research/acme").unwrap()];
        assert_eq!(acme.config.source.as_str(), "acme/projects/{name}");
        assert_eq!(acme.config.membership.as_path(), root.join("acme"));

        // Groups follow the directory: removal and addition.
        std::fs::remove_dir(root.join("beta")).unwrap();
        std::fs::create_dir(root.join("gamma")).unwrap();
        rt.discover(&BTreeSet::new());
        assert_eq!(ids(&rt), ["research/acme", "research/gamma"]);

        // Skipped (degraded) sets contribute no groups.
        let mut skip = BTreeSet::new();
        skip.insert(Name::new("research").unwrap());
        let discovery = rt.discover(&skip);
        assert!(rt.groups.is_empty());
        assert!(discovery.scanned_sets.is_empty());
    }

    #[test]
    fn static_groups_survive_discovery() {
        let dir = tempfile::tempdir().unwrap();
        for d in ["src", "view", "members"] {
            std::fs::create_dir(dir.path().join(d)).unwrap();
        }
        let mut config = config_for(dir.path(), "members");
        config.group_sets = set_config(dir.path()).group_sets;
        std::fs::create_dir(dir.path().join("membership/research/acme")).unwrap();
        let mut rt = Runtime::open(&config).unwrap();
        rt.discover(&BTreeSet::new());
        assert_eq!(ids(&rt), ["g", "research/acme"]);
        rt.discover(&BTreeSet::new());
        assert_eq!(ids(&rt), ["g", "research/acme"]);
    }

    #[test]
    fn deleted_set_root_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let config = set_config(dir.path());
        let mut rt = Runtime::open(&config).unwrap();
        std::fs::remove_dir(dir.path().join("membership/research")).unwrap();
        let discovery = rt.discover(&BTreeSet::new());
        assert!(matches!(discovery.notes[..], [Note::SetRootDeleted { .. }]));
        assert_eq!(discovery.unavailable_sets.len(), 1);
    }

    #[test]
    fn open_lenient_reports_set_subject() {
        let dir = tempfile::tempdir().unwrap();
        let config = set_config(dir.path());
        std::fs::remove_dir(dir.path().join("membership/research")).unwrap();
        let (rt, errors) = Runtime::open_lenient(&config);
        assert!(rt.sets.is_empty());
        assert_eq!(
            errors[0].subject,
            Subject::Set(Name::new("research").unwrap())
        );
        assert!(errors[0].to_string().starts_with("group set 'research'"));
    }
}
