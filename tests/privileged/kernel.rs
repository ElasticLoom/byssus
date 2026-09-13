//! Kernel-layer behavior: bind mounts, attributes, identity, unmounting and
//! propagation.

use std::fs;
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::fs::PermissionsExt;

use byssus::config::MountAttrs;
use byssus::fsops;
use byssus::mount::{self, VerifiedOpError};
use byssus::mountinfo::MountTable;
use byssus::probe::{self, PropagationCheck};
use byssus::reconcile::plan::TargetState;
use rustix::fs::{Mode, OFlags};
use rustix::mount::{MountFlags, MountPropagationFlags, UnmountFlags};

use crate::common::{Sandbox, errno_of};

const DEFAULT: MountAttrs = MountAttrs {
    read_only: true,
    noexec: true,
    nosymfollow: false,
};

fn unique() -> bool {
    probe::probe_kernel().unique_mount_ids()
}

/// Creates `src/<name>/workspace` with a file and returns roots plus the
/// member's target descriptor.
struct Member {
    source_root: std::os::fd::OwnedFd,
    target_root: std::os::fd::OwnedFd,
}

fn setup(sb: &Sandbox, name: &str) -> Member {
    sb.write(&format!("src/{name}/workspace/hello.txt"), "hello");
    sb.mkdir("view");
    Member {
        source_root: sb.open_root("src"),
        target_root: sb.open_root("view"),
    }
}

fn mount_member(m: &Member, name: &str, attrs: MountAttrs) -> byssus::identity::MountIdentity {
    let source = fsops::resolve_dir(m.source_root.as_fd(), &format!("{name}/workspace")).unwrap();
    let target = fsops::ensure_dirs_beneath(m.target_root.as_fd(), &[name.to_owned()]).unwrap();
    mount::create_bind(source.as_fd(), target.as_fd(), &attrs, unique()).unwrap()
}

#[test]
#[ignore = "requires mount privileges; run scripts/integration-tests.sh"]
fn bind_mount_is_readonly_and_identified() {
    let sb = Sandbox::new();
    let m = setup(&sb, "libcurl");
    let id = mount_member(&m, "libcurl", DEFAULT);

    assert_eq!(
        fs::read_to_string(sb.path("view/libcurl/hello.txt")).unwrap(),
        "hello"
    );
    let err = fs::write(sb.path("view/libcurl/new.txt"), "x").unwrap_err();
    assert_eq!(errno_of(&err), libc::EROFS);
    let err = fs::write(sb.path("view/libcurl/hello.txt"), "x").unwrap_err();
    assert_eq!(errno_of(&err), libc::EROFS);

    // Source is untouched and still writable.
    fs::write(sb.path("src/libcurl/workspace/new.txt"), "live").unwrap();
    assert_eq!(
        fs::read_to_string(sb.path("view/libcurl/new.txt")).unwrap(),
        "live"
    );

    // Identity: root device/inode equal the source directory's.
    let source = fsops::resolve_dir(m.source_root.as_fd(), "libcurl/workspace").unwrap();
    assert_eq!(id.root, fsops::dev_ino(source.as_fd()).unwrap());
    match mount::inspect(m.target_root.as_fd(), "libcurl", unique()) {
        TargetState::Mounted { identity, attrs } => {
            assert!(identity.matches(&id));
            let attrs = attrs.unwrap();
            assert!(attrs.read_only && attrs.nosuid && attrs.nodev && attrs.noexec);
            assert!(!attrs.nosymfollow);
        }
        other => panic!("unexpected {other:?}"),
    }
    if unique() {
        assert!(id.mnt_id_unique.is_some());
    }
}

#[test]
#[ignore = "requires mount privileges; run scripts/integration-tests.sh"]
fn noexec_is_enforced() {
    let sb = Sandbox::new();
    let script = sb.write("src/tool/workspace/run.sh", "#!/bin/sh\nexit 0\n");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let m = setup(&sb, "tool");
    mount_member(&m, "tool", DEFAULT);
    let err = std::process::Command::new(sb.path("view/tool/run.sh"))
        .status()
        .unwrap_err();
    assert_eq!(errno_of(&err), libc::EACCES);
}

#[test]
#[ignore = "requires mount privileges; run scripts/integration-tests.sh"]
fn nosymfollow_is_applied_when_configured() {
    let sb = Sandbox::new();
    sb.write("src/s/workspace/target.txt", "t");
    std::os::unix::fs::symlink("target.txt", sb.path("src/s/workspace/link")).unwrap();
    let m = setup(&sb, "s");
    let attrs = MountAttrs {
        nosymfollow: true,
        ..DEFAULT
    };
    mount_member(&m, "s", attrs);
    let err = fs::read_to_string(sb.path("view/s/link")).unwrap_err();
    assert_eq!(errno_of(&err), libc::ELOOP);
    match mount::inspect(m.target_root.as_fd(), "s", unique()) {
        TargetState::Mounted { attrs, .. } => assert!(attrs.unwrap().nosymfollow),
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
#[ignore = "requires mount privileges; run scripts/integration-tests.sh"]
fn submounts_inside_source_are_not_exposed() {
    let sb = Sandbox::new();
    sb.write("src/m/workspace/visible.txt", "v");
    let secret_dir = sb.mkdir("src/m/workspace/secret");
    rustix::mount::mount("secret", &secret_dir, "tmpfs", MountFlags::empty(), None).unwrap();
    fs::write(secret_dir.join("sensitive.txt"), "s").unwrap();
    assert!(sb.path("src/m/workspace/secret/sensitive.txt").exists());

    let m = setup(&sb, "m");
    mount_member(&m, "m", DEFAULT);

    assert!(sb.path("view/m/visible.txt").exists());
    assert!(sb.path("view/m/secret").is_dir());
    assert!(
        !sb.path("view/m/secret/sensitive.txt").exists(),
        "a submount inside the source leaked into the view"
    );
    assert_eq!(fs::read_dir(sb.path("view/m/secret")).unwrap().count(), 0);
}

#[test]
#[ignore = "requires mount privileges; run scripts/integration-tests.sh"]
fn verified_unmount_removes_only_our_mount() {
    let sb = Sandbox::new();
    let m = setup(&sb, "a");
    let id = mount_member(&m, "a", DEFAULT);

    mount::unmount_verified(m.target_root.as_fd(), "a", &id, unique()).unwrap();
    assert_eq!(
        mount::inspect(m.target_root.as_fd(), "a", unique()),
        TargetState::NotMounted
    );
    assert!(!sb.path("view/a/hello.txt").exists());
    assert!(fsops::remove_empty_dir(m.target_root.as_fd(), None, "a").unwrap());

    // Nothing mounted any more.
    fs::create_dir(sb.path("view/a")).unwrap();
    assert!(matches!(
        mount::unmount_verified(m.target_root.as_fd(), "a", &id, unique()),
        Err(VerifiedOpError::NotMounted)
    ));
}

#[test]
#[ignore = "requires mount privileges; run scripts/integration-tests.sh"]
fn replaced_mount_is_detected_and_left_alone() {
    let sb = Sandbox::new();
    sb.write("src/other/workspace/foreign.txt", "foreign");
    let m = setup(&sb, "a");
    let id = mount_member(&m, "a", DEFAULT);

    // Someone unmounts ours and mounts something else at the same target.
    rustix::mount::unmount(sb.path("view/a").as_path(), UnmountFlags::DETACH).unwrap();
    rustix::mount::mount_bind(
        sb.path("src/other/workspace").as_path(),
        sb.path("view/a").as_path(),
    )
    .unwrap();

    match mount::unmount_verified(m.target_root.as_fd(), "a", &id, unique()) {
        Err(VerifiedOpError::IdentityMismatch { .. }) => {}
        other => panic!("expected identity mismatch, got {other:?}"),
    }
    assert!(
        sb.path("view/a/foreign.txt").exists(),
        "foreign mount was touched"
    );
}

#[test]
#[ignore = "requires mount privileges; run scripts/integration-tests.sh"]
fn stacked_mount_on_top_is_a_mismatch() {
    let sb = Sandbox::new();
    sb.write("src/other/workspace/top.txt", "top");
    let m = setup(&sb, "a");
    let id = mount_member(&m, "a", DEFAULT);
    rustix::mount::mount_bind(
        sb.path("src/other/workspace").as_path(),
        sb.path("view/a").as_path(),
    )
    .unwrap();
    assert!(matches!(
        mount::unmount_verified(m.target_root.as_fd(), "a", &id, unique()),
        Err(VerifiedOpError::IdentityMismatch { .. })
    ));
    assert!(sb.path("view/a/top.txt").exists());
}

#[test]
#[ignore = "requires mount privileges; run scripts/integration-tests.sh"]
fn fd_link_unmount_detaches_a_mount_stacked_after_pinning() {
    // Documents the kernel behavior noted in unmount_verified: opening
    // /proc/self/fd/N reaches the pinned mount, but umount2 on it detaches a
    // mount stacked on top after pinning.
    let sb = Sandbox::new();
    sb.write("src/other/workspace/top.txt", "top");
    let m = setup(&sb, "a");
    let id = mount_member(&m, "a", DEFAULT);
    let pinned = fsops::resolve_dir(m.target_root.as_fd(), "a").unwrap();
    rustix::mount::mount_bind(
        sb.path("src/other/workspace").as_path(),
        sb.path("view/a").as_path(),
    )
    .unwrap();
    let link = format!("/proc/self/fd/{}", pinned.as_raw_fd());

    let via_link =
        rustix::fs::open(link.as_str(), OFlags::PATH | OFlags::CLOEXEC, Mode::empty()).unwrap();
    assert!(
        mount::read_identity(via_link.as_fd(), unique())
            .unwrap()
            .matches(&id)
    );
    rustix::mount::unmount(link.as_str(), UnmountFlags::DETACH).unwrap();
    assert!(
        mount::read_identity(pinned.as_fd(), unique())
            .unwrap()
            .matches(&id)
    );
    assert_eq!(
        mount::inspect(m.target_root.as_fd(), "a", unique()),
        TargetState::Mounted {
            identity: mount::read_identity(pinned.as_fd(), unique()).unwrap(),
            attrs: mount::observed_attrs(pinned.as_fd()).ok(),
        }
    );
    assert!(!sb.path("view/a/top.txt").exists());
}

#[test]
#[ignore = "requires mount privileges; run scripts/integration-tests.sh"]
fn verified_unmount_refuses_a_replaced_proc_fd_directory() {
    let code = crate::privileges::in_child_with(|| {
        // SAFETY: the child is single-threaded; a private mount namespace
        // keeps the overmount from outliving it.
        assert_eq!(unsafe { libc::unshare(libc::CLONE_NEWNS) }, 0, "unshare");
        rustix::mount::mount_change(
            "/",
            MountPropagationFlags::PRIVATE | MountPropagationFlags::REC,
        )
        .unwrap();
        let sb = Sandbox::new();
        sb.write("src/decoy/workspace/decoy.txt", "decoy");
        let m = setup(&sb, "a");
        let id = mount_member(&m, "a", DEFAULT);
        mount_member(&m, "decoy", DEFAULT);

        // Every descriptor number leads to the decoy mount.
        let fake = sb.mkdir("fake-fd");
        for n in 0..256 {
            std::os::unix::fs::symlink(sb.path("view/decoy"), fake.join(n.to_string())).unwrap();
        }
        rustix::mount::mount_bind(fake.as_path(), "/proc/self/fd").unwrap();

        let result = mount::unmount_verified(m.target_root.as_fd(), "a", &id, unique());
        let untouched =
            sb.path("view/a/hello.txt").exists() && sb.path("view/decoy/decoy.txt").exists();
        match result {
            Err(VerifiedOpError::Io(_)) if untouched => 0,
            other => {
                eprintln!("unexpected: {other:?}, mounts untouched: {untouched}");
                1
            }
        }
    });
    assert_eq!(code, 0);
}

#[test]
#[ignore = "requires mount privileges; run scripts/integration-tests.sh"]
fn attribute_changes_do_not_reach_propagated_copies() {
    // Documents the kernel behavior that makes Byssus re-create mounts rather
    // than change attributes in place: mount_setattr on the host mount leaves
    // copies already propagated into consumers unchanged.
    let sb = Sandbox::new();
    sb.write("src/a/workspace/f", "f");
    let view = sb.make_shared_anchor("view");
    let consumer = sb.attach_consumer(&view, "container/group");
    let source_root = sb.open_root("src");
    let target_root = sb.open_root("view");
    let loose = MountAttrs {
        read_only: false,
        noexec: false,
        nosymfollow: false,
    };
    let source = fsops::resolve_dir(source_root.as_fd(), "a/workspace").unwrap();
    let target = fsops::ensure_dirs_beneath(target_root.as_fd(), &["a".into()]).unwrap();
    let id = mount::create_bind(source.as_fd(), target.as_fd(), &loose, unique()).unwrap();
    fs::write(consumer.join("a/before"), "w").unwrap();

    let mounted = fsops::resolve_dir(target_root.as_fd(), "a").unwrap();
    byssus::sys::mount_setattr_add(mounted.as_fd(), byssus::sys::MOUNT_ATTR_RDONLY).unwrap();
    assert_eq!(
        errno_of(&fs::write(view.join("a/host"), "w").unwrap_err()),
        libc::EROFS
    );
    assert!(
        fs::write(consumer.join("a/consumer"), "w").is_ok(),
        "kernel behavior changed: attributes now propagate"
    );

    // Re-creating the mount does reach the consumer.
    drop((target, mounted));
    mount::unmount_verified(target_root.as_fd(), "a", &id, unique()).unwrap();
    assert!(!consumer.join("a/f").exists());
    let target = fsops::ensure_dirs_beneath(target_root.as_fd(), &["a".into()]).unwrap();
    mount::create_bind(source.as_fd(), target.as_fd(), &DEFAULT, unique()).unwrap();
    assert_eq!(
        errno_of(&fs::write(consumer.join("a/after"), "w").unwrap_err()),
        libc::EROFS
    );
}

#[test]
#[ignore = "requires mount privileges; run scripts/integration-tests.sh"]
fn read_only_bind_of_view_does_not_restrict_members() {
    // A consumer binding the view read-only still gets writable member mounts
    // if the group is read-write: read-only applies per mount, not to mounts
    // beneath it. Only the group's read_only setting restricts members.
    let sb = Sandbox::new();
    sb.write("src/a/workspace/f", "f");
    let view = sb.make_shared_anchor("view");
    let consumer = sb.attach_consumer(&view, "container/group");
    rustix::mount::mount_remount(
        consumer.as_path(),
        MountFlags::BIND | MountFlags::RDONLY,
        "",
    )
    .unwrap();
    let source_root = sb.open_root("src");
    let target_root = sb.open_root("view");
    let source = fsops::resolve_dir(source_root.as_fd(), "a/workspace").unwrap();
    let target = fsops::ensure_dirs_beneath(target_root.as_fd(), &["a".into()]).unwrap();
    let loose = MountAttrs {
        read_only: false,
        noexec: false,
        nosymfollow: false,
    };
    mount::create_bind(source.as_fd(), target.as_fd(), &loose, unique()).unwrap();
    assert_eq!(
        errno_of(&fs::create_dir(consumer.join("new-dir")).unwrap_err()),
        libc::EROFS,
        "the view directory itself is read-only"
    );
    assert!(fs::write(consumer.join("a/written"), "w").is_ok());
}

#[test]
#[ignore = "requires mount privileges; run scripts/integration-tests.sh"]
fn mounts_propagate_to_slave_consumers() {
    let sb = Sandbox::new();
    sb.write("src/libcurl/workspace/hello.txt", "hello");
    let view = sb.make_shared_anchor("view");
    let consumer = sb.attach_consumer(&view, "container/group");
    let source_root = sb.open_root("src");
    let target_root = sb.open_root("view");

    let source = fsops::resolve_dir(source_root.as_fd(), "libcurl/workspace").unwrap();
    let target = fsops::ensure_dirs_beneath(target_root.as_fd(), &["libcurl".into()]).unwrap();
    let id = mount::create_bind(source.as_fd(), target.as_fd(), &DEFAULT, unique()).unwrap();

    assert_eq!(
        fs::read_to_string(consumer.join("libcurl/hello.txt")).unwrap(),
        "hello"
    );
    let err = fs::write(consumer.join("libcurl/x"), "x").unwrap_err();
    assert_eq!(errno_of(&err), libc::EROFS);

    mount::unmount_verified(target_root.as_fd(), "libcurl", &id, unique()).unwrap();
    assert!(!consumer.join("libcurl/hello.txt").exists());
}

#[test]
#[ignore = "requires mount privileges; run scripts/integration-tests.sh"]
fn propagation_check_classifies_mounts() {
    let sb = Sandbox::new();
    let shared = sb.make_shared_anchor("shared");
    let consumer = sb.attach_consumer(&shared, "slave");
    let private = sb.mkdir("private");
    rustix::mount::mount_bind(&private, &private).unwrap();
    rustix::mount::mount_change(&private, rustix::mount::MountPropagationFlags::PRIVATE).unwrap();

    let table = MountTable::read_self().unwrap();
    let check = |path: &std::path::Path| {
        let fd = fsops::open_root(path).unwrap();
        probe::check_propagation(fd.as_fd(), &table)
    };
    assert_eq!(check(&shared), PropagationCheck::Shared);
    assert_eq!(check(&private), PropagationCheck::Private);
    assert_eq!(check(&consumer), PropagationCheck::SlaveOnly);
    // A slave that is also shared (as in a service manager's private mount
    // namespace) is distinguished too.
    rustix::mount::mount_change(&consumer, rustix::mount::MountPropagationFlags::SHARED).unwrap();
    let table = MountTable::read_self().unwrap();
    let fd = fsops::open_root(&consumer).unwrap();
    assert_eq!(
        probe::check_propagation(fd.as_fd(), &table),
        PropagationCheck::SharedAndSlave
    );
    // A plain directory reports the mount containing it.
    let inner = sb.mkdir("shared/inner");
    assert_eq!(check(&inner), PropagationCheck::Shared);
}

#[test]
#[ignore = "requires mount privileges; run scripts/integration-tests.sh"]
fn nested_target_directories_are_created() {
    let sb = Sandbox::new();
    sb.write("src/n/workspace/f", "f");
    sb.mkdir("view");
    let source_root = sb.open_root("src");
    let target_root = sb.open_root("view");
    let source = fsops::resolve_dir(source_root.as_fd(), "n/workspace").unwrap();
    let target =
        fsops::ensure_dirs_beneath(target_root.as_fd(), &["n".into(), "ro".into()]).unwrap();
    mount::create_bind(source.as_fd(), target.as_fd(), &DEFAULT, unique()).unwrap();
    assert!(sb.path("view/n/ro/f").exists());
    assert!(matches!(
        mount::inspect(target_root.as_fd(), "n/ro", unique()),
        TargetState::Mounted { .. }
    ));
    assert_eq!(
        mount::inspect(target_root.as_fd(), "n", unique()),
        TargetState::NotMounted
    );
}

#[test]
#[ignore = "requires mount privileges; run scripts/integration-tests.sh"]
fn symlinked_target_is_refused() {
    let sb = Sandbox::new();
    let m = setup(&sb, "a");
    let elsewhere = sb.mkdir("elsewhere");
    std::os::unix::fs::symlink(&elsewhere, sb.path("view/a")).unwrap();
    let err = fsops::ensure_dirs_beneath(m.target_root.as_fd(), &["a".into()]).unwrap_err();
    assert_eq!(errno_of(&err), libc::ELOOP);
    assert!(matches!(
        mount::inspect(m.target_root.as_fd(), "a", unique()),
        TargetState::Unavailable(_)
    ));
}

#[test]
#[ignore = "requires mount privileges; run scripts/integration-tests.sh"]
fn remount_of_same_source_is_a_mismatch() {
    let sb = Sandbox::new();
    let m = setup(&sb, "a");
    let id = mount_member(&m, "a", DEFAULT);
    // Same source directory, same target, but a different mount.
    rustix::mount::unmount(sb.path("view/a").as_path(), UnmountFlags::DETACH).unwrap();
    rustix::mount::mount_bind(
        sb.path("src/a/workspace").as_path(),
        sb.path("view/a").as_path(),
    )
    .unwrap();
    match mount::inspect(m.target_root.as_fd(), "a", unique()) {
        TargetState::Mounted { identity, .. } => {
            assert_eq!(identity.root, id.root, "same source directory");
            assert!(!identity.matches(&id), "a new mount must not match");
        }
        other => panic!("unexpected {other:?}"),
    }
    assert!(matches!(
        mount::unmount_verified(m.target_root.as_fd(), "a", &id, unique()),
        Err(VerifiedOpError::IdentityMismatch { .. })
    ));
}
