//! Shared fixtures for privileged tests.

use std::fs;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};

use rustix::mount::{MountFlags, MountPropagationFlags, UnmountFlags};

/// Panics unless running inside the namespace set up by
/// `scripts/integration-tests.sh`.
pub fn require_test_namespace() {
    assert_eq!(
        std::env::var("BYSSUS_TEST_NAMESPACE").as_deref(),
        Ok("1"),
        "privileged tests must be run via scripts/integration-tests.sh"
    );
    let uid_map = fs::read_to_string("/proc/self/uid_map").expect("read uid_map");
    let identity = uid_map.split_whitespace().collect::<Vec<_>>() == ["0", "0", "4294967295"];
    assert!(
        !identity,
        "refusing to run privileged tests in the initial user namespace"
    );
}

/// Opens and verifies `/proc`.
pub fn proc() -> OwnedFd {
    byssus::probe::verify_procfs(Path::new("/proc")).expect("verify /proc")
}

/// A private tmpfs sandbox. Everything a test mounts lives beneath it, and
/// the tmpfs is detached when the sandbox is dropped.
pub struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    pub fn new() -> Self {
        require_test_namespace();
        let dir = tempfile::tempdir().expect("tempdir");
        rustix::mount::mount(
            "byssus-test",
            dir.path(),
            "tmpfs",
            MountFlags::empty(),
            Some(c"mode=0755"),
        )
        .expect("mount sandbox tmpfs");
        Self { dir }
    }

    pub fn path(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    pub fn mkdir(&self, rel: &str) -> PathBuf {
        let p = self.path(rel);
        fs::create_dir_all(&p).expect("create dir");
        p
    }

    pub fn write(&self, rel: &str, contents: &str) -> PathBuf {
        let p = self.path(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).expect("create parent");
        }
        fs::write(&p, contents).expect("write file");
        p
    }

    pub fn open_root(&self, rel: &str) -> OwnedFd {
        byssus::fsops::open_root(&self.path(rel)).expect("open root")
    }

    /// Turns `rel` into its own mount point with shared propagation.
    pub fn make_shared_anchor(&self, rel: &str) -> PathBuf {
        let p = self.mkdir(rel);
        rustix::mount::mount_bind(&p, &p).expect("bind anchor");
        rustix::mount::mount_change(&p, MountPropagationFlags::SHARED).expect("make shared");
        p
    }

    /// Simulates a container: binds `view` at `consumer` with slave
    /// propagation (what `rslave` gives a container).
    pub fn attach_consumer(&self, view: &Path, consumer_rel: &str) -> PathBuf {
        let c = self.mkdir(consumer_rel);
        rustix::mount::mount_bind(view, &c).expect("bind consumer");
        rustix::mount::mount_change(
            &c,
            MountPropagationFlags::DOWNSTREAM | MountPropagationFlags::REC,
        )
        .expect("make slave");
        c
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = rustix::mount::unmount(self.dir.path(), UnmountFlags::DETACH);
    }
}

pub fn errno_of(err: &std::io::Error) -> i32 {
    err.raw_os_error().unwrap_or(0)
}
