//! End-to-end tests of the `byssus` CLI performing real mounts.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use byssus::state::State;

use crate::common::Sandbox;

pub fn bin(name: &str) -> PathBuf {
    let dir = std::env::var_os("BYSSUS_TEST_BIN_DIR").expect("BYSSUS_TEST_BIN_DIR");
    PathBuf::from(dir).join(name)
}

/// A sandbox laid out like a deployment, with configuration.
pub struct Deployment {
    pub sb: Sandbox,
}

impl Deployment {
    pub fn new() -> Self {
        let sb = Sandbox::new();
        for dir in ["etc/conf.d", "src", "view", "members", "state"] {
            sb.mkdir(dir);
        }
        let d = Self { sb };
        d.write_config("");
        d
    }

    pub fn path(&self, rel: &str) -> PathBuf {
        self.sb.path(rel)
    }

    /// Writes the main config with one group `g`; `extra` is appended to the
    /// group table.
    pub fn write_config(&self, extra: &str) {
        let p = |s: &str| self.path(s).display().to_string();
        let text = format!(
            "[daemon]\nstate_dir = \"{}\"\nresync_interval_secs = 1\n\n[groups.g]\nsource_root = \"{}\"\nsource = \"{{name}}/workspace\"\ntarget_root = \"{}\"\ntarget = \"{{name}}\"\nmembership = \"{}\"\n{extra}",
            p("state"),
            p("src"),
            p("view"),
            p("members"),
        );
        let file = self.path("etc/byssus.toml");
        fs::write(&file, text).unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        for dir in ["etc", "etc/conf.d"] {
            fs::set_permissions(self.path(dir), fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    /// Replaces the main configuration with `text` (root-owned mode 0644).
    pub fn write_main_config(&self, text: &str) {
        let file = self.path("etc/byssus.toml");
        fs::write(&file, text).unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
    }

    pub fn add_source(&self, name: &str) {
        self.sb.write(
            &format!("src/{name}/workspace/README"),
            &format!("I am {name}"),
        );
    }

    pub fn join(&self, name: &str) {
        fs::write(self.path(&format!("members/{name}")), "").unwrap();
    }

    pub fn leave(&self, name: &str) {
        fs::remove_file(self.path(&format!("members/{name}"))).unwrap();
    }

    pub fn config_args(&self) -> Vec<String> {
        vec![
            "--config".into(),
            self.path("etc/byssus.toml").display().to_string(),
            "--config-dir".into(),
            self.path("etc/conf.d").display().to_string(),
        ]
    }

    pub fn byssus(&self, args: &[&str]) -> Output {
        let mut cmd = Command::new(bin("byssus"));
        cmd.args(self.config_args()).args(args);
        cmd.output().expect("run byssus")
    }

    pub fn reconcile(&self) -> Output {
        self.byssus(&["reconcile", "--allow-root"])
    }

    pub fn state(&self) -> State {
        match fs::read(self.path("state/state.json")) {
            Ok(bytes) => State::decode(&bytes).unwrap(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::default(),
            Err(e) => panic!("cannot read state: {e}"),
        }
    }

    pub fn visible(&self, name: &str) -> bool {
        self.path(&format!("view/{name}/README")).exists()
    }
}

pub fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

pub fn assert_success(out: &Output) {
    assert!(
        out.status.success(),
        "exit {:?}\nstdout:\n{}\nstderr:\n{}",
        out.status,
        text(&out.stdout),
        text(&out.stderr)
    );
}

fn is_mount_point(path: &Path) -> bool {
    let fd = byssus::fsops::open_root(path).unwrap();
    byssus::mount::is_mount_root(std::os::fd::AsFd::as_fd(&fd)).unwrap()
}

#[test]
#[ignore = "requires mount privileges; run scripts/integration-tests.sh"]
fn reconcile_end_to_end() {
    let d = Deployment::new();
    d.add_source("libcurl");
    d.add_source("openssl");
    d.join("libcurl");
    d.join("openssl");

    let out = d.reconcile();
    assert_success(&out);
    let log = text(&out.stderr);
    assert!(log.contains("op=mount group=g name=libcurl"), "{log}");
    assert!(log.contains("result=ok"));
    assert!(log.contains("msg=\"privileges normalized\""), "{log}");
    assert!(d.visible("libcurl") && d.visible("openssl"));
    assert_eq!(d.state().len(), 2);
    let meta = fs::metadata(d.path("state/state.json")).unwrap();
    assert_eq!(meta.permissions().mode() & 0o777, 0o640);

    // Second run is a no-op.
    let out = d.reconcile();
    assert_success(&out);
    assert!(
        text(&out.stderr).contains("steps=0"),
        "{}",
        text(&out.stderr)
    );

    // Leaving unmounts and removes the record and target directory.
    d.leave("openssl");
    let out = d.reconcile();
    assert_success(&out);
    assert!(text(&out.stderr).contains("op=unmount group=g name=openssl"));
    assert!(!d.path("view/openssl").exists());
    assert!(d.visible("libcurl"));
    assert_eq!(d.state().len(), 1);
}

#[test]
#[ignore = "requires mount privileges; run scripts/integration-tests.sh"]
fn invalid_membership_entries_are_rejected() {
    let d = Deployment::new();
    for name in ["good", "full", "linked", "dir"] {
        d.add_source(name);
    }
    d.add_source(".hidden");
    d.join("good");
    fs::write(d.path("members/full"), "data").unwrap();
    std::os::unix::fs::symlink(d.path("members/good"), d.path("members/linked")).unwrap();
    fs::create_dir(d.path("members/dir")).unwrap();
    fs::write(d.path("members/.hidden"), "").unwrap();
    fs::write(d.path("members/.gitkeep"), "").unwrap();

    let out = d.reconcile();
    assert_success(&out);
    let log = text(&out.stderr);
    for name in ["full", "linked", "dir"] {
        assert!(
            log.contains(&format!("op=reject group=g name={name}")),
            "{name}: {log}"
        );
        assert!(
            !d.path(&format!("view/{name}")).exists(),
            "{name} was mounted"
        );
    }
    // Hidden entries are ignored without a warning.
    assert!(!log.contains("name=.hidden"), "{log}");
    assert!(!log.contains("name=.gitkeep"), "{log}");
    assert!(!d.path("view/.hidden").exists());
    assert!(d.visible("good"));
    assert_eq!(d.state().len(), 1);

    // status lists rejected entries (a warning, so exit 0) and ignored counts.
    let out = d.byssus(&["status"]);
    assert_success(&out);
    let status = text(&out.stdout);
    assert!(
        status.contains("ignored: group 'g': 2 hidden entries"),
        "{status}"
    );
    assert!(
        status.contains("g/linked  state=rejected  detail=\"not a regular file (symbolic link)\""),
        "{status}"
    );
    assert!(status.contains("g/full  state=rejected"), "{status}");

    let out = d.byssus(&["dry-run", "--format", "json"]);
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json["groups"][0]["rejected"], 3);
    assert_eq!(json["groups"][0]["ignored"], 2);
}

#[test]
#[ignore = "requires mount privileges; run scripts/integration-tests.sh"]
fn foreign_mount_is_reported_and_untouched() {
    let d = Deployment::new();
    d.add_source("a");
    d.sb.write("foreign/marker", "foreign");
    fs::create_dir(d.path("view/a")).unwrap();
    rustix::mount::mount_bind(d.path("foreign").as_path(), d.path("view/a").as_path()).unwrap();
    d.join("a");

    let out = d.reconcile();
    assert_success(&out);
    let log = text(&out.stderr);
    assert!(log.contains("op=conflict group=g name=a"), "{log}");
    assert!(d.path("view/a/marker").exists());
    assert!(!d.visible("a"));
    assert!(d.state().is_empty());

    // Removing membership does not unmount a mount we never recorded.
    d.leave("a");
    assert_success(&d.reconcile());
    assert!(d.path("view/a/marker").exists());
}

#[test]
#[ignore = "requires mount privileges; run scripts/integration-tests.sh"]
fn source_removed_then_recreated() {
    let d = Deployment::new();
    d.add_source("a");
    d.join("a");
    assert_success(&d.reconcile());
    assert!(d.visible("a"));

    fs::remove_dir_all(d.path("src/a")).unwrap();
    let out = d.reconcile();
    assert_success(&out);
    assert!(text(&out.stderr).contains("op=unmount group=g name=a"));
    assert!(!is_mount_point(&d.path("view")) || !d.path("view/a").exists());
    assert!(d.state().is_empty());

    d.add_source("a");
    let out = d.reconcile();
    assert_success(&out);
    assert!(d.visible("a"));
    assert_eq!(d.state().len(), 1);
}

#[test]
#[ignore = "requires mount privileges; run scripts/integration-tests.sh"]
fn missing_source_is_skipped_then_mounted_when_it_appears() {
    let d = Deployment::new();
    d.join("later");
    let out = d.reconcile();
    assert_success(&out);
    assert!(text(&out.stderr).contains("op=skip group=g name=later"));
    assert!(d.state().is_empty());

    d.add_source("later");
    assert_success(&d.reconcile());
    assert!(d.visible("later"));
}

#[test]
#[ignore = "requires mount privileges; run scripts/integration-tests.sh"]
fn configuration_change_moves_and_restricts_mounts() {
    let d = Deployment::new();
    d.add_source("a");
    d.join("a");
    d.write_config("read_only = false\nnoexec = false\n");
    assert_success(&d.reconcile());
    fs::write(d.path("view/a/written"), "w").unwrap();

    // Tightening applies in place.
    d.write_config("read_only = true\nnoexec = false\n");
    let out = d.reconcile();
    assert_success(&out);
    assert!(
        text(&out.stderr).contains("reason=\"configured mount attributes changed\""),
        "{}",
        text(&out.stderr)
    );
    assert!(fs::write(d.path("view/a/again"), "w").is_err());

    // Changing the target template moves the mount.
    let moved = d.path("etc/byssus.toml");
    let content = fs::read_to_string(&moved)
        .unwrap()
        .replace("target = \"{name}\"", "target = \"{name}-ro\"");
    fs::write(&moved, content).unwrap();
    let out = d.reconcile();
    assert_success(&out);
    assert!(d.path("view/a-ro/README").exists());
    assert!(!d.path("view/a").exists());
    assert_eq!(d.state().records().next().unwrap().target, "a-ro");
}

#[test]
#[ignore = "requires mount privileges; run scripts/integration-tests.sh"]
fn removed_group_is_unmounted() {
    let d = Deployment::new();
    d.add_source("a");
    d.join("a");
    assert_success(&d.reconcile());
    assert!(d.visible("a"));

    let file = d.path("etc/byssus.toml");
    let content = fs::read_to_string(&file).unwrap();
    let daemon_only = content.split("[groups.g]").next().unwrap().to_owned();
    fs::write(&file, daemon_only).unwrap();
    let out = d.reconcile();
    assert_success(&out);
    assert!(!d.visible("a"));
    assert!(d.state().is_empty());
}

#[test]
#[ignore = "requires mount privileges; run scripts/integration-tests.sh"]
fn reconcile_refuses_while_lock_is_held() {
    let d = Deployment::new();
    let store = byssus::state::StateStore::open(d.path("state")).unwrap();
    let _lock = byssus::lock::StateLock::acquire(store.dir_fd()).unwrap();
    let out = d.reconcile();
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("holds the state lock"),
        "{}",
        text(&out.stderr)
    );
}

#[test]
#[ignore = "requires mount privileges; run scripts/integration-tests.sh"]
fn reconcile_refuses_insecure_configuration() {
    let d = Deployment::new();
    fs::set_permissions(d.path("etc/byssus.toml"), fs::Permissions::from_mode(0o666)).unwrap();
    let out = d.reconcile();
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("world-writable"),
        "{}",
        text(&out.stderr)
    );
}

#[test]
#[ignore = "requires mount privileges; run scripts/integration-tests.sh"]
fn reconcile_refuses_root_without_user() {
    let d = Deployment::new();
    let out = d.byssus(&["reconcile"]);
    assert!(!out.status.success());
    assert!(text(&out.stderr).contains("refusing to run as root"));
}

#[test]
#[ignore = "requires mount privileges; run scripts/integration-tests.sh"]
fn status_and_dry_run_reports() {
    let d = Deployment::new();
    d.add_source("a");
    d.add_source("b");
    d.join("a");
    assert_success(&d.reconcile());
    d.join("b");
    d.join("missing");

    let out = d.byssus(&["dry-run", "--format", "json"]);
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let group = &json["groups"][0];
    assert_eq!(group["members"], 3);
    assert_eq!(group["existing_mounts"], 1);
    assert_eq!(group["would_create"], 1);
    assert_eq!(group["sources_missing"], 1);
    assert!(!d.visible("b"), "dry-run must not mount");
    // Private propagation in the test sandbox is a warning, not an error.
    assert!(out.status.success(), "{}", text(&out.stdout));

    let out = d.byssus(&["dry-run"]);
    let report = text(&out.stdout);
    assert!(report.contains("kernel.mount_setattr"), "{report}");
    assert!(report.contains("group=g  status=warn"), "{report}");
    assert!(report.contains("g/b  state=would_mount"), "{report}");

    let out = d.byssus(&["status"]);
    assert_success(&out);
    let status = text(&out.stdout);
    assert!(status.contains("g/a  state=mounted"), "{status}");
    assert!(status.contains("g/b  state=would_mount"), "{status}");
    assert!(
        status.contains("g/missing  state=source_unavailable"),
        "{status}"
    );
}

fn fragment(d: &Deployment, group: &str, membership: &str, view: &str) -> String {
    let p = |s: &str| d.path(s).display().to_string();
    format!(
        "[groups.{group}]\nsource_root = \"{}\"\nsource = \"{{name}}/workspace\"\ntarget_root = \"{}\"\ntarget = \"{{name}}\"\nmembership = \"{}\"\n",
        p("src"),
        p(view),
        p(membership)
    )
}

fn write_mode(path: &Path, content: &str, mode: u32) {
    fs::write(path, content).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

#[test]
#[ignore = "requires mount privileges; run scripts/integration-tests.sh"]
fn check_validates_candidate_fragments_without_installing() {
    let d = Deployment::new();
    d.sb.mkdir("staging");
    d.sb.mkdir("members-acme");
    d.sb.mkdir("view-acme");

    // A valid new org fragment.
    let acme = d.path("staging/acme.toml");
    write_mode(
        &acme,
        &fragment(&d, "acme-research", "members-acme", "view-acme"),
        0o644,
    );
    let out = d.byssus(&["check", "--add", acme.to_str().unwrap()]);
    assert_success(&out);
    let report = text(&out.stdout);
    assert!(report.contains("group: acme-research"), "{report}");
    assert!(report.contains("result: valid"), "{report}");
    assert!(
        !d.path("etc/conf.d/acme.toml").exists(),
        "check must not install"
    );

    // JSON output.
    let out = d.byssus(&["check", "--add", acme.to_str().unwrap(), "--format", "json"]);
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json["valid"], true);
    assert_eq!(json["groups"], serde_json::json!(["acme-research", "g"]));

    // A candidate reusing an installed group name is rejected.
    let dup = d.path("staging/dup.toml");
    write_mode(&dup, &fragment(&d, "g", "members-acme", "view-acme"), 0o644);
    let out = d.byssus(&["check", "--add", dup.to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(
        text(&out.stdout).contains("already defined"),
        "{}",
        text(&out.stdout)
    );
    assert!(text(&out.stdout).contains("result: invalid"));

    // A candidate whose membership directory does not exist is rejected.
    let missing = d.path("staging/missing.toml");
    write_mode(
        &missing,
        &fragment(&d, "m", "members-nope", "view-acme"),
        0o644,
    );
    let out = d.byssus(&["check", "--add", missing.to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(
        text(&out.stdout).contains("does not exist"),
        "{}",
        text(&out.stdout)
    );

    // Installed fragments can be removed hypothetically.
    fs::copy(&acme, d.path("etc/conf.d/acme.toml")).unwrap();
    let out = d.byssus(&["check", "--remove", "acme.toml"]);
    assert_success(&out);
    assert!(!text(&out.stdout).contains("acme-research"));
    let out = d.byssus(&["check", "--remove", "nope.toml"]);
    assert!(!out.status.success());
}

#[test]
#[ignore = "requires mount privileges; run scripts/integration-tests.sh"]
fn check_rejects_slave_only_target_unless_allowed() {
    let d = Deployment::new();
    let anchor = d.sb.make_shared_anchor("anchor");
    fs::remove_dir(d.path("view")).unwrap();
    d.sb.attach_consumer(&anchor, "view");
    let out = d.byssus(&["check"]);
    assert!(!out.status.success());
    assert!(
        text(&out.stdout).contains("slave only"),
        "{}",
        text(&out.stdout)
    );
    let out = d.byssus(&["check", "--allow-slave-namespace"]);
    assert_success(&out);
    assert!(text(&out.stdout).contains("warning: group 'g'"));
}

#[test]
#[ignore = "requires mount privileges; run scripts/integration-tests.sh"]
fn check_and_dry_run_warn_when_running_as_root_without_service_user() {
    // The test deployment's configuration has no daemon.user, and the
    // namespace runs the CLI as root.
    let d = Deployment::new();
    let out = d.byssus(&["check"]);
    assert_success(&out);
    let report = text(&out.stdout);
    assert!(
        report.contains("warning: checks ran as root because daemon.user is not set"),
        "{report}"
    );

    let out = d.byssus(&["dry-run"]);
    let report = text(&out.stdout);
    assert!(
        report.contains("privileges") && report.contains("= warn (checks ran as root"),
        "{report}"
    );
}
