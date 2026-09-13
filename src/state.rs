//! The state file: Byssus's record of the mounts it created.
//!
//! See `docs/DESIGN.md`, "State file".

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::PathBuf;

use rustix::fs::{AtFlags, Mode, OFlags};

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::config::{AbsPath, MountAttrs};
use crate::identity::{DevIno, MountIdentity};
use crate::name::Name;

/// Current state file format version.
pub const STATE_VERSION: u64 = 1;
/// State file name within the state directory.
pub const STATE_FILE_NAME: &str = "state.json";
/// Permission bits of the state file.
pub const STATE_FILE_MODE: u32 = 0o640;

/// A mount Byssus created.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MountRecord {
    /// Group name.
    pub group: Name,
    /// Member name.
    pub name: Name,
    /// Source root at the time of mounting.
    pub source_root: AbsPath,
    /// Interpolated source path relative to `source_root`.
    pub source: String,
    /// Target root at the time of mounting.
    pub target_root: AbsPath,
    /// Interpolated target path relative to `target_root`.
    pub target: String,
    /// Reusable mount ID.
    pub mnt_id: u64,
    /// Unique mount ID, if the kernel supports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mnt_id_unique: Option<u64>,
    /// Device major number of the mount root.
    pub root_dev_major: u32,
    /// Device minor number of the mount root.
    pub root_dev_minor: u32,
    /// Inode number of the mount root.
    pub root_ino: u64,
    /// `MOUNT_ATTR_RDONLY` was applied.
    pub read_only: bool,
    /// `MOUNT_ATTR_NOEXEC` was applied.
    pub noexec: bool,
    /// `MOUNT_ATTR_NOSYMFOLLOW` was applied.
    pub nosymfollow: bool,
    /// When the mount was created.
    pub created_at: Timestamp,
}

impl MountRecord {
    /// The recorded identity.
    #[must_use]
    pub fn identity(&self) -> MountIdentity {
        MountIdentity {
            mnt_id: self.mnt_id,
            mnt_id_unique: self.mnt_id_unique,
            root: DevIno {
                dev_major: self.root_dev_major,
                dev_minor: self.root_dev_minor,
                ino: self.root_ino,
            },
        }
    }

    /// The attributes that were applied (in addition to `nosuid` and `nodev`).
    #[must_use]
    pub fn attrs(&self) -> MountAttrs {
        MountAttrs {
            read_only: self.read_only,
            noexec: self.noexec,
            nosymfollow: self.nosymfollow,
        }
    }

    /// The `(group, name)` key.
    #[must_use]
    pub fn key(&self) -> RecordKey {
        RecordKey {
            group: self.group.clone(),
            name: self.name.clone(),
        }
    }
}

/// Identifies a record: at most one record exists per group and member.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RecordKey {
    /// Group name.
    pub group: Name,
    /// Member name.
    pub name: Name,
}

impl std::fmt::Display for RecordKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.group, self.name)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StateDocument {
    version: u64,
    mounts: Vec<MountRecord>,
}

/// Why state content could not be decoded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    /// The document declares a format version this binary does not support.
    #[error("unsupported state file version {0} (this build supports version {STATE_VERSION})")]
    UnsupportedVersion(u64),
    /// The document is malformed.
    #[error("malformed state file: {0}")]
    Malformed(String),
}

/// The in-memory set of mount records.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct State {
    records: BTreeMap<RecordKey, MountRecord>,
}

impl State {
    /// Looks up the record for a member.
    #[must_use]
    pub fn get(&self, key: &RecordKey) -> Option<&MountRecord> {
        self.records.get(key)
    }

    /// Inserts or replaces a record, returning the previous one.
    pub fn insert(&mut self, record: MountRecord) -> Option<MountRecord> {
        self.records.insert(record.key(), record)
    }

    /// Removes a record.
    pub fn remove(&mut self, key: &RecordKey) -> Option<MountRecord> {
        self.records.remove(key)
    }

    /// All records, ordered by group then member.
    pub fn records(&self) -> impl Iterator<Item = &MountRecord> {
        self.records.values()
    }

    /// Number of records.
    #[must_use]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Whether there are no records.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Serializes to the on-disk JSON format.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let doc = StateDocument {
            version: STATE_VERSION,
            mounts: self.records.values().cloned().collect(),
        };
        let mut out = serde_json::to_vec_pretty(&doc).unwrap_or_else(|e| {
            // Every field is a string, integer or timestamp; serialization
            // cannot fail.
            unreachable!("state serialization failed: {e}")
        });
        out.push(b'\n');
        out
    }

    /// Decodes the on-disk JSON format.
    pub fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let value: serde_json::Value =
            serde_json::from_slice(bytes).map_err(|e| DecodeError::Malformed(e.to_string()))?;
        let version = value
            .get("version")
            .ok_or_else(|| DecodeError::Malformed("missing 'version'".into()))?
            .as_u64()
            .ok_or_else(|| DecodeError::Malformed("'version' is not an unsigned integer".into()))?;
        if version != STATE_VERSION {
            return Err(DecodeError::UnsupportedVersion(version));
        }
        let doc: StateDocument =
            serde_json::from_value(value).map_err(|e| DecodeError::Malformed(e.to_string()))?;

        let mut state = Self::default();
        for record in doc.mounts {
            for (field, path) in [("source", &record.source), ("target", &record.target)] {
                if !is_clean_relative(path) {
                    return Err(DecodeError::Malformed(format!(
                        "record {}: invalid {field} path '{}'",
                        record.key(),
                        path.escape_debug()
                    )));
                }
            }
            let key = record.key();
            if state.insert(record).is_some() {
                return Err(DecodeError::Malformed(format!(
                    "duplicate record for {key}"
                )));
            }
        }
        Ok(state)
    }
}

/// A non-empty relative path with no empty, `.` or `..` components.
fn is_clean_relative(path: &str) -> bool {
    !path.is_empty()
        && !path.contains('\0')
        && path
            .split('/')
            .all(|c| !c.is_empty() && c != "." && c != "..")
}

/// Result of loading the state file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadOutcome {
    /// The state file was read successfully.
    Loaded(State),
    /// There is no state file.
    Missing,
    /// The state file was unreadable as state. When loaded for writing, it has
    /// been renamed aside to `preserved_as`.
    Corrupt {
        /// Why it could not be decoded.
        reason: String,
        /// Where the corrupt file was moved, if it was.
        preserved_as: Option<PathBuf>,
    },
}

impl LoadOutcome {
    /// The loaded state, or empty state if missing or corrupt.
    #[must_use]
    pub fn into_state(self) -> State {
        match self {
            Self::Loaded(state) => state,
            Self::Missing | Self::Corrupt { .. } => State::default(),
        }
    }
}

/// Why the state file could not be loaded at all.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    /// The file declares an unsupported version; it is never overwritten.
    #[error("{path}: {source}")]
    UnsupportedVersion {
        /// State file path.
        path: PathBuf,
        /// The decode error.
        source: DecodeError,
    },
    /// An I/O error other than the file not existing.
    #[error("{path}: {source}")]
    Io {
        /// Path involved.
        path: PathBuf,
        /// The I/O error.
        source: io::Error,
    },
}

/// Reads and writes the state file through a descriptor for the state
/// directory.
#[derive(Debug)]
pub struct StateStore {
    dir: OwnedFd,
    dir_path: PathBuf,
}

impl StateStore {
    /// Opens the state directory.
    pub fn open(dir: impl Into<PathBuf>) -> io::Result<Self> {
        let dir_path = dir.into();
        let dir = rustix::fs::open(
            &dir_path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        Ok(Self { dir, dir_path })
    }

    /// The state directory descriptor (for taking the state lock).
    #[must_use]
    pub fn dir_fd(&self) -> BorrowedFd<'_> {
        self.dir.as_fd()
    }

    /// Path of the state file, for messages.
    #[must_use]
    pub fn path(&self) -> PathBuf {
        self.dir_path.join(STATE_FILE_NAME)
    }

    /// Loads state without modifying anything on disk (for read-only
    /// commands).
    pub fn load_read_only(&self) -> Result<LoadOutcome, LoadError> {
        self.load_inner(None)
    }

    /// Loads state for a writer. A corrupt file is renamed aside to
    /// `state.json.corrupt-<timestamp>` so it is preserved for the operator.
    pub fn load_for_write(&self, now: Timestamp) -> Result<LoadOutcome, LoadError> {
        self.load_inner(Some(now))
    }

    fn io_error(&self, source: impl Into<io::Error>) -> LoadError {
        LoadError::Io {
            path: self.path(),
            source: source.into(),
        }
    }

    fn load_inner(&self, rename_corrupt_at: Option<Timestamp>) -> Result<LoadOutcome, LoadError> {
        let fd = match rustix::fs::openat(
            &self.dir,
            STATE_FILE_NAME,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(rustix::io::Errno::NOENT) => return Ok(LoadOutcome::Missing),
            Err(e) => return Err(self.io_error(e)),
        };
        let mut bytes = Vec::new();
        File::from(fd)
            .read_to_end(&mut bytes)
            .map_err(|e| self.io_error(e))?;
        match State::decode(&bytes) {
            Ok(state) => Ok(LoadOutcome::Loaded(state)),
            Err(source @ DecodeError::UnsupportedVersion(_)) => {
                Err(LoadError::UnsupportedVersion {
                    path: self.path(),
                    source,
                })
            }
            Err(DecodeError::Malformed(reason)) => {
                let preserved_as = match rename_corrupt_at {
                    None => None,
                    Some(now) => {
                        let aside = format!(
                            "{STATE_FILE_NAME}.corrupt-{}",
                            now.strftime("%Y%m%dT%H%M%S%.fZ")
                        );
                        rustix::fs::renameat(&self.dir, STATE_FILE_NAME, &self.dir, aside.as_str())
                            .map_err(|e| self.io_error(e))?;
                        Some(self.dir_path.join(aside))
                    }
                };
                Ok(LoadOutcome::Corrupt {
                    reason,
                    preserved_as,
                })
            }
        }
    }

    /// Atomically replaces the state file: write a temporary file with mode
    /// `0640`, `fsync` it, rename it over the state file, then `fsync` the
    /// directory.
    pub fn save(&self, state: &State) -> io::Result<()> {
        let result = self.write_atomically(&state.encode());
        if result.is_err() {
            let _ = rustix::fs::unlinkat(&self.dir, TMP_FILE_NAME, AtFlags::empty());
        }
        result
    }

    fn write_atomically(&self, contents: &[u8]) -> io::Result<()> {
        let fd = rustix::fs::openat(
            &self.dir,
            TMP_FILE_NAME,
            OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::from_raw_mode(STATE_FILE_MODE),
        )?;
        // The creation mode is filtered by the umask; set it exactly.
        rustix::fs::fchmod(&fd, Mode::from_raw_mode(STATE_FILE_MODE))?;
        let mut file = File::from(fd);
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);
        rustix::fs::renameat(&self.dir, TMP_FILE_NAME, &self.dir, STATE_FILE_NAME)?;
        rustix::fs::fsync(&self.dir)?;
        Ok(())
    }
}

const TMP_FILE_NAME: &str = "state.json.tmp";

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::MetadataExt;

    use super::*;

    fn record(group: &str, name: &str) -> MountRecord {
        MountRecord {
            group: Name::new(group).unwrap(),
            name: Name::new(name).unwrap(),
            source_root: AbsPath::new("/srv/example/projects").unwrap(),
            source: format!("{name}/workspace"),
            target_root: AbsPath::new("/srv/example/groups/view").unwrap(),
            target: name.to_owned(),
            mnt_id: 4132,
            mnt_id_unique: Some(2_147_487_780),
            root_dev_major: 8,
            root_dev_minor: 1,
            root_ino: 1_842_211,
            read_only: true,
            noexec: true,
            nosymfollow: false,
            created_at: "2026-09-12T14:30:01Z".parse().unwrap(),
        }
    }

    fn sample_state() -> State {
        let mut state = State::default();
        state.insert(record("research", "libcurl"));
        let mut r = record("research", "openssl");
        r.mnt_id_unique = None;
        state.insert(r);
        state
    }

    #[test]
    fn encode_format_is_stable() {
        let mut state = State::default();
        state.insert(record("research", "libcurl"));
        let text = String::from_utf8(state.encode()).unwrap();
        let expected = r#"{
  "version": 1,
  "mounts": [
    {
      "group": "research",
      "name": "libcurl",
      "source_root": "/srv/example/projects",
      "source": "libcurl/workspace",
      "target_root": "/srv/example/groups/view",
      "target": "libcurl",
      "mnt_id": 4132,
      "mnt_id_unique": 2147487780,
      "root_dev_major": 8,
      "root_dev_minor": 1,
      "root_ino": 1842211,
      "read_only": true,
      "noexec": true,
      "nosymfollow": false,
      "created_at": "2026-09-12T14:30:01Z"
    }
  ]
}
"#;
        assert_eq!(text, expected);
    }

    #[test]
    fn round_trip() {
        let state = sample_state();
        let decoded = State::decode(&state.encode()).unwrap();
        assert_eq!(decoded, state);
        let openssl = decoded.get(&record("research", "openssl").key()).unwrap();
        assert_eq!(openssl.mnt_id_unique, None);
        assert!(!String::from_utf8(state.encode()).unwrap().contains("null"));
    }

    #[test]
    fn identity_and_key() {
        let r = record("g", "n");
        let id = r.identity();
        assert_eq!(id.mnt_id, 4132);
        assert_eq!(id.root.ino, 1_842_211);
        assert_eq!(r.key().to_string(), "g/n");
    }

    #[test]
    fn version_handling() {
        assert_eq!(
            State::decode(br#"{"version": 2, "mounts": []}"#),
            Err(DecodeError::UnsupportedVersion(2))
        );
        assert!(matches!(
            State::decode(br#"{"mounts": []}"#),
            Err(DecodeError::Malformed(m)) if m.contains("missing 'version'")
        ));
        assert!(matches!(
            State::decode(br#"{"version": "1", "mounts": []}"#),
            Err(DecodeError::Malformed(_))
        ));
        assert_eq!(
            State::decode(br#"{"version": 1, "mounts": []}"#),
            Ok(State::default())
        );
    }

    #[test]
    fn rejects_malformed_documents() {
        let valid = String::from_utf8(sample_state().encode()).unwrap();
        let cases = [
            ("not json".to_owned(), "expected"),
            (
                valid.replace("\"version\": 1,", "\"version\": 1, \"extra\": 0,"),
                "unknown field",
            ),
            (
                valid.replace("\"root_ino\"", "\"root_inode\""),
                "unknown field",
            ),
            (
                valid.replace("\"libcurl/workspace\"", "\"../x\""),
                "invalid source path",
            ),
            (
                valid.replace("\"target\": \"libcurl\"", "\"target\": \"/abs\""),
                "invalid target path",
            ),
            (
                valid.replace("\"name\": \"libcurl\"", "\"name\": \".bad\""),
                "begins with '.'",
            ),
            (
                valid.replace("\"/srv/example/projects\"", "\"relative\""),
                "absolute",
            ),
            (
                valid.replace("\"name\": \"openssl\"", "\"name\": \"libcurl\""),
                "duplicate record for research/libcurl",
            ),
        ];
        for (doc, needle) in cases {
            match State::decode(doc.as_bytes()) {
                Err(DecodeError::Malformed(m)) => assert!(m.contains(needle), "{needle}: {m}"),
                other => panic!("{needle}: unexpected {other:?}"),
            }
        }
    }

    #[test]
    fn clean_relative_paths() {
        for ok in ["a", "a/b", "a.b/..c", "x/y/z"] {
            assert!(is_clean_relative(ok), "{ok}");
        }
        for bad in ["", "/a", "a/", "a//b", ".", "a/../b", "a/./b", "a\0"] {
            assert!(!is_clean_relative(bad), "{bad}");
        }
    }

    #[test]
    fn store_missing_save_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::open(dir.path()).unwrap();
        assert_eq!(store.load_read_only().unwrap(), LoadOutcome::Missing);

        let state = sample_state();
        store.save(&state).unwrap();
        let meta = fs::metadata(store.path()).unwrap();
        assert_eq!(meta.mode() & 0o7777, STATE_FILE_MODE);
        assert!(!dir.path().join("state.json.tmp").exists());

        assert_eq!(
            store.load_for_write(Timestamp::now()).unwrap(),
            LoadOutcome::Loaded(state.clone())
        );

        // Overwrite with different content.
        let mut smaller = state;
        smaller.remove(&record("research", "libcurl").key());
        store.save(&smaller).unwrap();
        assert_eq!(store.load_read_only().unwrap().into_state(), smaller);
    }

    #[test]
    fn corrupt_file_preserved_only_for_writers() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::open(dir.path()).unwrap();
        fs::write(store.path(), b"{garbage").unwrap();

        let outcome = store.load_read_only().unwrap();
        assert!(matches!(
            outcome,
            LoadOutcome::Corrupt {
                preserved_as: None,
                ..
            }
        ));
        assert!(store.path().exists());

        let now: Timestamp = "2026-09-12T14:30:01.5Z".parse().unwrap();
        match store.load_for_write(now).unwrap() {
            LoadOutcome::Corrupt {
                preserved_as: Some(aside),
                ..
            } => {
                assert_eq!(
                    aside.file_name().unwrap(),
                    "state.json.corrupt-20260912T143001.5Z"
                );
                assert_eq!(fs::read(&aside).unwrap(), b"{garbage");
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(!store.path().exists());
    }

    #[test]
    fn unsupported_version_is_an_error_and_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::open(dir.path()).unwrap();
        fs::write(store.path(), br#"{"version": 9, "mounts": []}"#).unwrap();
        assert!(matches!(
            store.load_for_write(Timestamp::now()),
            Err(LoadError::UnsupportedVersion { .. })
        ));
        assert!(store.path().exists());
    }

    #[test]
    fn save_refuses_symlinked_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::open(dir.path()).unwrap();
        let victim = dir.path().join("victim");
        fs::write(&victim, b"original").unwrap();
        std::os::unix::fs::symlink(&victim, dir.path().join("state.json.tmp")).unwrap();
        assert!(store.save(&sample_state()).is_err());
        assert_eq!(fs::read(&victim).unwrap(), b"original");
    }

    #[test]
    fn symlinked_state_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::open(dir.path()).unwrap();
        let elsewhere = dir.path().join("elsewhere.json");
        fs::write(&elsewhere, sample_state().encode()).unwrap();
        std::os::unix::fs::symlink(&elsewhere, store.path()).unwrap();
        assert!(matches!(store.load_read_only(), Err(LoadError::Io { .. })));
    }

    #[test]
    fn unreadable_file_is_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::open(dir.path()).unwrap();
        fs::create_dir(store.path()).unwrap();
        assert!(matches!(store.load_read_only(), Err(LoadError::Io { .. })));
    }
}
