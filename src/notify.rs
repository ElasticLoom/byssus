//! systemd service notifications (`sd_notify` protocol).
//!
//! When started by systemd with `Type=notify`, `NOTIFY_SOCKET` names a Unix
//! datagram socket. `byssusd` reports readiness only after its startup
//! reconcile, so units ordered after it (for example container runtimes) start
//! once views are populated. Without `NOTIFY_SOCKET` every call is a no-op.

use std::ffi::OsStr;
use std::io;
use std::os::linux::net::SocketAddrExt;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::{SocketAddr, UnixDatagram};

/// Sends notifications to the service manager, if there is one.
#[derive(Debug)]
pub struct Notifier {
    target: Option<(UnixDatagram, SocketAddr)>,
    last_status: std::cell::RefCell<String>,
}

impl Notifier {
    /// A notifier that never sends anything.
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            target: None,
            last_status: std::cell::RefCell::default(),
        }
    }

    /// Uses `NOTIFY_SOCKET` from the environment, if set.
    pub fn from_env() -> io::Result<Self> {
        match std::env::var_os("NOTIFY_SOCKET") {
            None => Ok(Self::disabled()),
            Some(value) => Self::for_socket(value.as_encoded_bytes()),
        }
    }

    /// Uses the given `NOTIFY_SOCKET` value: an absolute path, or `@name` for
    /// an abstract socket.
    pub fn for_socket(value: &[u8]) -> io::Result<Self> {
        let addr = match value.split_first() {
            Some((b'/', _)) => {
                SocketAddr::from_pathname(std::path::Path::new(OsStr::from_bytes(value)))?
            }
            Some((b'@', name)) => SocketAddr::from_abstract_name(name)?,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "NOTIFY_SOCKET must be an absolute path or begin with '@'",
                ));
            }
        };
        Ok(Self {
            target: Some((UnixDatagram::unbound()?, addr)),
            last_status: std::cell::RefCell::default(),
        })
    }

    /// Whether notifications are being sent.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.target.is_some()
    }

    fn send(&self, message: &str) {
        if let Some((socket, addr)) = &self.target {
            if let Err(e) = socket.send_to_addr(message.as_bytes(), addr) {
                tracing::warn!(msg = "cannot notify service manager", error = %e);
            }
        }
    }

    /// Startup (or a reload) is complete.
    pub fn ready(&self, status: &str) {
        let status = one_line(status);
        self.send(&format!("READY=1\nSTATUS={status}"));
        *self.last_status.borrow_mut() = status;
    }

    /// A configuration reload has started.
    pub fn reloading(&self) {
        let now = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
        let usec = u64::try_from(now.tv_sec).unwrap_or(0) * 1_000_000
            + u64::try_from(now.tv_nsec).unwrap_or(0) / 1_000;
        self.send(&format!(
            "RELOADING=1\nMONOTONIC_USEC={usec}\nSTATUS=Reloading configuration"
        ));
    }

    /// The daemon is shutting down.
    pub fn stopping(&self) {
        self.send("STOPPING=1\nSTATUS=Stopping; mounts are preserved");
    }

    /// Updates the status line if it changed.
    pub fn status(&self, status: &str) {
        let status = one_line(status);
        if *self.last_status.borrow() != status {
            self.send(&format!("STATUS={status}"));
            *self.last_status.borrow_mut() = status;
        }
    }
}

fn one_line(s: &str) -> String {
    s.replace(['\n', '\r'], " ")
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn receive(socket: &UnixDatagram) -> String {
        let mut buf = [0u8; 1024];
        let n = socket.recv(&mut buf).unwrap();
        String::from_utf8(buf[..n].to_vec()).unwrap()
    }

    #[test]
    fn sends_to_path_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notify");
        let listener = UnixDatagram::bind(&path).unwrap();
        listener
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();

        let n = Notifier::for_socket(path.as_os_str().as_encoded_bytes()).unwrap();
        assert!(n.is_enabled());
        n.ready("2 groups\nand more");
        assert_eq!(receive(&listener), "READY=1\nSTATUS=2 groups and more");
        n.status("2 groups and more");
        n.status("3 groups");
        assert_eq!(receive(&listener), "STATUS=3 groups");
        n.reloading();
        let msg = receive(&listener);
        assert!(msg.starts_with("RELOADING=1\nMONOTONIC_USEC="), "{msg}");
        n.stopping();
        assert!(receive(&listener).starts_with("STOPPING=1"));
    }

    #[test]
    fn sends_to_abstract_socket() {
        let name = format!("byssus-notify-test-{}", std::process::id());
        let addr = SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
        let listener = UnixDatagram::bind_addr(&addr).unwrap();
        listener
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let n = Notifier::for_socket(format!("@{name}").as_bytes()).unwrap();
        n.ready("ok");
        assert_eq!(receive(&listener), "READY=1\nSTATUS=ok");
    }

    #[test]
    fn disabled_and_invalid() {
        let n = Notifier::disabled();
        assert!(!n.is_enabled());
        n.ready("nothing happens");
        assert!(Notifier::for_socket(b"relative").is_err());
        assert!(Notifier::for_socket(b"").is_err());
    }
}
