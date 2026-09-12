//! Configuration loading and validation.
//!
//! Configuration is TOML: a main file plus drop-in fragments. Loading is
//! all-or-nothing — [`load`] either returns a fully validated [`Config`] or a
//! list of every problem found — so a reload can never apply a partially
//! valid configuration. See `docs/DESIGN.md`, "Configuration".

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::name::Name;
use crate::template::Template;

/// Default main configuration file.
pub const DEFAULT_CONFIG_FILE: &str = "/etc/byssus/byssus.toml";
/// Default drop-in fragment directory.
pub const DEFAULT_CONFIG_DIR: &str = "/etc/byssus/conf.d";
/// Default state directory.
pub const DEFAULT_STATE_DIR: &str = "/var/lib/byssus";
/// Default periodic resync interval in seconds.
pub const DEFAULT_RESYNC_INTERVAL_SECS: u64 = 60;
/// Largest accepted resync interval (one day).
pub const MAX_RESYNC_INTERVAL_SECS: u64 = 86_400;

// ---------------------------------------------------------------------------
// Validated configuration types
// ---------------------------------------------------------------------------

/// A validated absolute path: begins with `/`, contains no empty, `.` or `..`
/// components, no NUL, and has no trailing `/` (except the root itself).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AbsPath(PathBuf);

/// Why a path was rejected by [`AbsPath::new`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AbsPathError {
    /// The path does not begin with `/`.
    #[error("path must be absolute")]
    NotAbsolute,
    /// The path contains `//` (or ends in more than one `/`).
    #[error("path contains an empty component")]
    EmptyComponent,
    /// The path contains a `.` or `..` component.
    #[error("path contains a '{0}' component")]
    DotComponent(String),
    /// The path contains a NUL byte.
    #[error("path contains a NUL byte")]
    Nul,
}

impl AbsPath {
    /// Validates an absolute path. A single trailing `/` is removed.
    pub fn new(raw: &str) -> Result<Self, AbsPathError> {
        if raw.contains('\0') {
            return Err(AbsPathError::Nul);
        }
        let Some(rest) = raw.strip_prefix('/') else {
            return Err(AbsPathError::NotAbsolute);
        };
        if rest.is_empty() {
            return Ok(Self(PathBuf::from("/")));
        }
        let rest = rest.strip_suffix('/').unwrap_or(rest);
        for component in rest.split('/') {
            match component {
                "" => return Err(AbsPathError::EmptyComponent),
                "." | ".." => return Err(AbsPathError::DotComponent(component.to_owned())),
                _ => {}
            }
        }
        Ok(Self(PathBuf::from(format!("/{rest}"))))
    }

    /// Returns the path.
    #[must_use]
    pub fn as_path(&self) -> &Path {
        &self.0
    }

    /// Whether `self` equals `other` or lies beneath it, compared by
    /// components.
    #[must_use]
    pub fn is_at_or_beneath(&self, other: &Self) -> bool {
        self.0.starts_with(&other.0)
    }
}

impl fmt::Display for AbsPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.display().fmt(f)
    }
}

impl serde::Serialize for AbsPath {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // Constructed only from `&str`, so always valid UTF-8.
        serializer.serialize_str(&self.0.to_string_lossy())
    }
}

impl<'de> Deserialize<'de> for AbsPath {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::new(&s).map_err(serde::de::Error::custom)
    }
}

impl AsRef<Path> for AbsPath {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

/// Configurable mount attributes. `nosuid` and `nodev` are always applied and
/// therefore not represented here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[allow(clippy::struct_excessive_bools)]
pub struct MountAttrs {
    /// Apply `MOUNT_ATTR_RDONLY`.
    pub read_only: bool,
    /// Apply `MOUNT_ATTR_NOEXEC`.
    pub noexec: bool,
    /// Apply `MOUNT_ATTR_NOSYMFOLLOW`.
    pub nosymfollow: bool,
}

impl Default for MountAttrs {
    fn default() -> Self {
        Self {
            read_only: true,
            noexec: true,
            nosymfollow: false,
        }
    }
}

/// Settings for the daemon process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonConfig {
    /// Service user to switch to when started as root.
    pub user: Option<String>,
    /// Directory containing the state file and lock.
    pub state_dir: AbsPath,
    /// Periodic full reconcile interval, or `None` if disabled.
    pub resync_interval: Option<Duration>,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            user: None,
            state_dir: AbsPath(PathBuf::from(DEFAULT_STATE_DIR)),
            resync_interval: Some(Duration::from_secs(DEFAULT_RESYNC_INTERVAL_SECS)),
        }
    }
}

/// A validated group definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupConfig {
    /// Group name.
    pub name: Name,
    /// Trusted root beneath which sources are resolved.
    pub source_root: AbsPath,
    /// Source template relative to `source_root`.
    pub source: Template,
    /// Trusted root beneath which views are mounted.
    pub target_root: AbsPath,
    /// Target template relative to `target_root`.
    pub target: Template,
    /// Membership directory.
    pub membership: AbsPath,
    /// Mount attributes.
    pub attrs: MountAttrs,
    /// The file that defined this group.
    pub origin: PathBuf,
}

/// A complete, validated configuration.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Config {
    /// Daemon settings.
    pub daemon: DaemonConfig,
    /// Groups by name.
    pub groups: BTreeMap<Name, GroupConfig>,
    /// Files that were read, in order.
    pub files: Vec<PathBuf>,
}

// ---------------------------------------------------------------------------
// Issues
// ---------------------------------------------------------------------------

/// Severity of a configuration issue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Does not prevent the configuration from being used.
    Warning,
    /// Prevents the configuration from being used.
    Error,
}

/// A single problem found while loading configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    /// How serious the issue is.
    pub severity: Severity,
    /// File the issue relates to, if any.
    pub file: Option<PathBuf>,
    /// Group the issue relates to, if any.
    pub group: Option<String>,
    /// Human-readable description.
    pub message: String,
}

impl fmt::Display for Issue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(file) = &self.file {
            write!(f, "{}: ", file.display())?;
        }
        if let Some(group) = &self.group {
            write!(f, "group '{group}': ")?;
        }
        f.write_str(&self.message)
    }
}

/// Configuration could not be loaded. Contains every issue found, including
/// warnings.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid configuration ({} error(s))", self.errors().count())]
pub struct LoadError {
    /// All issues found.
    pub issues: Vec<Issue>,
}

impl LoadError {
    /// Issues with [`Severity::Error`].
    pub fn errors(&self) -> impl Iterator<Item = &Issue> {
        self.issues.iter().filter(|i| i.severity == Severity::Error)
    }
}

/// A successfully loaded configuration and any warnings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loaded {
    /// The validated configuration.
    pub config: Config,
    /// Non-fatal issues.
    pub warnings: Vec<Issue>,
}

#[derive(Debug, Default)]
struct Issues(Vec<Issue>);

impl Issues {
    fn push(
        &mut self,
        severity: Severity,
        file: Option<&Path>,
        group: Option<&str>,
        message: impl Into<String>,
    ) {
        self.0.push(Issue {
            severity,
            file: file.map(Path::to_path_buf),
            group: group.map(str::to_owned),
            message: message.into(),
        });
    }

    fn error(&mut self, file: Option<&Path>, group: Option<&str>, message: impl Into<String>) {
        self.push(Severity::Error, file, group, message);
    }

    fn warn(&mut self, file: Option<&Path>, group: Option<&str>, message: impl Into<String>) {
        self.push(Severity::Warning, file, group, message);
    }

    fn has_errors(&self) -> bool {
        self.0.iter().any(|i| i.severity == Severity::Error)
    }
}

// ---------------------------------------------------------------------------
// Raw (unvalidated) file format
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    daemon: Option<RawDaemon>,
    #[serde(default)]
    groups: BTreeMap<String, RawGroup>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDaemon {
    user: Option<String>,
    state_dir: Option<String>,
    resync_interval_secs: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGroup {
    source_root: String,
    source: String,
    target_root: String,
    target: String,
    membership: String,
    read_only: Option<bool>,
    noexec: Option<bool>,
    nosymfollow: Option<bool>,
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

/// How ownership and mode violations on configuration files are treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnershipPolicy {
    /// Violations are errors. Used by everything that mounts.
    Enforce,
    /// Violations are warnings. Used by read-only CLI commands.
    Warn,
}

/// Where to load configuration from and how strictly.
#[derive(Debug, Clone)]
pub struct LoadOptions {
    /// Main configuration file.
    pub main_file: PathBuf,
    /// Whether a missing main file is an error (true when given explicitly).
    pub main_file_required: bool,
    /// Drop-in fragment directory.
    pub config_dir: PathBuf,
    /// Whether a missing fragment directory is an error.
    pub config_dir_required: bool,
    /// Treatment of ownership and mode violations.
    pub ownership: OwnershipPolicy,
    /// The UID that must own configuration files and directories (0 in
    /// production; overridable for tests).
    pub trusted_uid: u32,
    /// Whether to check that configured directories exist.
    pub check_paths: bool,
}

impl Default for LoadOptions {
    fn default() -> Self {
        Self {
            main_file: PathBuf::from(DEFAULT_CONFIG_FILE),
            main_file_required: false,
            config_dir: PathBuf::from(DEFAULT_CONFIG_DIR),
            config_dir_required: false,
            ownership: OwnershipPolicy::Enforce,
            trusted_uid: 0,
            check_paths: true,
        }
    }
}

/// Loads and fully validates configuration.
pub fn load(options: &LoadOptions) -> Result<Loaded, LoadError> {
    let mut issues = Issues::default();
    let mut sources: Vec<(PathBuf, String, bool)> = Vec::new();

    match read_file(&options.main_file) {
        Ok(Some(content)) => sources.push((options.main_file.clone(), content, true)),
        Ok(None) if options.main_file_required => {
            issues.error(Some(&options.main_file), None, "file does not exist");
        }
        Ok(None) => {}
        Err(e) => issues.error(Some(&options.main_file), None, format!("cannot read: {e}")),
    }

    match list_fragments(&options.config_dir) {
        Ok(Some(paths)) => {
            for path in paths {
                match read_file(&path) {
                    Ok(Some(content)) => sources.push((path, content, false)),
                    // Vanished between listing and reading.
                    Ok(None) => {}
                    Err(e) => issues.error(Some(&path), None, format!("cannot read: {e}")),
                }
            }
        }
        Ok(None) if options.config_dir_required => {
            issues.error(Some(&options.config_dir), None, "directory does not exist");
        }
        Ok(None) => {}
        Err(e) => issues.error(
            Some(&options.config_dir),
            None,
            format!("cannot list directory: {e}"),
        ),
    }

    for (path, _, _) in &sources {
        check_ownership(path, options, &mut issues);
    }
    if sources.iter().any(|(_, _, main)| !main) {
        check_ownership(&options.config_dir, options, &mut issues);
    }

    let parsed: Vec<(PathBuf, RawFile, bool)> = sources
        .into_iter()
        .filter_map(
            |(path, content, main)| match toml::from_str::<RawFile>(&content) {
                Ok(raw) => Some((path, raw, main)),
                Err(e) => {
                    issues.error(Some(&path), None, format!("invalid TOML: {e}"));
                    None
                }
            },
        )
        .collect();

    if parsed.is_empty() && !issues.has_errors() {
        issues.warn(
            None,
            None,
            "no configuration files found; no groups are defined",
        );
    }

    let config = assemble(parsed, &mut issues);

    if options.check_paths && !issues.has_errors() {
        check_paths_exist(&config, &mut issues);
    }

    if issues.has_errors() {
        Err(LoadError { issues: issues.0 })
    } else {
        Ok(Loaded {
            config,
            warnings: issues.0,
        })
    }
}

/// Parses and validates configuration from in-memory TOML documents, without
/// touching the filesystem. Each entry is `(origin, content, is_main_file)`.
pub fn load_from_strs(documents: &[(&Path, &str, bool)]) -> Result<Loaded, LoadError> {
    let mut issues = Issues::default();
    let parsed = documents
        .iter()
        .filter_map(
            |&(path, content, main)| match toml::from_str::<RawFile>(content) {
                Ok(raw) => Some((path.to_path_buf(), raw, main)),
                Err(e) => {
                    issues.error(Some(path), None, format!("invalid TOML: {e}"));
                    None
                }
            },
        )
        .collect();
    let config = assemble(parsed, &mut issues);
    if issues.has_errors() {
        Err(LoadError { issues: issues.0 })
    } else {
        Ok(Loaded {
            config,
            warnings: issues.0,
        })
    }
}

fn read_file(path: &Path) -> io::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn list_fragments(dir: &Path) -> io::Result<Option<Vec<PathBuf>>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry?;
        let file_name = entry.file_name();
        let bytes = file_name.as_encoded_bytes();
        if bytes.starts_with(b".") || !bytes.ends_with(b".toml") {
            continue;
        }
        paths.push(entry.path());
    }
    // Lexicographic by file name; all paths share the same directory.
    paths.sort();
    Ok(Some(paths))
}

fn check_ownership(path: &Path, options: &LoadOptions, issues: &mut Issues) {
    let severity = match options.ownership {
        OwnershipPolicy::Enforce => Severity::Error,
        OwnershipPolicy::Warn => Severity::Warning,
    };
    // Check the file itself (following symlinks) and the directory that
    // actually contains it, since whoever can write that directory can
    // replace the file.
    let resolved = match std::fs::canonicalize(path) {
        Ok(p) => p,
        Err(e) => {
            issues.push(
                severity,
                Some(path),
                None,
                format!("cannot resolve path: {e}"),
            );
            return;
        }
    };
    let mut to_check = vec![resolved.clone()];
    if let Some(parent) = resolved.parent() {
        to_check.push(parent.to_path_buf());
    }
    for p in to_check {
        match std::fs::metadata(&p) {
            Ok(meta) => {
                if meta.uid() != options.trusted_uid {
                    issues.push(
                        severity,
                        Some(path),
                        None,
                        format!(
                            "{} is owned by uid {}, expected uid {}",
                            p.display(),
                            meta.uid(),
                            options.trusted_uid
                        ),
                    );
                }
                if meta.mode() & 0o022 != 0 {
                    issues.push(
                        severity,
                        Some(path),
                        None,
                        format!(
                            "{} is group- or world-writable (mode {:04o})",
                            p.display(),
                            meta.mode() & 0o7777
                        ),
                    );
                }
            }
            Err(e) => issues.push(
                severity,
                Some(path),
                None,
                format!("cannot stat {}: {e}", p.display()),
            ),
        }
    }
}

fn assemble(files: Vec<(PathBuf, RawFile, bool)>, issues: &mut Issues) -> Config {
    let mut config = Config::default();
    let mut origins: BTreeMap<Name, PathBuf> = BTreeMap::new();

    for (path, raw, is_main) in files {
        config.files.push(path.clone());

        if let Some(daemon) = raw.daemon {
            if is_main {
                config.daemon = validate_daemon(daemon, &path, issues);
            } else {
                issues.error(
                    Some(&path),
                    None,
                    "[daemon] settings are only allowed in the main configuration file",
                );
            }
        }

        for (group_name, raw_group) in raw.groups {
            let name = match Name::new(&group_name) {
                Ok(n) => n,
                Err(e) => {
                    issues.error(
                        Some(&path),
                        Some(&group_name),
                        format!("invalid group name: {e}"),
                    );
                    continue;
                }
            };
            if let Some(first) = origins.get(&name) {
                issues.error(
                    Some(&path),
                    Some(&group_name),
                    format!("group is already defined in {}", first.display()),
                );
                continue;
            }
            origins.insert(name.clone(), path.clone());
            if let Some(group) = validate_group(name, &raw_group, &path, issues) {
                config.groups.insert(group.name.clone(), group);
            }
        }
    }

    check_cross_group(&config, issues);
    config
}

fn validate_daemon(raw: RawDaemon, path: &Path, issues: &mut Issues) -> DaemonConfig {
    let mut daemon = DaemonConfig::default();
    if let Some(user) = raw.user {
        if is_valid_user_name(&user) {
            daemon.user = Some(user);
        } else {
            issues.error(
                Some(path),
                None,
                format!("daemon.user: invalid user name '{}'", user.escape_debug()),
            );
        }
    }
    if let Some(dir) = raw.state_dir {
        match AbsPath::new(&dir) {
            Ok(p) => daemon.state_dir = p,
            Err(e) => issues.error(Some(path), None, format!("daemon.state_dir: {e}")),
        }
    }
    if let Some(secs) = raw.resync_interval_secs {
        if secs > MAX_RESYNC_INTERVAL_SECS {
            issues.error(
                Some(path),
                None,
                format!("daemon.resync_interval_secs: {secs} exceeds the maximum of {MAX_RESYNC_INTERVAL_SECS}"),
            );
        } else {
            daemon.resync_interval = (secs != 0).then(|| Duration::from_secs(secs));
        }
    }
    daemon
}

/// Accepts conventional POSIX-style user names: 1–32 bytes of
/// `[A-Za-z0-9_.-]`, not starting with `-`, optionally ending in `$`.
fn is_valid_user_name(user: &str) -> bool {
    let body = user.strip_suffix('$').unwrap_or(user);
    !body.is_empty()
        && user.len() <= 32
        && !body.starts_with('-')
        && body
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

fn validate_group(
    name: Name,
    raw: &RawGroup,
    path: &Path,
    issues: &mut Issues,
) -> Option<GroupConfig> {
    let g = Some(name.as_str());
    let abs = |issues: &mut Issues, field: &str, value: &str| {
        AbsPath::new(value)
            .map_err(|e| issues.error(Some(path), g, format!("{field}: {e}")))
            .ok()
    };
    let source_root = abs(issues, "source_root", &raw.source_root);
    let target_root = abs(issues, "target_root", &raw.target_root);
    let membership = abs(issues, "membership", &raw.membership);

    let template = |issues: &mut Issues, field: &str, value: &str| {
        Template::parse(value)
            .map_err(|e| issues.error(Some(path), g, format!("{field}: {e}")))
            .ok()
    };
    let source = template(issues, "source", &raw.source);
    let target = template(issues, "target", &raw.target);

    let defaults = MountAttrs::default();
    Some(GroupConfig {
        source_root: source_root?,
        source: source?,
        target_root: target_root?,
        target: target?,
        membership: membership?,
        attrs: MountAttrs {
            read_only: raw.read_only.unwrap_or(defaults.read_only),
            noexec: raw.noexec.unwrap_or(defaults.noexec),
            nosymfollow: raw.nosymfollow.unwrap_or(defaults.nosymfollow),
        },
        origin: path.to_path_buf(),
        name,
    })
}

fn check_cross_group(config: &Config, issues: &mut Issues) {
    for group in config.groups.values() {
        for other in config.groups.values() {
            if group.membership.is_at_or_beneath(&other.target_root) {
                let message = if group.name == other.name {
                    format!(
                        "membership directory {} must not be at or beneath target_root {}",
                        group.membership, other.target_root
                    )
                } else {
                    format!(
                        "membership directory {} must not be at or beneath target_root {} of group '{}'",
                        group.membership, other.target_root, other.name
                    )
                };
                issues.error(Some(&group.origin), Some(group.name.as_str()), message);
            }
        }
    }
}

fn check_paths_exist(config: &Config, issues: &mut Issues) {
    let mut check = |group: Option<&GroupConfig>, field: &str, path: &AbsPath| {
        let origin = group.map_or_else(
            || config.files.first().map(PathBuf::as_path),
            |g| Some(g.origin.as_path()),
        );
        let name = group.map(|g| g.name.as_str());
        match std::fs::metadata(path.as_path()) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => issues.error(origin, name, format!("{field}: {path} is not a directory")),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                issues.error(origin, name, format!("{field}: {path} does not exist"));
            }
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                issues.warn(origin, name, format!("{field}: cannot verify {path}: {e}"));
            }
            Err(e) => issues.error(origin, name, format!("{field}: cannot access {path}: {e}")),
        }
    };
    check(None, "daemon.state_dir", &config.daemon.state_dir);
    for group in config.groups.values() {
        check(Some(group), "source_root", &group.source_root);
        check(Some(group), "target_root", &group.target_root);
        check(Some(group), "membership", &group.membership);
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    const GROUP: &str = r#"
[groups.research]
source_root = "/srv/example/projects"
source = "{name}/workspace"
target_root = "/srv/example/groups/research/view"
target = "{name}"
membership = "/srv/example/membership/research"
"#;

    fn main_only(content: &str) -> Result<Loaded, LoadError> {
        load_from_strs(&[(Path::new("/etc/byssus/byssus.toml"), content, true)])
    }

    fn messages(err: &LoadError) -> Vec<String> {
        err.errors().map(ToString::to_string).collect()
    }

    fn assert_error_contains(result: Result<Loaded, LoadError>, needle: &str) {
        let err = result.expect_err("expected configuration to be rejected");
        let msgs = messages(&err);
        assert!(
            msgs.iter().any(|m| m.contains(needle)),
            "no error containing {needle:?} in {msgs:#?}"
        );
    }

    #[test]
    fn abs_path_validation() {
        assert_eq!(AbsPath::new("/").unwrap().as_path(), Path::new("/"));
        assert_eq!(AbsPath::new("/a/b/").unwrap().as_path(), Path::new("/a/b"));
        assert_eq!(AbsPath::new("a"), Err(AbsPathError::NotAbsolute));
        assert_eq!(AbsPath::new(""), Err(AbsPathError::NotAbsolute));
        assert_eq!(AbsPath::new("/a//b"), Err(AbsPathError::EmptyComponent));
        assert_eq!(AbsPath::new("/a//"), Err(AbsPathError::EmptyComponent));
        assert_eq!(
            AbsPath::new("/a/./b"),
            Err(AbsPathError::DotComponent(".".into()))
        );
        assert_eq!(
            AbsPath::new("/a/.."),
            Err(AbsPathError::DotComponent("..".into()))
        );
        assert_eq!(AbsPath::new("/a\0"), Err(AbsPathError::Nul));
    }

    #[test]
    fn abs_path_beneath_is_component_based() {
        let a = AbsPath::new("/srv/view").unwrap();
        assert!(AbsPath::new("/srv/view").unwrap().is_at_or_beneath(&a));
        assert!(AbsPath::new("/srv/view/x").unwrap().is_at_or_beneath(&a));
        assert!(!AbsPath::new("/srv/viewer").unwrap().is_at_or_beneath(&a));
        assert!(!AbsPath::new("/srv").unwrap().is_at_or_beneath(&a));
    }

    #[test]
    fn minimal_group_uses_defaults() {
        let loaded = main_only(GROUP).unwrap();
        assert!(loaded.warnings.is_empty());
        let config = loaded.config;
        assert_eq!(config.daemon, DaemonConfig::default());
        let group = &config.groups[&Name::new("research").unwrap()];
        assert_eq!(group.source.as_str(), "{name}/workspace");
        assert_eq!(
            group.source_root.as_path(),
            Path::new("/srv/example/projects")
        );
        assert_eq!(group.attrs, MountAttrs::default());
        assert!(group.attrs.read_only && group.attrs.noexec && !group.attrs.nosymfollow);
    }

    #[test]
    fn full_configuration() {
        let content = format!(
            r#"
[daemon]
user = "byssus"
state_dir = "/var/lib/byssus-test"
resync_interval_secs = 0
{GROUP}
read_only = false
noexec = false
nosymfollow = true
"#
        );
        let config = main_only(&content).unwrap().config;
        assert_eq!(config.daemon.user.as_deref(), Some("byssus"));
        assert_eq!(
            config.daemon.state_dir.as_path(),
            Path::new("/var/lib/byssus-test")
        );
        assert_eq!(config.daemon.resync_interval, None);
        let group = config.groups.values().next().unwrap();
        assert_eq!(
            group.attrs,
            MountAttrs {
                read_only: false,
                noexec: false,
                nosymfollow: true
            }
        );
    }

    #[test]
    fn rejects_unknown_keys() {
        assert_error_contains(main_only("[daemon]\nfoo = 1\n"), "unknown field");
        assert_error_contains(
            main_only(&format!("{GROUP}\nextra = true\n")),
            "unknown field",
        );
        assert_error_contains(main_only("[other]\n"), "unknown field");
    }

    #[test]
    fn rejects_missing_required_fields() {
        assert_error_contains(
            main_only("[groups.g]\nsource_root = \"/a\"\n"),
            "missing field",
        );
    }

    #[test]
    fn rejects_invalid_values() {
        let replace = |from: &str, to: &str| main_only(&GROUP.replace(from, to));
        assert_error_contains(
            replace("\"/srv/example/projects\"", "\"srv/example\""),
            "source_root: path must be absolute",
        );
        assert_error_contains(
            replace("\"{name}/workspace\"", "\"workspace\""),
            "source: template must contain {name}",
        );
        assert_error_contains(
            replace("target = \"{name}\"", "target = \"../{name}\""),
            "target: template contains a '..' path component",
        );
        assert_error_contains(
            replace("[groups.research]", "[groups.\".bad\"]"),
            "invalid group name",
        );
        assert_error_contains(
            main_only("[daemon]\nuser = \"bad:user\"\n"),
            "invalid user name",
        );
        assert_error_contains(
            main_only("[daemon]\nresync_interval_secs = 100000\n"),
            "exceeds the maximum",
        );
        assert_error_contains(
            main_only("[daemon]\nstate_dir = \"relative\"\n"),
            "daemon.state_dir",
        );
    }

    #[test]
    fn user_names() {
        for ok in ["byssus", "_byssus", "svc-byssus", "a.b", "machine$", "u1"] {
            assert!(is_valid_user_name(ok), "{ok}");
        }
        for bad in ["", "-x", "a b", "a:b", "$", &"a".repeat(33)] {
            assert!(!is_valid_user_name(bad), "{bad}");
        }
    }

    #[test]
    fn reports_all_errors_at_once() {
        let content = GROUP
            .replace("\"/srv/example/projects\"", "\"x\"")
            .replace("\"{name}\"", "\"y\"");
        let err = main_only(&content).unwrap_err();
        assert_eq!(err.errors().count(), 2, "{:#?}", err.issues);
    }

    #[test]
    fn membership_beneath_target_root_rejected() {
        let content = GROUP.replace(
            "\"/srv/example/membership/research\"",
            "\"/srv/example/groups/research/view/members\"",
        );
        assert_error_contains(main_only(&content), "must not be at or beneath target_root");

        let other = r#"
[groups.other]
source_root = "/a"
source = "{name}"
target_root = "/srv/example/membership"
target = "{name}"
membership = "/b"
"#;
        assert_error_contains(main_only(&format!("{GROUP}{other}")), "of group 'other'");
    }

    #[test]
    fn fragments_and_duplicates() {
        let frag_group = GROUP.replace("research", "builds");
        let loaded = load_from_strs(&[
            (
                Path::new("/main.toml"),
                "[daemon]\nuser = \"byssus\"\n",
                true,
            ),
            (Path::new("/conf.d/a.toml"), GROUP, false),
            (Path::new("/conf.d/b.toml"), &frag_group, false),
        ])
        .unwrap();
        assert_eq!(loaded.config.groups.len(), 2);
        assert_eq!(loaded.config.files.len(), 3);

        let err = load_from_strs(&[
            (Path::new("/conf.d/a.toml"), GROUP, false),
            (Path::new("/conf.d/b.toml"), GROUP, false),
        ])
        .unwrap_err();
        assert!(messages(&err)[0].contains("already defined in /conf.d/a.toml"));

        assert_error_contains(
            load_from_strs(&[(Path::new("/conf.d/a.toml"), "[daemon]\n", false)]),
            "only allowed in the main configuration file",
        );
    }

    #[test]
    fn invalid_toml_reports_file() {
        let err = load_from_strs(&[(Path::new("/conf.d/x.toml"), "[groups", false)]).unwrap_err();
        assert!(messages(&err)[0].starts_with("/conf.d/x.toml: invalid TOML"));
    }

    // --- Filesystem-backed loading -------------------------------------------

    struct Fixture {
        dir: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
            for sub in ["etc", "etc/conf.d", "projects", "view", "members", "state"] {
                let p = dir.path().join(sub);
                fs::create_dir(&p).unwrap();
                fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
            }
            Self { dir }
        }

        fn path(&self, rel: &str) -> PathBuf {
            self.dir.path().join(rel)
        }

        fn group(&self, name: &str) -> String {
            format!(
                "[groups.{name}]\nsource_root = \"{}\"\nsource = \"{{name}}\"\ntarget_root = \"{}\"\ntarget = \"{{name}}\"\nmembership = \"{}\"\n",
                self.path("projects").display(),
                self.path("view").display(),
                self.path("members").display()
            )
        }

        fn write(&self, rel: &str, content: &str, mode: u32) {
            let p = self.path(rel);
            fs::write(&p, content).unwrap();
            fs::set_permissions(&p, fs::Permissions::from_mode(mode)).unwrap();
        }

        fn options(&self) -> LoadOptions {
            LoadOptions {
                main_file: self.path("etc/byssus.toml"),
                main_file_required: false,
                config_dir: self.path("etc/conf.d"),
                config_dir_required: false,
                ownership: OwnershipPolicy::Enforce,
                trusted_uid: rustix::process::getuid().as_raw(),
                check_paths: true,
            }
        }

        fn state_dir_config(&self) -> String {
            format!(
                "[daemon]\nstate_dir = \"{}\"\n",
                self.path("state").display()
            )
        }
    }

    #[test]
    fn loads_main_and_sorted_fragments() {
        let fx = Fixture::new();
        fx.write("etc/byssus.toml", &fx.state_dir_config(), 0o644);
        fx.write("etc/conf.d/20-b.toml", &fx.group("b"), 0o644);
        fx.write("etc/conf.d/10-a.toml", &fx.group("a"), 0o644);
        fx.write("etc/conf.d/.hidden.toml", "garbage", 0o644);
        fx.write("etc/conf.d/notes.txt", "garbage", 0o644);
        let loaded = load(&fx.options()).unwrap();
        assert!(loaded.warnings.is_empty(), "{:#?}", loaded.warnings);
        let files: Vec<_> = loaded
            .config
            .files
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap().to_owned())
            .collect();
        assert_eq!(files, ["byssus.toml", "10-a.toml", "20-b.toml"]);
        assert_eq!(loaded.config.groups.len(), 2);
    }

    #[test]
    fn missing_files() {
        let fx = Fixture::new();
        let loaded = load(&LoadOptions {
            check_paths: false,
            ..fx.options()
        })
        .unwrap();
        assert!(
            loaded.warnings[0]
                .message
                .contains("no configuration files found")
        );

        let err = load(&LoadOptions {
            main_file_required: true,
            ..fx.options()
        })
        .unwrap_err();
        assert!(messages(&err)[0].contains("file does not exist"));

        let err = load(&LoadOptions {
            config_dir: fx.path("etc/missing.d"),
            config_dir_required: true,
            ..fx.options()
        })
        .unwrap_err();
        assert!(messages(&err)[0].contains("directory does not exist"));
    }

    #[test]
    fn ownership_and_mode_enforced() {
        let fx = Fixture::new();
        fx.write("etc/byssus.toml", &fx.state_dir_config(), 0o664);
        let err = load(&fx.options()).unwrap_err();
        assert!(messages(&err)[0].contains("group- or world-writable (mode 0664)"));

        let other_uid = fx.options().trusted_uid.wrapping_add(1);
        let err = load(&LoadOptions {
            trusted_uid: other_uid,
            ..fx.options()
        })
        .unwrap_err();
        assert!(messages(&err).iter().any(|m| m.contains("expected uid")));

        let loaded = load(&LoadOptions {
            ownership: OwnershipPolicy::Warn,
            ..fx.options()
        })
        .unwrap();
        assert!(loaded.warnings[0].message.contains("world-writable"));
    }

    #[test]
    fn writable_containing_directory_rejected() {
        let fx = Fixture::new();
        fx.write("etc/conf.d/a.toml", &fx.group("a"), 0o644);
        fs::set_permissions(fx.path("etc/conf.d"), fs::Permissions::from_mode(0o777)).unwrap();
        let err = load(&LoadOptions {
            check_paths: false,
            ..fx.options()
        })
        .unwrap_err();
        assert!(
            messages(&err)
                .iter()
                .any(|m| m.contains("conf.d is group- or world-writable"))
        );
    }

    #[test]
    fn nonexistent_paths_rejected() {
        let fx = Fixture::new();
        fx.write("etc/byssus.toml", &fx.state_dir_config(), 0o644);
        let broken = fx.group("a").replace("/members\"", "/absent\"");
        fx.write("etc/conf.d/a.toml", &broken, 0o644);
        let err = load(&fx.options()).unwrap_err();
        let msgs = messages(&err);
        assert!(
            msgs[0].contains("membership:") && msgs[0].contains("does not exist"),
            "{msgs:#?}"
        );

        fx.write("projects/file", "", 0o644);
        let broken = fx.group("a").replace("/projects\"", "/projects/file\"");
        fx.write("etc/conf.d/a.toml", &broken, 0o644);
        let err = load(&fx.options()).unwrap_err();
        let msgs = messages(&err);
        assert!(
            msgs[0].contains("source_root:") && msgs[0].contains("is not a directory"),
            "{msgs:#?}"
        );
    }
}
