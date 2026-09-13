//! Service user resolution from `/etc/passwd` and `/etc/group`.
//!
//! Static musl binaries have no NSS, so the files are parsed directly. Only
//! local users are supported for the service account.

use std::io;
use std::path::Path;

use crate::privileges::plan::ServiceUser;

/// Why a user could not be resolved.
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    /// No such user in the password file.
    #[error("user '{0}' not found in {1}")]
    NotFound(String, String),
    /// A file could not be read.
    #[error("cannot read {path}: {source}")]
    Io {
        /// File path.
        path: String,
        /// Underlying error.
        source: io::Error,
    },
}

/// Resolves `name` using the system password and group files.
pub fn resolve(name: &str) -> Result<ServiceUser, ResolveError> {
    resolve_from(name, Path::new("/etc/passwd"), Path::new("/etc/group"))
}

/// Resolves `name` using the given password and group files.
pub fn resolve_from(name: &str, passwd: &Path, group: &Path) -> Result<ServiceUser, ResolveError> {
    let read = |path: &Path| {
        std::fs::read_to_string(path).map_err(|source| ResolveError::Io {
            path: path.display().to_string(),
            source,
        })
    };
    let passwd_text = read(passwd)?;
    let (uid, gid) = find_user(&passwd_text, name)
        .ok_or_else(|| ResolveError::NotFound(name.to_owned(), passwd.display().to_string()))?;
    let group_text = read(group)?;
    let mut groups = supplementary_groups(&group_text, name);
    groups.retain(|&g| g != gid);
    Ok(ServiceUser {
        name: name.to_owned(),
        uid,
        gid,
        groups,
    })
}

/// Finds `(uid, gid)` for `name` in passwd-format text.
#[must_use]
pub fn find_user(passwd: &str, name: &str) -> Option<(u32, u32)> {
    passwd.lines().find_map(|line| {
        let mut fields = line.split(':');
        if fields.next()? != name {
            return None;
        }
        let _password = fields.next()?;
        let uid = fields.next()?.parse().ok()?;
        let gid = fields.next()?.parse().ok()?;
        Some((uid, gid))
    })
}

/// Group IDs whose member list (fourth field) includes `name`, sorted and
/// deduplicated.
#[must_use]
pub fn supplementary_groups(group: &str, name: &str) -> Vec<u32> {
    let mut gids: Vec<u32> = group
        .lines()
        .filter_map(|line| {
            let mut fields = line.split(':');
            let _group_name = fields.next()?;
            let _password = fields.next()?;
            let gid = fields.next()?.parse().ok()?;
            let members = fields.next()?;
            members.split(',').any(|m| m == name).then_some(gid)
        })
        .collect();
    gids.sort_unstable();
    gids.dedup();
    gids
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASSWD: &str = "\
root:x:0:0:root:/root:/bin/bash
# comment lines are not valid entries
byssus:x:991:991::/nonexistent:/usr/sbin/nologin
byssus2:x:992:992::/nonexistent:/usr/sbin/nologin
broken:x:notanumber:1::/:/bin/false
";

    const GROUP: &str = "\
root:x:0:
byssus:x:991:
acl-readers:x:500:alice,byssus
other:x:501:byssus2
dupe:x:500:byssus
";

    #[test]
    fn finds_users() {
        assert_eq!(find_user(PASSWD, "byssus"), Some((991, 991)));
        assert_eq!(find_user(PASSWD, "root"), Some((0, 0)));
        assert_eq!(find_user(PASSWD, "byssu"), None);
        assert_eq!(find_user(PASSWD, "broken"), None);
        assert_eq!(find_user(PASSWD, "missing"), None);
    }

    #[test]
    fn supplementary_membership_is_exact() {
        assert_eq!(supplementary_groups(GROUP, "byssus"), [500]);
        assert_eq!(supplementary_groups(GROUP, "byssus2"), [501]);
        assert!(supplementary_groups(GROUP, "nobody").is_empty());
    }

    #[test]
    fn resolve_from_files() {
        let dir = tempfile::tempdir().unwrap();
        let passwd = dir.path().join("passwd");
        let group = dir.path().join("group");
        std::fs::write(&passwd, PASSWD).unwrap();
        std::fs::write(&group, GROUP).unwrap();
        let user = resolve_from("byssus", &passwd, &group).unwrap();
        assert_eq!(
            user,
            ServiceUser {
                name: "byssus".into(),
                uid: 991,
                gid: 991,
                groups: vec![500],
            }
        );
        assert!(matches!(
            resolve_from("ghost", &passwd, &group),
            Err(ResolveError::NotFound(..))
        ));
        assert!(matches!(
            resolve_from("byssus", &dir.path().join("none"), &group),
            Err(ResolveError::Io { .. })
        ));
    }
}
