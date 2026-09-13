//! Group sets: groups and subgroups discovered from directories, end to end.

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use rustix::process::Signal;

use crate::cli::{Deployment, assert_success, text};
use crate::daemon::{Daemon, wait_until};

/// A deployment laid out like a multi-tenant platform, with one group set
/// whose groups are orgs and whose subgroups are user-created groups.
/// Periodic resync is disabled, so every change must be event-driven.
struct Platform {
    d: Deployment,
    orgs: PathBuf,
}

impl Platform {
    fn new() -> Self {
        let d = Deployment::new();
        let orgs = d.sb.make_shared_anchor("orgs");
        d.sb.mkdir("membership");
        let p = |s: &str| d.path(s).display().to_string();
        d.write_main_config(&format!(
            "[daemon]\nstate_dir = \"{}\"\nresync_interval_secs = 0\n\n\
             [group_sets.projects]\nmembership_root = \"{}\"\nsource_root = \"{}\"\n\
             source = \"{{group}}/projects/{{name}}/workspace\"\ntarget_root = \"{}\"\n\
             target = \"{{group}}/groups/{{subgroup}}/view/{{name}}\"\n",
            p("state"),
            p("membership"),
            p("orgs"),
            p("orgs"),
        ));
        Self { d, orgs }
    }

    fn project(&self, org: &str, project: &str) {
        self.d.sb.write(
            &format!("orgs/{org}/projects/{project}/workspace/README"),
            &format!("{org}/{project}"),
        );
    }

    /// Creates the subgroup's view and a simulated container attached to it.
    fn view(&self, org: &str, subgroup: &str) -> PathBuf {
        let view = self
            .d
            .sb
            .mkdir(&format!("orgs/{org}/groups/{subgroup}/view"));
        self.d
            .sb
            .attach_consumer(&view, &format!("containers/{org}-{subgroup}"))
    }

    fn subgroup(&self, org: &str, subgroup: &str) {
        self.d.sb.mkdir(&format!("membership/{org}/{subgroup}"));
    }

    fn member(&self, org: &str, subgroup: &str, name: &str) {
        fs::write(
            self.d.path(&format!("membership/{org}/{subgroup}/{name}")),
            "",
        )
        .unwrap();
    }
}

#[test]
#[ignore = "requires mount privileges; run scripts/integration-tests.sh"]
fn subgroups_are_created_and_removed_at_runtime() {
    let p = Platform::new();
    p.project("acme", "webapp");
    p.project("acme", "api");
    p.project("beta", "secret");
    let daemon = Daemon::started(&p.d);

    // A subgroup appears by creating directories; no configuration change.
    let research = p.view("acme", "research");
    p.subgroup("acme", "research");
    p.member("acme", "research", "webapp");
    wait_until("webapp in acme/research", || {
        research.join("webapp/README").exists()
    });
    assert_eq!(
        fs::read_to_string(research.join("webapp/README")).unwrap(),
        "acme/webapp"
    );
    assert!(
        fs::write(research.join("webapp/x"), "x").is_err(),
        "read-only"
    );
    daemon.wait_log("op=mount group=projects/acme/research name=webapp");

    // Another subgroup of the same org has its own view.
    let monitoring = p.view("acme", "monitoring");
    p.subgroup("acme", "monitoring");
    p.member("acme", "monitoring", "api");
    wait_until("api in acme/monitoring", || {
        monitoring.join("api/README").exists()
    });
    assert!(
        !research.join("api").exists(),
        "subgroups have separate views"
    );

    // A member name only resolves within its own org's projects.
    p.member("acme", "research", "secret");
    daemon.wait_log("op=skip group=projects/acme/research name=secret");
    assert!(
        !research.join("secret/README").exists(),
        "another org's project was exposed"
    );
    assert!(
        !p.orgs
            .join("acme/groups/research/view/secret/README")
            .exists()
    );

    // A brand-new org and subgroup created at runtime.
    p.project("gamma", "tool");
    let gamma = p.view("gamma", "builds");
    p.subgroup("gamma", "builds");
    p.member("gamma", "builds", "tool");
    wait_until("tool in gamma/builds", || {
        gamma.join("tool/README").exists()
    });

    // Status shows set groups by their full identity.
    let out = p.d.byssus(&["status", "--format", "json"]);
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let mounted: Vec<String> = json["members"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["state"] == "mounted")
        .map(|m| {
            format!(
                "{}/{}",
                m["group"].as_str().unwrap(),
                m["name"].as_str().unwrap()
            )
        })
        .collect();
    assert_eq!(
        mounted,
        [
            "projects/acme/monitoring/api",
            "projects/acme/research/webapp",
            "projects/gamma/builds/tool"
        ]
    );

    // Deleting a subgroup directory removes the subgroup and its mounts.
    fs::remove_dir_all(p.d.path("membership/acme/monitoring")).unwrap();
    wait_until("api unmounted", || !monitoring.join("api/README").exists());
    assert!(
        research.join("webapp/README").exists(),
        "other subgroups unaffected"
    );

    // Deleting an org directory removes all its subgroups.
    fs::remove_dir_all(p.d.path("membership/gamma")).unwrap();
    wait_until("tool unmounted", || !gamma.join("tool/README").exists());

    assert_eq!(daemon.stop(), 0);
    let groups: Vec<String> = p.d.state().records().map(|r| r.group.to_string()).collect();
    assert_eq!(groups, ["projects/acme/research"]);

    // A restart finds everything as recorded.
    let daemon = Daemon::started(&p.d);
    assert!(
        daemon
            .log()
            .contains("msg=\"reconcile complete\" trigger=startup steps=0"),
        "{}",
        daemon.log()
    );
    assert_eq!(daemon.stop(), 0);
}

#[test]
#[ignore = "requires mount privileges; run scripts/integration-tests.sh"]
fn replaced_subgroup_directories_are_followed() {
    let p = Platform::new();
    p.project("acme", "webapp");
    p.project("acme", "api");
    let monitoring = p.view("acme", "monitoring");
    p.subgroup("acme", "monitoring");
    p.member("acme", "monitoring", "api");
    let daemon = Daemon::started(&p.d);
    wait_until("api in acme/monitoring", || {
        monitoring.join("api/README").exists()
    });

    // A subgroup directory replaced by a new one within one burst of
    // changes is followed: the old incarnation's members go, and later
    // changes in the new one are seen.
    fs::rename(
        p.d.path("membership/acme/monitoring"),
        p.d.path("monitoring-old"),
    )
    .unwrap();
    p.subgroup("acme", "monitoring");
    p.member("acme", "monitoring", "webapp");
    wait_until("webapp in replaced acme/monitoring", || {
        monitoring.join("webapp/README").exists() && !monitoring.join("api/README").exists()
    });
    fs::write(p.d.path("monitoring-old/stale"), "").unwrap();
    p.member("acme", "monitoring", "api");
    wait_until("api back in acme/monitoring", || {
        monitoring.join("api/README").exists()
    });
    assert!(!monitoring.join("stale").exists());
    fs::remove_file(p.d.path("membership/acme/monitoring/webapp")).unwrap();
    wait_until("webapp removed from acme/monitoring", || {
        !monitoring.join("webapp/README").exists()
    });
    assert_eq!(daemon.stop(), 0);
}

#[test]
#[ignore = "requires mount privileges; run scripts/integration-tests.sh"]
fn invalid_set_entries_are_rejected_and_hidden_ones_ignored() {
    let p = Platform::new();
    p.project("acme", "webapp");
    let research = p.view("acme", "research");
    p.subgroup("acme", "research");
    p.member("acme", "research", "webapp");
    fs::write(p.d.path("membership/stray-file"), "").unwrap();
    fs::write(p.d.path("membership/acme/member-at-org-level"), "").unwrap();
    p.d.sb.mkdir("membership/.staging");
    std::os::unix::fs::symlink(
        p.d.path("membership/acme"),
        p.d.path("membership/linked-org"),
    )
    .unwrap();

    let out = p.d.reconcile();
    assert_success(&out);
    let log = text(&out.stderr);
    for entry in ["stray-file", "acme/member-at-org-level", "linked-org"] {
        assert!(
            log.contains(&format!("op=reject group=projects/* name={entry}")),
            "{entry}: {log}"
        );
    }
    assert!(!log.contains(".staging"), "{log}");
    assert!(research.join("webapp/README").exists());

    let out = p.d.byssus(&["check"]);
    assert_success(&out);
    let report = text(&out.stdout);
    assert!(report.contains("group: projects/*"), "{report}");
    assert!(report.contains("entry 'stray-file' rejected"), "{report}");
}

#[test]
#[ignore = "requires mount privileges; run scripts/integration-tests.sh"]
fn lost_membership_root_freezes_the_set() {
    let p = Platform::new();
    p.project("acme", "webapp");
    let research = p.view("acme", "research");
    p.subgroup("acme", "research");
    p.member("acme", "research", "webapp");
    let daemon = Daemon::started(&p.d);
    wait_until("mounted", || research.join("webapp/README").exists());

    fs::rename(p.d.path("membership"), p.d.path("membership-moved")).unwrap();
    daemon.wait_log("op=degrade group=projects/*");
    std::thread::sleep(Duration::from_millis(1500));
    assert!(
        research.join("webapp/README").exists(),
        "mounts must be kept"
    );

    // Restore and reload: the set recovers.
    fs::rename(p.d.path("membership-moved"), p.d.path("membership")).unwrap();
    daemon.signal(Signal::HUP);
    daemon.wait_log("msg=\"configuration reloaded\"");
    fs::remove_file(p.d.path("membership/acme/research/webapp")).unwrap();
    wait_until("unmounted after recovery", || {
        !research.join("webapp/README").exists()
    });
    assert_eq!(daemon.stop(), 0);
}

#[test]
#[ignore = "requires mount privileges; run scripts/integration-tests.sh"]
fn one_level_sets_still_work() {
    let d = Deployment::new();
    let p = |s: &str| d.path(s).display().to_string();
    d.sb.make_shared_anchor("orgs");
    d.sb.mkdir("membership/research/acme");
    d.sb.write("orgs/acme/projects/webapp/workspace/README", "w");
    d.sb.mkdir("orgs/acme/view");
    d.write_main_config(&format!(
        "[daemon]\nstate_dir = \"{}\"\n\n[group_sets.research]\nmembership_root = \"{}\"\n\
         source_root = \"{}\"\nsource = \"{{group}}/projects/{{name}}/workspace\"\n\
         target_root = \"{}\"\ntarget = \"{{group}}/view/{{name}}\"\n",
        p("state"),
        p("membership/research"),
        p("orgs"),
        p("orgs"),
    ));
    fs::write(d.path("membership/research/acme/webapp"), "").unwrap();
    let out = d.reconcile();
    assert_success(&out);
    assert!(
        text(&out.stderr).contains("op=mount group=research/acme name=webapp"),
        "{}",
        text(&out.stderr)
    );
    assert!(d.path("orgs/acme/view/webapp/README").exists());
}
