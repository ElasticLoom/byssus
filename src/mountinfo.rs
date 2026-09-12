//! Parser for `/proc/<pid>/mountinfo`.
//!
//! Byssus uses mountinfo only to check mount propagation (looked up by mount
//! ID) and to produce human-readable status output. It is never used to decide
//! mount ownership; see `docs/DESIGN.md`, "Mount identity".
//!
//! Line format (see `proc_pid_mountinfo(5)`):
//!
//! ```text
//! 36 35 98:0 /mnt1 /mnt2 rw,noatime master:1 - ext3 /dev/root rw,errors=continue
//! (1)(2)(3)   (4)   (5)      (6)      (7)   (8) (9)   (10)         (11)
//! ```
//!
//! Fields 4, 5 and 10 escape space, tab, newline and backslash as `\ooo`
//! octal sequences.

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;

/// Mount propagation, derived from the optional fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Propagation {
    /// No propagation fields: events neither propagate out nor in.
    Private,
    /// `shared:N` only.
    Shared {
        /// Peer group ID.
        peer_group: u64,
    },
    /// `master:N` only: receives events from a peer group but does not send.
    Slave {
        /// Master peer group ID.
        master: u64,
    },
    /// Both `shared:N` and `master:N`.
    SharedAndSlave {
        /// Peer group ID.
        peer_group: u64,
        /// Master peer group ID.
        master: u64,
    },
    /// `unbindable`.
    Unbindable,
}

/// One entry from mountinfo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountInfo {
    /// Field 1: mount ID.
    pub mount_id: u64,
    /// Field 2: parent mount ID.
    pub parent_id: u64,
    /// Field 3: device major number.
    pub major: u32,
    /// Field 3: device minor number.
    pub minor: u32,
    /// Field 4: root of the mount within its filesystem (unescaped).
    pub root: PathBuf,
    /// Field 5: mount point relative to the reader's root (unescaped).
    pub mount_point: PathBuf,
    /// Field 6: per-mount options.
    pub mount_options: String,
    /// Field 7: propagation, from the optional fields.
    pub propagation: Propagation,
    /// Field 7: the raw optional fields.
    pub optional_fields: Vec<String>,
    /// Field 9: filesystem type.
    pub fs_type: String,
    /// Field 10: mount source (unescaped).
    pub source: OsString,
    /// Field 11: per-superblock options.
    pub super_options: String,
}

impl MountInfo {
    /// Whether a per-mount option (for example `ro`) is present.
    #[must_use]
    pub fn has_mount_option(&self, option: &str) -> bool {
        self.mount_options.split(',').any(|o| o == option)
    }
}

/// A mountinfo line could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("mountinfo line {line}: {reason}")]
pub struct ParseError {
    /// 1-based line number.
    pub line: usize,
    /// What was wrong.
    pub reason: String,
}

/// A parsed mount table.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MountTable {
    /// Entries in file order.
    pub mounts: Vec<MountInfo>,
}

impl MountTable {
    /// Parses a complete mountinfo file.
    pub fn parse(content: &[u8]) -> Result<Self, ParseError> {
        let mut mounts = Vec::new();
        for (index, line) in content.split(|&b| b == b'\n').enumerate() {
            if line.is_empty() {
                continue;
            }
            let entry = parse_line(line).map_err(|reason| ParseError {
                line: index + 1,
                reason,
            })?;
            mounts.push(entry);
        }
        Ok(Self { mounts })
    }

    /// Reads and parses `/proc/self/mountinfo`.
    pub fn read_self() -> std::io::Result<Self> {
        let content = std::fs::read("/proc/self/mountinfo")?;
        Self::parse(&content).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    /// Finds a mount by its ID.
    #[must_use]
    pub fn by_id(&self, mount_id: u64) -> Option<&MountInfo> {
        self.mounts.iter().find(|m| m.mount_id == mount_id)
    }
}

fn parse_line(line: &[u8]) -> Result<MountInfo, String> {
    // Paths may contain arbitrary non-UTF-8 bytes (only space, tab, newline
    // and backslash are escaped), so split on raw bytes.
    let fields: Vec<&[u8]> = line.split(|&b| b == b' ').collect();
    let separator = fields
        .iter()
        .position(|&f| f == b"-")
        .ok_or_else(|| "missing '-' separator".to_owned())?;
    if separator < 6 {
        return Err(format!(
            "expected at least 6 fields before the separator, found {separator}"
        ));
    }
    if fields.len() != separator + 4 {
        return Err(format!(
            "expected 3 fields after the separator, found {}",
            fields.len() - separator - 1
        ));
    }

    let mount_id = parse_num(fields[0], "mount ID")?;
    let parent_id = parse_num(fields[1], "parent ID")?;
    let device = text(fields[2]);
    let (major, minor) = device
        .split_once(':')
        .ok_or_else(|| format!("invalid device number '{device}'"))?;
    let major = parse_num(major.as_bytes(), "device major")?;
    let minor = parse_num(minor.as_bytes(), "device minor")?;
    let optional_fields: Vec<String> = fields[6..separator].iter().map(|&f| text(f)).collect();
    let propagation = classify_propagation(&optional_fields)?;

    Ok(MountInfo {
        mount_id,
        parent_id,
        major,
        minor,
        root: PathBuf::from(OsString::from_vec(unescape(fields[3])?)),
        mount_point: PathBuf::from(OsString::from_vec(unescape(fields[4])?)),
        mount_options: text(fields[5]),
        propagation,
        optional_fields,
        fs_type: text(fields[separator + 1]),
        source: OsString::from_vec(unescape(fields[separator + 2])?),
        super_options: text(fields[separator + 3]),
    })
}

fn text(field: &[u8]) -> String {
    String::from_utf8_lossy(field).into_owned()
}

fn parse_num<T: std::str::FromStr>(field: &[u8], what: &str) -> Result<T, String> {
    std::str::from_utf8(field)
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("invalid {what} '{}'", field.escape_ascii()))
}

fn classify_propagation(optional: &[String]) -> Result<Propagation, String> {
    let mut shared = None;
    let mut master = None;
    let mut unbindable = false;
    for field in optional {
        if let Some(v) = field.strip_prefix("shared:") {
            shared = Some(parse_num(v.as_bytes(), "shared peer group")?);
        } else if let Some(v) = field.strip_prefix("master:") {
            master = Some(parse_num(v.as_bytes(), "master peer group")?);
        } else if field == "unbindable" {
            unbindable = true;
        }
        // Other fields (e.g. `propagate_from:N`) are informational and
        // future kernels may add more; they do not affect classification.
    }
    Ok(match (shared, master, unbindable) {
        (_, _, true) => Propagation::Unbindable,
        (Some(peer_group), Some(master), false) => {
            Propagation::SharedAndSlave { peer_group, master }
        }
        (Some(peer_group), None, false) => Propagation::Shared { peer_group },
        (None, Some(master), false) => Propagation::Slave { master },
        (None, None, false) => Propagation::Private,
    })
}

/// Decodes the kernel's `\ooo` octal escapes.
fn unescape(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let field = bytes.escape_ascii();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            let digits = bytes
                .get(i + 1..i + 4)
                .filter(|d| d.iter().all(|b| (b'0'..=b'7').contains(b)))
                .ok_or_else(|| format!("invalid escape sequence in '{field}'"))?;
            let value = digits
                .iter()
                .fold(0u32, |acc, &d| acc * 8 + u32::from(d - b'0'));
            out.push(u8::try_from(value).map_err(|_| format!("escape out of range in '{field}'"))?);
            i += 4;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    const SAMPLE: &str = "\
22 1 259:2 / / rw,relatime shared:1 - ext4 /dev/nvme0n1p2 rw,errors=remount-ro
23 22 0:21 / /proc rw,nosuid,nodev,noexec,relatime shared:12 - proc proc rw
36 35 98:0 /mnt1 /mnt2 rw,noatime master:1 - ext3 /dev/root rw,errors=continue
40 22 259:2 /srv/with\\040space /srv/view/a\\011b ro,nosuid,nodev,noexec,relatime shared:5 master:3 - ext4 /dev/nvme0n1p2 rw
41 22 0:50 / /private rw,relatime - tmpfs my\\134source rw,size=1024k
42 22 0:51 / /unbind rw unbindable - tmpfs tmpfs rw
43 22 0:52 / /propfrom rw master:7 propagate_from:2 - tmpfs tmpfs rw
";

    #[test]
    fn parses_sample() {
        let table = MountTable::parse(SAMPLE.as_bytes()).unwrap();
        assert_eq!(table.mounts.len(), 7);

        let root = table.by_id(22).unwrap();
        assert_eq!(root.parent_id, 1);
        assert_eq!((root.major, root.minor), (259, 2));
        assert_eq!(root.mount_point, Path::new("/"));
        assert_eq!(root.propagation, Propagation::Shared { peer_group: 1 });
        assert_eq!(root.fs_type, "ext4");
        assert_eq!(root.source, "/dev/nvme0n1p2");
        assert_eq!(root.super_options, "rw,errors=remount-ro");

        let slave = table.by_id(36).unwrap();
        assert_eq!(slave.root, Path::new("/mnt1"));
        assert_eq!(slave.mount_point, Path::new("/mnt2"));
        assert_eq!(slave.propagation, Propagation::Slave { master: 1 });
    }

    #[test]
    fn unescapes_paths_and_source() {
        let table = MountTable::parse(SAMPLE.as_bytes()).unwrap();
        let m = table.by_id(40).unwrap();
        assert_eq!(m.root, Path::new("/srv/with space"));
        assert_eq!(m.mount_point, Path::new("/srv/view/a\tb"));
        assert!(m.has_mount_option("ro"));
        assert!(m.has_mount_option("noexec"));
        assert!(!m.has_mount_option("no"));
        assert_eq!(
            m.propagation,
            Propagation::SharedAndSlave {
                peer_group: 5,
                master: 3
            }
        );
        assert_eq!(table.by_id(41).unwrap().source, "my\\source");
    }

    #[test]
    fn propagation_variants() {
        let table = MountTable::parse(SAMPLE.as_bytes()).unwrap();
        assert_eq!(table.by_id(41).unwrap().propagation, Propagation::Private);
        assert_eq!(
            table.by_id(42).unwrap().propagation,
            Propagation::Unbindable
        );
        let m = table.by_id(43).unwrap();
        assert_eq!(m.propagation, Propagation::Slave { master: 7 });
        assert_eq!(m.optional_fields, ["master:7", "propagate_from:2"]);
        assert!(table.by_id(999).is_none());
    }

    #[test]
    fn unescape_edge_cases() {
        assert_eq!(unescape(b"plain").unwrap(), b"plain");
        assert_eq!(unescape(b"\\012").unwrap(), b"\n");
        assert_eq!(unescape(b"a\\134\\040").unwrap(), b"a\\ ");
        assert_eq!(unescape(b"\\377").unwrap(), [0xff]);
        assert_eq!(unescape(b"raw\xff").unwrap(), b"raw\xff");
        assert!(unescape(b"\\400").is_err());
        assert!(unescape(b"\\01").is_err());
        assert!(unescape(b"\\").is_err());
        assert!(unescape(b"\\089").is_err());
    }

    #[test]
    fn rejects_malformed_lines() {
        let cases = [
            (
                "22 1 259:2 / / rw shared:1 ext4 /dev/x rw",
                "missing '-' separator",
            ),
            ("22 1 259:2 / / - ext4 /dev/x rw", "at least 6 fields"),
            ("22 1 259:2 / / rw - ext4 /dev/x", "3 fields after"),
            ("x 1 259:2 / / rw - ext4 /dev/x rw", "invalid mount ID"),
            ("22 1 2592 / / rw - ext4 /dev/x rw", "invalid device number"),
            (
                "22 1 259:2 / / rw shared:x - ext4 /dev/x rw",
                "invalid shared peer group",
            ),
            ("22 1 259:2 /\\9 / rw - ext4 /dev/x rw", "invalid escape"),
        ];
        for (line, expected) in cases {
            let err = MountTable::parse(format!("{SAMPLE}{line}\n").as_bytes()).unwrap_err();
            assert_eq!(err.line, 8, "{line}");
            assert!(err.reason.contains(expected), "{line}: {}", err.reason);
        }
    }

    #[test]
    fn accepts_non_utf8_paths() {
        let line = b"50 22 0:60 / /srv/caf\xe9 rw - tmpfs tmpfs rw\n";
        let table = MountTable::parse(line).unwrap();
        assert_eq!(
            table.mounts[0].mount_point.as_os_str().as_encoded_bytes(),
            b"/srv/caf\xe9"
        );
    }

    #[test]
    fn reads_own_mountinfo() {
        let table = MountTable::read_self().unwrap();
        assert!(!table.mounts.is_empty());
    }
}
