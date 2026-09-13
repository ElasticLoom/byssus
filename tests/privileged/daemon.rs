//! End-to-end tests of `byssusd`: live membership changes, propagation,
//! reload, restart, degradation and resync.

use std::fs;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal};

use crate::cli::{Deployment, bin, text};

const TIMEOUT: Duration = Duration::from_secs(10);

fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let start = Instant::now();
    while start.elapsed() < TIMEOUT {
        if condition() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for: {what}");
}

struct Daemon {
    child: Option<Child>,
    log: PathBuf,
}

impl Daemon {
    fn start(d: &Deployment, extra: &[&str]) -> Self {
        Self::start_with_env(d, extra, &[])
    }

    fn start_with_env(d: &Deployment, extra: &[&str], env: &[(&str, &std::path::Path)]) -> Self {
        let log = d.path(&format!(
            "daemon-{}.log",
            Instant::now().elapsed().as_nanos()
        ));
        let log = unique_log(&log);
        let file = fs::File::create(&log).unwrap();
        let child = Command::new(bin("byssusd"))
            .args(d.config_args())
            .args(["--allow-root", "--log-level", "debug"])
            .args(extra)
            .envs(env.iter().map(|(k, v)| (*k, *v)))
            .stdout(Stdio::null())
            .stderr(file)
            .spawn()
            .expect("spawn byssusd");
        Self {
            child: Some(child),
            log,
        }
    }

    fn started(d: &Deployment) -> Self {
        let daemon = Self::start(d, &[]);
        daemon.wait_log("msg=\"reconcile complete\" trigger=startup");
        daemon
    }

    fn log(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn wait_log(&self, needle: &str) {
        let start = Instant::now();
        while start.elapsed() < TIMEOUT {
            if self.log().contains(needle) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("timed out waiting for log {needle:?}\n{}", self.log());
    }

    fn count(&self, needle: &str) -> usize {
        self.log().matches(needle).count()
    }

    fn signal(&self, signal: Signal) {
        let pid = self.child.as_ref().unwrap().id();
        let pid = Pid::from_raw(i32::try_from(pid).unwrap()).unwrap();
        rustix::process::kill_process(pid, signal).unwrap();
    }

    fn stop(self) -> i32 {
        self.signal(Signal::TERM);
        self.stop_after_signal()
    }

    fn stop_after_signal(mut self) -> i32 {
        let mut child = self.child.take().unwrap();
        let start = Instant::now();
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                return status.code().unwrap_or(-1);
            }
            assert!(
                start.elapsed() < TIMEOUT,
                "byssusd did not stop\n{}",
                self.log()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn wait_exit(mut self) -> (i32, String) {
        let mut child = self.child.take().unwrap();
        let start = Instant::now();
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                return (status.code().unwrap_or(-1), self.log());
            }
            assert!(
                start.elapsed() < TIMEOUT,
                "byssusd did not exit\n{}",
                self.log()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn unique_log(base: &std::path::Path) -> PathBuf {
    let mut candidate = base.to_path_buf();
    let mut n = 0;
    while candidate.exists() {
        n += 1;
        candidate = base.with_extension(format!("{n}.log"));
    }
    candidate
}

#[test]
#[ignore = "requires mount privileges; run scripts/integration-tests.sh"]
fn live_changes_reach_a_running_consumer() {
    let d = Deployment::new();
    let view = d.sb.make_shared_anchor("view");
    let consumer = d.sb.attach_consumer(&view, "container/group");
    d.add_source("libcurl");
    let daemon = Daemon::started(&d);
    assert!(daemon.log().contains("msg=\"byssusd started\""));

    d.join("libcurl");
    wait_until("member visible in consumer", || {
        consumer.join("libcurl/README").exists()
    });
    assert_eq!(
        fs::read_to_string(consumer.join("libcurl/README")).unwrap(),
        "I am libcurl"
    );
    assert!(fs::write(consumer.join("libcurl/x"), "x").is_err());
    daemon.wait_log("op=mount group=g name=libcurl");
    assert!(daemon.log().contains("trigger=inotify"));

    d.leave("libcurl");
    wait_until("member gone from consumer", || {
        !consumer.join("libcurl/README").exists()
    });

    d.join("libcurl");
    wait_until("member visible again", || {
        consumer.join("libcurl/README").exists()
    });
    assert_eq!(daemon.stop(), 0);
    // Mounts survive daemon shutdown.
    assert!(consumer.join("libcurl/README").exists());
    assert!(d.visible("libcurl"));
}

#[test]
#[ignore = "requires mount privileges; run scripts/integration-tests.sh"]
fn restart_is_a_no_op() {
    let d = Deployment::new();
    d.add_source("a");
    d.add_source("b");
    d.join("a");
    d.join("b");
    let daemon = Daemon::started(&d);
    wait_until("mounted", || d.visible("a") && d.visible("b"));
    assert_eq!(daemon.stop(), 0);
    let before = d.state();

    let daemon = Daemon::started(&d);
    let log = daemon.log();
    assert!(
        log.contains("msg=\"reconcile complete\" trigger=startup steps=0"),
        "{log}"
    );
    assert!(!log.contains("op=mount"));
    assert_eq!(daemon.stop(), 0);
    assert_eq!(d.state(), before);
}

#[test]
#[ignore = "requires mount privileges; run scripts/integration-tests.sh"]
fn reload_adds_and_removes_groups_and_rejects_bad_config() {
    let d = Deployment::new();
    d.add_source("a");
    d.join("a");
    d.sb.mkdir("members-h");
    d.sb.mkdir("view-h");
    let daemon = Daemon::started(&d);
    wait_until("group g mounted", || d.visible("a"));

    // Add group h through a drop-in fragment.
    let fragment = d.path("etc/conf.d/h.toml");
    let p = |s: &str| d.path(s).display().to_string();
    let write_fragment = |content: String| {
        fs::write(&fragment, content).unwrap();
        std::os::unix::fs::PermissionsExt::set_mode(
            &mut fs::metadata(&fragment).unwrap().permissions(),
            0o644,
        );
        fs::set_permissions(
            &fragment,
            std::os::unix::fs::PermissionsExt::from_mode(0o644),
        )
        .unwrap();
    };
    write_fragment(format!(
        "[groups.h]\nsource_root = \"{}\"\nsource = \"{{name}}/workspace\"\ntarget_root = \"{}\"\ntarget = \"{{name}}\"\nmembership = \"{}\"\n",
        p("src"),
        p("view-h"),
        p("members-h")
    ));
    fs::write(d.path("members-h/a"), "").unwrap();
    daemon.signal(Signal::HUP);
    daemon.wait_log("msg=\"configuration reloaded\" groups=2");
    wait_until("group h mounted", || d.path("view-h/a/README").exists());

    // An invalid configuration is rejected and the old one keeps working.
    fs::write(d.path("etc/conf.d/zz-bad.toml"), "[groups.bad\n").unwrap();
    fs::set_permissions(
        d.path("etc/conf.d/zz-bad.toml"),
        std::os::unix::fs::PermissionsExt::from_mode(0o644),
    )
    .unwrap();
    daemon.signal(Signal::HUP);
    daemon.wait_log("reload failed; keeping previous configuration");
    d.add_source("b");
    fs::write(d.path("members-h/b"), "").unwrap();
    wait_until("group h still live", || d.path("view-h/b/README").exists());
    fs::remove_file(d.path("etc/conf.d/zz-bad.toml")).unwrap();

    // Removing the fragment removes the group's mounts.
    fs::remove_file(&fragment).unwrap();
    daemon.signal(Signal::HUP);
    daemon.wait_log("msg=\"configuration reloaded\" groups=1");
    wait_until("group h unmounted", || {
        !d.path("view-h/a/README").exists() && !d.path("view-h/b/README").exists()
    });
    assert!(d.visible("a"));
    assert_eq!(daemon.stop(), 0);
    assert_eq!(d.state().len(), 1);
}

#[test]
#[ignore = "requires mount privileges; run scripts/integration-tests.sh"]
fn rapid_churn_converges() {
    let d = Deployment::new();
    let names: Vec<String> = (0..8).map(|i| format!("m{i}")).collect();
    for n in &names {
        d.add_source(n);
    }
    let daemon = Daemon::started(&d);
    for round in 0..40 {
        for (i, n) in names.iter().enumerate() {
            let path = d.path(&format!("members/{n}"));
            if (round + i) % 3 == 0 {
                let _ = fs::write(&path, "");
            } else {
                let _ = fs::remove_file(&path);
            }
        }
    }
    // Final desired set.
    for (i, n) in names.iter().enumerate() {
        let path = d.path(&format!("members/{n}"));
        if i % 2 == 0 {
            fs::write(&path, "").unwrap();
        } else {
            let _ = fs::remove_file(&path);
        }
    }
    wait_until("converged", || {
        names
            .iter()
            .enumerate()
            .all(|(i, n)| d.visible(n) == (i % 2 == 0))
    });
    assert_eq!(daemon.stop(), 0);
    assert_eq!(d.state().len(), 4);
}

#[test]
#[ignore = "requires mount privileges; run scripts/integration-tests.sh"]
fn moved_membership_directory_degrades_group() {
    let d = Deployment::new();
    d.add_source("a");
    d.join("a");
    let daemon = Daemon::started(&d);
    wait_until("mounted", || d.visible("a"));

    fs::rename(d.path("members"), d.path("members-moved")).unwrap();
    daemon.wait_log("op=degrade group=g");
    std::thread::sleep(Duration::from_millis(1500));
    assert!(d.visible("a"), "existing mounts must be kept");

    // Restore an empty directory and reload: the group recovers.
    fs::create_dir(d.path("members")).unwrap();
    daemon.signal(Signal::HUP);
    daemon.wait_log("msg=\"configuration reloaded\"");
    wait_until("unmounted after recovery", || !d.visible("a"));
    assert_eq!(daemon.stop(), 0);
}

#[test]
#[ignore = "requires mount privileges; run scripts/integration-tests.sh"]
fn recursively_deleted_membership_directory_keeps_mounts() {
    let d = Deployment::new();
    for n in ["a", "b", "c"] {
        d.add_source(n);
        d.join(n);
    }
    let daemon = Daemon::started(&d);
    wait_until("mounted", || {
        d.visible("a") && d.visible("b") && d.visible("c")
    });

    fs::remove_dir_all(d.path("members")).unwrap();
    daemon.wait_log("op=degrade group=g");
    std::thread::sleep(Duration::from_millis(1500));
    assert!(
        d.visible("a") && d.visible("b") && d.visible("c"),
        "rm -rf of the membership directory must not mass-unmount\n{}",
        daemon.log()
    );
    assert_eq!(daemon.stop(), 0);
}

#[test]
#[ignore = "requires mount privileges; run scripts/integration-tests.sh"]
fn resync_recreates_externally_removed_mount() {
    let d = Deployment::new();
    d.add_source("a");
    d.join("a");
    let daemon = Daemon::started(&d);
    wait_until("mounted", || d.visible("a"));
    rustix::mount::unmount(
        d.path("view/a").as_path(),
        rustix::mount::UnmountFlags::DETACH,
    )
    .unwrap();
    assert!(!d.visible("a"));
    wait_until("remounted by resync", || d.visible("a"));
    daemon.wait_log("trigger=resync");
    assert_eq!(daemon.stop(), 0);
}

#[test]
#[ignore = "requires mount privileges; run scripts/integration-tests.sh"]
fn slave_only_target_root_is_refused() {
    let d = Deployment::new();
    let anchor = d.sb.make_shared_anchor("anchor");
    // Replace view with a slave of the anchor.
    fs::remove_dir(d.path("view")).unwrap();
    d.sb.attach_consumer(&anchor, "view");

    let (code, log) = Daemon::start(&d, &[]).wait_exit();
    assert_eq!(code, 1, "{log}");
    assert!(log.contains("slave-only"), "{log}");

    let daemon = Daemon::start(&d, &["--allow-slave-namespace"]);
    daemon.wait_log("msg=\"byssusd started\"");
    assert_eq!(daemon.stop(), 0);
}

#[test]
#[ignore = "requires mount privileges; run scripts/integration-tests.sh"]
fn second_instance_refuses_to_start() {
    let d = Deployment::new();
    let first = Daemon::started(&d);
    let (code, log) = Daemon::start(&d, &[]).wait_exit();
    assert_eq!(code, 1);
    assert!(log.contains("holds the state lock"), "{log}");
    let reconcile = d.reconcile();
    assert!(!reconcile.status.success());
    assert!(text(&reconcile.stderr).contains("holds the state lock"));
    assert_eq!(first.stop(), 0);
    assert!(first_log_mentions_shutdown(&d));
}

fn first_log_mentions_shutdown(d: &Deployment) -> bool {
    fs::read_dir(d.path(""))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with("daemon-"))
        .any(|e| {
            fs::read_to_string(e.path())
                .unwrap_or_default()
                .contains("byssusd stopped; mounts are preserved")
        })
}

#[test]
#[ignore = "requires mount privileges; run scripts/integration-tests.sh"]
fn non_empty_file_written_later_removes_member() {
    let d = Deployment::new();
    d.add_source("a");
    d.join("a");
    let daemon = Daemon::started(&d);
    wait_until("mounted", || d.visible("a"));
    fs::write(d.path("members/a"), "oops").unwrap();
    wait_until("unmounted", || !d.visible("a"));
    assert!(daemon.count("op=reject group=g name=a") >= 1);
    assert_eq!(daemon.stop(), 0);
}

#[test]
#[ignore = "requires mount privileges; run scripts/integration-tests.sh"]
fn persistent_rejection_is_logged_once_and_cleared() {
    let d = Deployment::new();
    d.add_source("a");
    d.join("a");
    std::os::unix::fs::symlink(d.path("members/a"), d.path("members/link")).unwrap();
    fs::write(d.path("members/.gitkeep"), "").unwrap();
    let daemon = Daemon::started(&d);
    daemon.wait_log("op=reject group=g name=link");

    // Several resync passes (interval 1s) must not repeat the warning.
    let resyncs_before = daemon.count("trigger=resync");
    wait_until("three resyncs", || {
        daemon.count("trigger=resync") >= resyncs_before + 3
    });
    assert_eq!(
        daemon.count("op=reject group=g name=link"),
        1,
        "{}",
        daemon.log()
    );
    assert!(
        !daemon
            .log()
            .contains("level=warn op=reject group=g name=.gitkeep")
    );
    assert!(
        daemon.log().contains("op=ignore group=g"),
        "{}",
        daemon.log()
    );

    fs::remove_file(d.path("members/link")).unwrap();
    daemon.wait_log("op=reject_cleared group=g name=link");
    assert_eq!(daemon.stop(), 0);
}

#[test]
#[ignore = "requires mount privileges; run scripts/integration-tests.sh"]
fn read_write_group_through_container_view() {
    let d = Deployment::new();
    d.write_config("read_only = false\n");
    let view = d.sb.make_shared_anchor("view");
    // A container binding the view with rslave, as documented, both writable
    // and read-only at the bind level.
    let consumer = d.sb.attach_consumer(&view, "container/group");
    let ro_consumer = d.sb.attach_consumer(&view, "container-ro/group");
    rustix::mount::mount_remount(
        ro_consumer.as_path(),
        rustix::mount::MountFlags::BIND | rustix::mount::MountFlags::RDONLY,
        "",
    )
    .unwrap();
    d.add_source("shared");
    d.join("shared");
    let daemon = Daemon::started(&d);
    wait_until("visible", || consumer.join("shared/README").exists());

    // Create, modify, rename and delete through the container view; changes
    // land in the member's source directory.
    fs::write(consumer.join("shared/new.txt"), "from container").unwrap();
    assert_eq!(
        fs::read_to_string(d.path("src/shared/workspace/new.txt")).unwrap(),
        "from container"
    );
    fs::write(consumer.join("shared/README"), "edited").unwrap();
    assert_eq!(
        fs::read_to_string(d.path("src/shared/workspace/README")).unwrap(),
        "edited"
    );
    fs::rename(
        consumer.join("shared/new.txt"),
        consumer.join("shared/renamed.txt"),
    )
    .unwrap();
    fs::create_dir(consumer.join("shared/dir")).unwrap();
    fs::remove_file(consumer.join("shared/renamed.txt")).unwrap();
    assert!(d.path("src/shared/workspace/dir").is_dir());
    assert!(!d.path("src/shared/workspace/renamed.txt").exists());

    // A read-only bind of the view does not restrict read-write members.
    assert!(fs::write(ro_consumer.join("shared/through-ro-bind.txt"), "x").is_ok());

    // Tightening to read-only re-creates the mount, reaching the container.
    d.write_config("read_only = true\n");
    daemon.signal(Signal::HUP);
    daemon.wait_log("reason=\"configured mount attributes changed\"");
    wait_until("read-only in container", || {
        consumer.join("shared/README").exists()
            && fs::write(consumer.join("shared/after.txt"), "x")
                .is_err_and(|e| e.raw_os_error() == Some(libc::EROFS))
    });
    assert!(
        fs::write(ro_consumer.join("shared/after.txt"), "x").is_err(),
        "read-only reaches every consumer"
    );

    // Loosening again re-creates it writable.
    d.write_config("read_only = false\n");
    daemon.signal(Signal::HUP);
    wait_until("writable again", || {
        fs::write(consumer.join("shared/again.txt"), "x").is_ok()
    });
    assert_eq!(daemon.stop(), 0);
    let record = d.state();
    assert!(!record.records().next().unwrap().read_only);
}

/// Receives notify messages until one satisfies `pred`, returning it.
fn next_notify(
    socket: &std::os::unix::net::UnixDatagram,
    what: &str,
    pred: impl Fn(&str) -> bool,
) -> String {
    let start = Instant::now();
    let mut buf = [0u8; 4096];
    while start.elapsed() < TIMEOUT {
        match socket.recv(&mut buf) {
            Ok(n) => {
                let msg = String::from_utf8_lossy(&buf[..n]).into_owned();
                if pred(&msg) {
                    return msg;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => panic!("recv: {e}"),
        }
    }
    panic!("timed out waiting for notify message: {what}");
}

#[test]
#[ignore = "requires mount privileges; run scripts/integration-tests.sh"]
fn notifies_systemd_ready_only_after_startup_reconcile() {
    let d = Deployment::new();
    d.add_source("a");
    d.join("a");
    let socket_path = d.path("notify.sock");
    let socket = std::os::unix::net::UnixDatagram::bind(&socket_path).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();

    let daemon = Daemon::start_with_env(&d, &[], &[("NOTIFY_SOCKET", &socket_path)]);
    let ready = next_notify(&socket, "READY", |m| m.contains("READY=1"));
    // The member is mounted by the time readiness is reported.
    assert!(d.visible("a"), "READY sent before the startup reconcile");
    assert!(ready.contains("STATUS=1 group(s), 1 mount(s)"), "{ready}");

    // Reload: RELOADING, then READY after the reload's reconcile.
    daemon.signal(Signal::HUP);
    let reloading = next_notify(&socket, "RELOADING", |m| m.contains("RELOADING=1"));
    assert!(reloading.contains("MONOTONIC_USEC="), "{reloading}");
    next_notify(&socket, "READY after reload", |m| m.contains("READY=1"));

    // A rejected reload is visible in the status line.
    let bad = d.path("etc/conf.d/zz-bad.toml");
    fs::write(&bad, "[groups.bad\n").unwrap();
    fs::set_permissions(&bad, std::os::unix::fs::PermissionsExt::from_mode(0o644)).unwrap();
    daemon.signal(Signal::HUP);
    let after_failure = next_notify(&socket, "READY after failed reload", |m| {
        m.contains("READY=1")
    });
    assert!(
        after_failure.contains("last reload failed, running previous configuration"),
        "{after_failure}"
    );
    fs::remove_file(&bad).unwrap();
    daemon.signal(Signal::HUP);
    let recovered = next_notify(&socket, "READY after fixed reload", |m| {
        m.contains("READY=1")
    });
    assert!(!recovered.contains("last reload failed"), "{recovered}");

    // Membership changes update the status line.
    d.add_source("b");
    d.join("b");
    next_notify(&socket, "status with two mounts", |m| {
        m.contains("STATUS=1 group(s), 2 mount(s)")
    });

    daemon.signal(Signal::TERM);
    next_notify(&socket, "STOPPING", |m| m.contains("STOPPING=1"));
    assert_eq!(daemon.stop_after_signal(), 0);
}
