//! Opened runtime resources: root and membership directory descriptors.

use std::collections::BTreeMap;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

use crate::config::{AbsPath, Config, GroupConfig};
use crate::fsops;
use crate::name::Name;

/// Descriptors for one configured group.
#[derive(Debug)]
pub struct GroupRuntime {
    /// The group's configuration.
    pub config: GroupConfig,
    /// Membership directory, opened for reading.
    pub membership_dir: OwnedFd,
}

/// Why a group's directories could not be opened.
#[derive(Debug, thiserror::Error)]
#[error("group '{group}': cannot open {field} {path}: {source}")]
pub struct OpenGroupError {
    /// Group name.
    pub group: Name,
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

/// All opened groups plus the root cache.
#[derive(Debug, Default)]
pub struct Runtime {
    /// Groups by name.
    pub groups: BTreeMap<Name, GroupRuntime>,
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
        Ok(runtime)
    }

    /// Opens every group it can, returning the errors for those it cannot.
    /// Used by read-only commands, which report unavailable groups instead of
    /// failing.
    #[must_use]
    pub fn open_lenient(config: &Config) -> (Self, Vec<OpenGroupError>) {
        let mut runtime = Self::default();
        let errors = config
            .groups
            .values()
            .filter_map(|group| runtime.open_group(group).err())
            .collect();
        (runtime, errors)
    }

    fn open_group(&mut self, group: &GroupConfig) -> Result<(), OpenGroupError> {
        let err = |field, path: &AbsPath, source| OpenGroupError {
            group: group.name.clone(),
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
}
