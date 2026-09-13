//! Synchronous signal handling with `signalfd`.
//!
//! Handled signals are blocked and delivered through a descriptor that the
//! event loop polls, so no asynchronous signal handlers run.

use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

/// A `signalfd` for a fixed set of blocked signals.
#[derive(Debug)]
pub struct SignalFd {
    fd: OwnedFd,
}

impl SignalFd {
    /// Blocks `signals` for the calling thread and opens a non-blocking,
    /// close-on-exec `signalfd` for them.
    ///
    /// Must be called while the process is single-threaded so that no other
    /// thread receives the signals instead.
    pub fn new(signals: &[libc::c_int]) -> io::Result<Self> {
        let mut set = MaybeUninit::<libc::sigset_t>::uninit();
        // SAFETY: `sigemptyset` initializes the set it is given a valid pointer to.
        if unsafe { libc::sigemptyset(set.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        for &signal in signals {
            // SAFETY: `set` was initialized by `sigemptyset` above.
            if unsafe { libc::sigaddset(set.as_mut_ptr(), signal) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        // SAFETY: fully initialized by `sigemptyset`/`sigaddset`.
        let set = unsafe { set.assume_init() };

        // SAFETY: `set` is a valid initialized sigset; a null old-set pointer
        // is permitted.
        let rc =
            unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &raw const set, std::ptr::null_mut()) };
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(rc));
        }

        // SAFETY: -1 requests a new descriptor; `set` is valid for reads.
        let raw =
            unsafe { libc::signalfd(-1, &raw const set, libc::SFD_CLOEXEC | libc::SFD_NONBLOCK) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `signalfd` returned a new descriptor that nothing else owns.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        Ok(Self { fd })
    }

    /// Reads one pending signal number, or `None` if none is pending.
    pub fn read(&self) -> io::Result<Option<u32>> {
        let mut info = MaybeUninit::<libc::signalfd_siginfo>::uninit();
        let size = std::mem::size_of::<libc::signalfd_siginfo>();
        // SAFETY: `info` provides `size` writable bytes; the descriptor is
        // owned by `self` and open.
        let n = unsafe { libc::read(self.fd.as_raw_fd(), info.as_mut_ptr().cast(), size) };
        if n < 0 {
            let err = io::Error::last_os_error();
            return if err.kind() == io::ErrorKind::WouldBlock {
                Ok(None)
            } else {
                Err(err)
            };
        }
        if usize::try_from(n).ok() != Some(size) {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "short read from signalfd",
            ));
        }
        // SAFETY: the kernel wrote a complete `signalfd_siginfo`.
        let info = unsafe { info.assume_init() };
        Ok(Some(info.ssi_signo))
    }
}

impl AsFd for SignalFd {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivers_blocked_signal() {
        // Tests run on multiple threads; SIGUSR2 sent to this thread only.
        let sfd = SignalFd::new(&[libc::SIGUSR2]).unwrap();
        assert_eq!(sfd.read().unwrap(), None);
        // SAFETY: `pthread_self` has no preconditions.
        let me = unsafe { libc::pthread_self() };
        // SAFETY: sending a signal to our own live thread, which has it blocked.
        let rc = unsafe { libc::pthread_kill(me, libc::SIGUSR2) };
        assert_eq!(rc, 0);
        assert_eq!(
            sfd.read().unwrap(),
            Some(u32::try_from(libc::SIGUSR2).unwrap())
        );
        assert_eq!(sfd.read().unwrap(), None);
    }
}
