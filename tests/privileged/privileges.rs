//! Privilege normalization applied to a real (forked) process.

use byssus::privileges::apply::{self, ProcessState};
use byssus::privileges::plan::{self, Goal, Request};
use std::os::fd::AsFd;

use rustix::mount::MountPropagationFlags;
use rustix::thread::CapabilitySet;

use crate::common::{proc, require_test_namespace};

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
        let before = ProcessState::read_self(proc().as_fd()).unwrap();
        assert_eq!(before.uids[1], 0, "expected namespace root");
        let request = Request {
            goal: Goal::KeepSysAdmin,
            user: None,
            allow_root: true,
        };
        let p = plan::plan(&before.credentials(), &request).unwrap();
        let after = apply::apply(&p, proc().as_fd()).unwrap();
        assert_eq!(after.cap_effective, CapabilitySet::SYS_ADMIN.bits());
        assert_eq!(after.cap_permitted, CapabilitySet::SYS_ADMIN.bits());
        assert_eq!(after.cap_bounding, CapabilitySet::SYS_ADMIN.bits());
        assert_eq!(after.cap_ambient, 0);
        assert!(after.no_new_privs);
        let locked = plan::LOCKED_SECURE_BITS.bits();
        assert_eq!(after.secure_bits & locked, locked);
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
fn normalization_keeps_securebits_already_set() {
    require_test_namespace();
    let code = in_child(|| {
        // SECBIT_EXEC_RESTRICT_FILE (Linux 6.14+), settable without privileges.
        const EXEC_RESTRICT_FILE: libc::c_ulong = 1 << 8;
        // SAFETY: PR_SET_SECUREBITS takes no pointers; unused arguments are zero.
        if unsafe { libc::prctl(libc::PR_SET_SECUREBITS, EXEC_RESTRICT_FILE, 0, 0, 0) } != 0 {
            eprintln!("skipping: kernel does not support SECBIT_EXEC_RESTRICT_FILE");
            return 0;
        }
        let before = ProcessState::read_self(proc().as_fd()).unwrap();
        let request = Request {
            goal: Goal::KeepSysAdmin,
            user: None,
            allow_root: true,
        };
        let p = plan::plan(&before.credentials(), &request).unwrap();
        let after = apply::apply(&p, proc().as_fd()).unwrap();
        let locked = plan::LOCKED_SECURE_BITS.bits();
        assert_eq!(after.secure_bits & locked, locked);
        assert_eq!(
            u64::from(after.secure_bits) & EXEC_RESTRICT_FILE,
            EXEC_RESTRICT_FILE
        );
        0
    });
    assert_eq!(code, 0);
}

#[test]
#[ignore = "requires a user namespace; run scripts/integration-tests.sh"]
fn drop_all_leaves_no_capabilities() {
    require_test_namespace();
    let code = in_child(|| {
        let before = ProcessState::read_self(proc().as_fd()).unwrap();
        let request = Request {
            goal: Goal::DropAll,
            user: None,
            allow_root: false,
        };
        let p = plan::plan(&before.credentials(), &request).unwrap();
        let after = apply::apply(&p, proc().as_fd()).unwrap();
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
        let before = ProcessState::read_self(proc().as_fd()).unwrap();
        let request = Request {
            goal: Goal::DropAll,
            user: None,
            allow_root: false,
        };
        let p = plan::plan(&before.credentials(), &request).unwrap();
        match apply::apply(&p, proc().as_fd()) {
            Err(apply::ApplyError::MultiThreaded(n)) if n > 1 => 0,
            other => {
                eprintln!("unexpected: {other:?}");
                1
            }
        }
    });
    assert_eq!(code, 0);
}

#[test]
#[ignore = "requires a user namespace; run scripts/integration-tests.sh"]
fn process_state_refuses_a_file_mounted_over_proc_status() {
    require_test_namespace();
    let code = in_child(|| {
        // SAFETY: the child is single-threaded; a private mount namespace
        // keeps the overmount from outliving it.
        assert_eq!(unsafe { libc::unshare(libc::CLONE_NEWNS) }, 0, "unshare");
        rustix::mount::mount_change(
            "/",
            MountPropagationFlags::PRIVATE | MountPropagationFlags::REC,
        )
        .unwrap();
        let fake = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(fake.path(), "Threads:\t1\n").unwrap();
        rustix::mount::mount_bind(fake.path(), "/proc/self/status").unwrap();
        let by_path = std::fs::read_to_string("/proc/self/status").unwrap();
        assert_eq!(by_path, "Threads:\t1\n", "overmount not in effect");

        let proc = proc();
        match ProcessState::read_self(proc.as_fd()) {
            Err(e) if e.raw_os_error() == Some(libc::EXDEV) => 0,
            other => {
                eprintln!("unexpected: {other:?}");
                1
            }
        }
    });
    assert_eq!(code, 0);
}
