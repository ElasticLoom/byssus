//! Validation of member and group names.
//!
//! Member names come from membership file names, which are untrusted input.
//! Group names come from configuration. Both follow the same rules (see
//! `docs/DESIGN.md`, "Names"):
//!
//! - only `A-Z`, `a-z`, `0-9`, `.`, `_`, `-`;
//! - 1 to 255 bytes;
//! - must not begin with `.`.
//!
//! Because a valid name can contain neither `/` nor be `.` or `..`, it can be
//! interpolated into a path template without introducing new path components
//! or traversal.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Maximum length of a name in bytes (the Linux `NAME_MAX`).
pub const MAX_NAME_LEN: usize = 255;

/// A validated member or group name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Name(String);

/// Why a candidate name was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NameError {
    /// The name is empty.
    #[error("name is empty")]
    Empty,
    /// The name is longer than [`MAX_NAME_LEN`] bytes.
    #[error("name is {0} bytes long; the maximum is {MAX_NAME_LEN}")]
    TooLong(usize),
    /// The name begins with `.`.
    #[error("name begins with '.'")]
    LeadingDot,
    /// The name contains a byte outside the allowlist.
    #[error("name contains disallowed byte {byte:#04x} at offset {offset}")]
    InvalidByte {
        /// The offending byte.
        byte: u8,
        /// Its offset within the name.
        offset: usize,
    },
}

const fn is_allowed_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-')
}

impl Name {
    /// Validates a name given as raw bytes, such as a directory entry name.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, NameError> {
        if bytes.is_empty() {
            return Err(NameError::Empty);
        }
        if bytes.len() > MAX_NAME_LEN {
            return Err(NameError::TooLong(bytes.len()));
        }
        if bytes[0] == b'.' {
            return Err(NameError::LeadingDot);
        }
        if let Some(offset) = bytes.iter().position(|&b| !is_allowed_byte(b)) {
            return Err(NameError::InvalidByte {
                byte: bytes[offset],
                offset,
            });
        }
        // Every byte is allowlisted ASCII, so each maps to exactly one char.
        Ok(Self(bytes.iter().copied().map(char::from).collect()))
    }

    /// Validates a name given as a string.
    pub fn new(s: &str) -> Result<Self, NameError> {
        Self::from_bytes(s.as_bytes())
    }

    /// Returns the name as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for Name {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::str::FromStr for Name {
    type Err = NameError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl Serialize for Name {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Name {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::new(&s).map_err(serde::de::Error::custom)
    }
}

/// Identifies a group: a statically configured group (`acme`), or a group
/// discovered in a group set, named by the set and one or two directory
/// levels (`research/acme`, or `projects/acme/research` for a set with
/// subgroups).
///
/// Names cannot contain `/`, so the forms never collide.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GroupId {
    set: Option<Name>,
    path: Vec<Name>,
}

/// The most directory levels a group set can have.
pub const MAX_SET_DEPTH: usize = 2;

impl GroupId {
    /// A statically configured group.
    #[must_use]
    pub fn statically(group: Name) -> Self {
        Self {
            set: None,
            path: vec![group],
        }
    }

    /// A group (or, with fewer levels than the set's depth, a directory
    /// prefix of groups) in a group set.
    ///
    /// # Panics
    ///
    /// If `path` is empty or longer than [`MAX_SET_DEPTH`].
    #[must_use]
    pub fn in_set(set: Name, path: Vec<Name>) -> Self {
        assert!(
            !path.is_empty() && path.len() <= MAX_SET_DEPTH,
            "invalid group set path length"
        );
        Self {
            set: Some(set),
            path,
        }
    }

    /// Parses `group`, `set/group` or `set/group/subgroup`.
    pub fn parse(s: &str) -> Result<Self, NameError> {
        let mut parts = s.split('/');
        let first = Name::new(parts.next().unwrap_or_default())?;
        let rest: Vec<Name> = parts.map(Name::new).collect::<Result<_, _>>()?;
        match rest.len() {
            0 => Ok(Self::statically(first)),
            n if n <= MAX_SET_DEPTH => Ok(Self::in_set(first, rest)),
            _ => Err(NameError::InvalidByte {
                byte: b'/',
                offset: s.rfind('/').unwrap_or(0),
            }),
        }
    }

    /// The set this group belongs to, if any.
    #[must_use]
    pub fn set(&self) -> Option<&Name> {
        self.set.as_ref()
    }

    /// The group's name components: the static group name, or the set
    /// directory levels.
    #[must_use]
    pub fn path(&self) -> &[Name] {
        &self.path
    }

    /// Whether `self` is `prefix` or lies beneath it (same set, and
    /// `prefix`'s path is a leading part of this path).
    #[must_use]
    pub fn starts_with(&self, prefix: &Self) -> bool {
        self.set == prefix.set && self.path.starts_with(&prefix.path)
    }
}

impl fmt::Display for GroupId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(set) = &self.set {
            write!(f, "{set}/")?;
        }
        let parts: Vec<&str> = self.path.iter().map(Name::as_str).collect();
        f.write_str(&parts.join("/"))
    }
}

impl std::str::FromStr for GroupId {
    type Err = NameError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl Serialize for GroupId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for GroupId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::parse(&s).map_err(serde::de::Error::custom)
    }
}

/// Renders arbitrary bytes (such as a rejected file name) safely for logs:
/// printable ASCII is kept, everything else is escaped.
#[must_use]
pub fn display_bytes(bytes: &[u8]) -> String {
    bytes.escape_ascii().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_typical_names() {
        for s in ["libcurl", "openssl-3", "a", "A_b.c-d", "0", "x.", "name..x"] {
            assert_eq!(Name::new(s).unwrap().as_str(), s, "{s}");
        }
    }

    #[test]
    fn accepts_maximum_length() {
        let s = "a".repeat(MAX_NAME_LEN);
        assert!(Name::new(&s).is_ok());
    }

    #[test]
    fn rejects_too_long() {
        let s = "a".repeat(MAX_NAME_LEN + 1);
        assert_eq!(Name::new(&s), Err(NameError::TooLong(MAX_NAME_LEN + 1)));
    }

    #[test]
    fn rejects_empty() {
        assert_eq!(Name::new(""), Err(NameError::Empty));
    }

    #[test]
    fn rejects_leading_dot_including_dot_entries() {
        for s in [".", "..", ".hidden", ".name.swp"] {
            assert_eq!(Name::new(s), Err(NameError::LeadingDot), "{s}");
        }
    }

    #[test]
    fn rejects_disallowed_bytes() {
        let cases: &[(&[u8], u8, usize)] = &[
            (b"a/b", b'/', 1),
            (b"a b", b' ', 1),
            (b"a\0", 0, 1),
            (b"a\nb", b'\n', 1),
            (b"{name}", b'{', 0),
            (b"caf\xc3\xa9", 0xc3, 3),
            (b"a:b", b':', 1),
            (b"a\\b", b'\\', 1),
            (b"a*", b'*', 1),
        ];
        for &(input, byte, offset) in cases {
            assert_eq!(
                Name::from_bytes(input),
                Err(NameError::InvalidByte { byte, offset }),
                "{}",
                display_bytes(input)
            );
        }
    }

    #[test]
    fn serde_round_trip_and_validation() {
        let name: Name = serde_json::from_str("\"libcurl\"").unwrap();
        assert_eq!(serde_json::to_string(&name).unwrap(), "\"libcurl\"");
        assert!(serde_json::from_str::<Name>("\"../etc\"").is_err());
    }

    #[test]
    fn group_ids() {
        let s = GroupId::parse("acme").unwrap();
        assert_eq!(s.set(), None);
        assert_eq!(s.to_string(), "acme");
        let g = GroupId::parse("research/acme").unwrap();
        assert_eq!(g.set().unwrap().as_str(), "research");
        assert_eq!(g.path().len(), 1);
        assert_eq!(g.to_string(), "research/acme");
        let sub = GroupId::parse("projects/acme/research").unwrap();
        assert_eq!(
            sub.path().iter().map(Name::as_str).collect::<Vec<_>>(),
            ["acme", "research"]
        );
        assert_eq!(sub.to_string(), "projects/acme/research");
        assert_ne!(s, g);
        for bad in [
            "",
            "/acme",
            "research/",
            "a/b/c/d",
            ".x",
            "research/.x",
            "a b",
            "a//b",
        ] {
            assert!(GroupId::parse(bad).is_err(), "{bad}");
        }
        let json = serde_json::to_string(&sub).unwrap();
        assert_eq!(json, "\"projects/acme/research\"");
        assert_eq!(serde_json::from_str::<GroupId>(&json).unwrap(), sub);
        assert!(serde_json::from_str::<GroupId>("\"a/b/c/d\"").is_err());
    }

    #[test]
    fn group_id_prefixes() {
        let org = GroupId::parse("projects/acme").unwrap();
        let sub = GroupId::parse("projects/acme/research").unwrap();
        assert!(sub.starts_with(&org));
        assert!(sub.starts_with(&sub));
        assert!(!org.starts_with(&sub));
        assert!(!sub.starts_with(&GroupId::parse("other/acme").unwrap()));
        assert!(!sub.starts_with(&GroupId::parse("projects/acm").unwrap()));
        assert!(
            !GroupId::parse("acme")
                .unwrap()
                .starts_with(&GroupId::parse("x/acme").unwrap())
        );
    }

    #[test]
    fn display_bytes_escapes_unprintable() {
        assert_eq!(display_bytes(b"ok"), "ok");
        assert_eq!(display_bytes(b"a\nb\xff"), "a\\nb\\xff");
    }
}
