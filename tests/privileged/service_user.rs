//! Switching to an unprivileged service user. Requires subordinate UIDs mapped
//! into the test namespace (`BYSSUS_TEST_SUBIDS=1`).

use std::fs;
use std::os::unix::fs::{PermissionsExt, chown};
use std::path::PathBuf;
use std::time::Duration;

use byssus::privileges::apply::{self, ProcStatus};
use byssus::privileges::plan::{self, Goal, Request};
use rustix::mount::{MountFlags, UnmountFlags};
use rustix::process::Signal;
use rustix::thread::CapabilitySet;

use crate::cli::{Deployment, assert_success, bin, text};

const SERVICE_UID: u32 = 991;
const SERVICE_GID: u32 = 991;

fn require_subids() {
    crate::common::require_test_namespace();
    assert_eq!(
        std::env::var("BYSSUS_TEST_SUBIDS").as_deref(),
        Ok("1"),
        "service user tests need subordinate UIDs mapped by scripts/integration-tests.sh"
    );
}

/// Overlays /etc/passwd and /etc/group with copies that also define the
/// `byssus` service user, for the lifetime of the value.
struct FakeUser {
    _dir: tempfile::TempDir,
    targets: Vec<PathBuf>,
}

impl FakeUser {
    fn install() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut targets = Vec::new();
        for (file, line) in [
            (
                "/etc/passwd",
                format!("byssus:x:{SERVICE_UID}:{SERVICE_GID}::/nonexistent:/usr/sbin/nologin\n"),
            ),
            ("/etc/group", format!("byssus:x:{SERVICE_GID}:\n")),
        ] {
            let mut content = fs::read_to_string(file).unwrap_or_default();
            content.push_str(&line);
            let copy = dir.path().join(file.trim_start_matches("/etc/"));
            fs::write(&copy, content).unwrap();
            fs::set_permissions(&copy, fs::Permissions::from_mode(0o644)).unwrap();
            rustix::mount::mount_bind(&copy, file).expect("bind over passwd/group");
            targets.push(PathBuf::from(file));
        }
        Self { _dir: dir, targets }
    }
}

impl Drop for FakeUser {
    fn drop(&mut self) {
        for t in &self.targets {
            let _ = rustix::mount::unmount(t.as_path(), UnmountFlags::DETACH);
        }
    }
}

#[test]
#[ignore = "requires subordinate UIDs; run scripts/integration-tests.sh"]
fn root_switches_to_service_user_keeping_sys_admin() {
    require_subids();
    let _user = FakeUser::install();
    let scratch = tempfile::tempdir().unwrap();
    let mount_point = scratch.path().join("m");
    fs::create_dir(&mount_point).unwrap();
    chown(&mount_point, Some(SERVICE_UID), Some(SERVICE_GID)).unwrap();
    let mount_point_for_child = mount_point.clone();

    let code = crate::privileges::in_child_with(move || {
        let before = ProcStatus::read_self().unwrap();
        let user = byssus::users::resolve("byssus").unwrap();
        let request = Request {
            goal: Goal::KeepSysAdmin,
            user: Some(user),
            allow_root: false,
        };
        let p = plan::plan(&before.credentials(), &request).unwrap();
        let after = apply::apply(&p).unwrap();
        assert_eq!(after.uids, [SERVICE_UID; 4]);
        assert_eq!(after.gids, [SERVICE_GID; 4]);
        assert_eq!(after.cap_effective, CapabilitySet::SYS_ADMIN.bits());
        assert_eq!(after.cap_bounding, CapabilitySet::SYS_ADMIN.bits());
        // CAP_SYS_ADMIN still works after the switch.
        rustix::mount::mount(
            "t",
            &mount_point_for_child,
            "tmpfs",
            MountFlags::empty(),
            None,
        )
        .expect("mount as service user");
        rustix::mount::unmount(&mount_point_for_child, UnmountFlags::DETACH).unwrap();
        // Root's file permissions are gone.
        assert!(fs::read_to_string("/proc/1/environ").is_err());
        0
    });
    assert_eq!(code, 0);
}

fn prepare_for_service_user(d: &Deployment) {
    for dir in ["state", "view"] {
        chown(d.path(dir), Some(SERVICE_UID), Some(SERVICE_GID)).unwrap();
    }
}

#[test]
#[ignore = "requires subordinate UIDs; run scripts/integration-tests.sh"]
fn daemon_runs_as_service_user() {
    require_subids();
    let _user = FakeUser::install();
    let d = Deployment::new();
    let content = fs::read_to_string(d.path("etc/byssus.toml"))
        .unwrap()
        .replace("[daemon]\n", "[daemon]\nuser = \"byssus\"\n");
    fs::write(d.path("etc/byssus.toml"), content).unwrap();
    prepare_for_service_user(&d);
    d.add_source("a");
    d.join("a");

    // dry-run as root checks access as the service user.
    let out = d.byssus(&["dry-run"]);
    let report = text(&out.stdout);
    assert!(
        report.contains(&format!(
            "checks run as uid {SERVICE_UID} with no capabilities"
        )),
        "{report}"
    );

    let log_path = d.path("daemon-user.log");
    let mut child = std::process::Command::new(bin("byssusd"))
        .args(d.config_args())
        .stderr(fs::File::create(&log_path).unwrap())
        .spawn()
        .unwrap();
    let start = std::time::Instant::now();
    while !d.visible("a") {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "not mounted\n{}",
            fs::read_to_string(&log_path).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let status = fs::read_to_string(format!("/proc/{}/status", child.id())).unwrap();
    assert!(
        status
            .lines()
            .any(|l| l.starts_with("Uid:") && l.contains(&SERVICE_UID.to_string())),
        "{status}"
    );
    let log = fs::read_to_string(&log_path).unwrap();
    assert!(
        log.contains(&format!(
            "msg=\"privileges normalized\" uid={SERVICE_UID} caps=cap_sys_admin"
        )),
        "{log}"
    );

    let pid = rustix::process::Pid::from_raw(i32::try_from(child.id()).unwrap()).unwrap();
    rustix::process::kill_process(pid, Signal::TERM).unwrap();
    assert!(child.wait().unwrap().success());
    let meta = fs::metadata(d.path("state/state.json")).unwrap();
    assert_eq!(std::os::unix::fs::MetadataExt::uid(&meta), SERVICE_UID);
    assert_eq!(meta.permissions().mode() & 0o777, 0o640);
}

#[test]
#[ignore = "requires subordinate UIDs; run scripts/integration-tests.sh"]
fn service_user_without_access_reports_permission_problems() {
    require_subids();
    let _user = FakeUser::install();
    let d = Deployment::new();
    prepare_for_service_user(&d);
    d.add_source("locked");
    d.join("locked");
    // Source tree not searchable by the service user.
    fs::set_permissions(d.path("src/locked"), fs::Permissions::from_mode(0o700)).unwrap();

    let out = d.byssus(&["reconcile", "--user", "byssus"]);
    assert_success(&out);
    let log = text(&out.stderr);
    assert!(log.contains("op=skip group=g name=locked"), "{log}");
    assert!(log.contains("Permission denied"), "{log}");
    assert!(!d.visible("locked"));
}

#[test]
#[ignore = "requires subordinate UIDs; run scripts/integration-tests.sh"]
fn check_detects_membership_directory_unreadable_by_service_user() {
    require_subids();
    let _user = FakeUser::install();
    let d = Deployment::new();
    let content = fs::read_to_string(d.path("etc/byssus.toml"))
        .unwrap()
        .replace("[daemon]\n", "[daemon]\nuser = \"byssus\"\n");
    fs::write(d.path("etc/byssus.toml"), content).unwrap();
    prepare_for_service_user(&d);
    assert_success(&d.byssus(&["check"]));

    // Root can read it; the service user cannot.
    fs::set_permissions(d.path("members"), fs::Permissions::from_mode(0o700)).unwrap();
    let out = d.byssus(&["check"]);
    assert!(!out.status.success(), "{}", text(&out.stdout));
    let report = text(&out.stdout);
    assert!(report.contains("cannot open membership"), "{report}");
    assert!(report.contains("Permission denied"), "{report}");
}
