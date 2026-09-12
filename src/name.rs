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
    fn display_bytes_escapes_unprintable() {
        assert_eq!(display_bytes(b"ok"), "ok");
        assert_eq!(display_bytes(b"a\nb\xff"), "a\\nb\\xff");
    }
}
