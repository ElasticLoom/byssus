//! Privilege normalization applied to a real (forked) process.

use byssus::privileges::apply::{self, ProcStatus};
use byssus::privileges::plan::{self, Goal, Request};
use rustix::thread::CapabilitySet;

use crate::common::require_test_namespace;

/// Runs `f` in a forked, single-threaded child and returns its exit status.
fn in_child(f: fn() -> i32) -> i32 {
    in_child_with(f)
}

/// Like [`in_child`] but accepts a closure.
pub fn in_child_with<F: FnOnce() -> i32 + std::panic::UnwindSafe>(f: F) -> i32 {
    // SAFETY: the test harness runs with --test-threads=1, and the child only
    // runs the given test code before exiting with `_exit`, never returning
    // into the harness.
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork failed");
    if pid == 0 {
        let code = std::panic::catch_unwind(f).unwrap_or(101);
        // SAFETY: terminating the child immediately without running parent
        // destructors or atexit handlers.
        unsafe { libc::_exit(code) };
    }
    let mut status = 0;
    // SAFETY: waiting for the child we just forked; `status` is a valid
    // pointer.
    let rc = unsafe { libc::waitpid(pid, &raw mut status, 0) };
    assert_eq!(rc, pid);
    assert!(libc::WIFEXITED(status), "child did not exit normally");
    libc::WEXITSTATUS(status)
}

#[test]
#[ignore = "requires a user namespace; run scripts/integration-tests.sh"]
fn root_with_allow_root_is_reduced_to_sys_admin() {
    require_test_namespace();
    let code = in_child(|| {
        let before = ProcStatus::read_self().unwrap();
        assert_eq!(before.uids[1], 0, "expected namespace root");
        let request = Request {
            goal: Goal::KeepSysAdmin,
            user: None,
            allow_root: true,
        };
        let p = plan::plan(&before.credentials(), &request).unwrap();
        let after = apply::apply(&p).unwrap();
        assert_eq!(after.cap_effective, CapabilitySet::SYS_ADMIN.bits());
        assert_eq!(after.cap_permitted, CapabilitySet::SYS_ADMIN.bits());
        assert_eq!(after.cap_bounding, CapabilitySet::SYS_ADMIN.bits());
        assert_eq!(after.cap_ambient, 0);
        assert!(after.no_new_privs);
        // Securebits are locked: an attempt to regain capabilities fails.
        let all = rustix::thread::CapabilitySets {
            effective: CapabilitySet::all(),
            permitted: CapabilitySet::all(),
            inheritable: CapabilitySet::empty(),
        };
        assert!(rustix::thread::set_capabilities(None, all).is_err());
        0
    });
    assert_eq!(code, 0);
}

#[test]
#[ignore = "requires a user namespace; run scripts/integration-tests.sh"]
fn drop_all_leaves_no_capabilities() {
    require_test_namespace();
    let code = in_child(|| {
        let before = ProcStatus::read_self().unwrap();
        let request = Request {
            goal: Goal::DropAll,
            user: None,
            allow_root: false,
        };
        let p = plan::plan(&before.credentials(), &request).unwrap();
        let after = apply::apply(&p).unwrap();
        assert_eq!(after.cap_effective, 0);
        assert_eq!(after.cap_permitted, 0);
        0
    });
    assert_eq!(code, 0);
}

#[test]
#[ignore = "requires a user namespace; run scripts/integration-tests.sh"]
fn apply_refuses_multithreaded_process() {
    require_test_namespace();
    let code = in_child(|| {
        let _t = std::thread::spawn(|| std::thread::sleep(std::time::Duration::from_secs(5)));
        let before = ProcStatus::read_self().unwrap();
        let request = Request {
            goal: Goal::DropAll,
            user: None,
            allow_root: false,
        };
        let p = plan::plan(&before.credentials(), &request).unwrap();
        match apply::apply(&p) {
            Err(apply::ApplyError::MultiThreaded(n)) if n > 1 => 0,
            other => {
                eprintln!("unexpected: {other:?}");
                1
            }
        }
    });
    assert_eq!(code, 0);
}
