//! Graceful termination on `SIGTERM`, `SIGINT` and `SIGHUP` (komawm/koma#4).
//!
//! A minimal async-signal-safe handler writes the signal number into a non-blocking self-pipe;
//! the read end is a level-triggered `calloop` source that stops the event loop. Returning from
//! `EventLoop::run` lets the normal drop order release DRM master, close the libseat session and
//! remove the Wayland and IPC sockets, so the process exits with status 0.
//!
//! Unlike `calloop::signals` (used by `niri/src/utils/signals.rs`), this does not block signals
//! with `pthread_sigmask`, so spawned clients do not inherit a modified signal mask and no
//! additional dependency (`nix`) is needed. Handlers are reset to `SIG_DFL` by `execve`, and both
//! pipe ends are `O_CLOEXEC`.

use std::{
    fs::File,
    io::{self, Read as _},
    os::fd::OwnedFd,
    sync::atomic::{AtomicI32, Ordering},
};

use anyhow::Context as _;
use calloop::{Interest, LoopHandle, Mode, PostAction, generic::Generic};
use rustix::pipe::{PipeFlags, pipe_with};
use tracing::{info, warn};

use crate::state::KomaState;

/// Signals treated as a request to shut down cleanly.
pub const TERMINATION_SIGNALS: [libc::c_int; 3] = [libc::SIGTERM, libc::SIGINT, libc::SIGHUP];

/// Write end of the self-pipe, read by the signal handler (`-1` until installed).
static PIPE_WRITE_FD: AtomicI32 = AtomicI32::new(-1);

#[allow(unsafe_code)]
extern "C" fn on_termination_signal(signum: libc::c_int) {
    let fd = PIPE_WRITE_FD.load(Ordering::Relaxed);
    if fd >= 0 {
        let byte = u8::try_from(signum).unwrap_or(u8::MAX);
        // SAFETY: `write(2)` is async-signal-safe; the pipe is non-blocking, so a full pipe
        // (already holding pending shutdown requests) drops the byte instead of blocking.
        unsafe {
            libc::write(fd, (&raw const byte).cast(), 1);
        }
    }
}

/// Human-readable name for log messages.
#[must_use]
pub fn signal_name(signum: i32) -> &'static str {
    match signum {
        libc::SIGTERM => "SIGTERM",
        libc::SIGINT => "SIGINT",
        libc::SIGHUP => "SIGHUP",
        _ => "unknown signal",
    }
}

fn create_self_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    pipe_with(PipeFlags::CLOEXEC | PipeFlags::NONBLOCK)
        .map_err(|e| io::Error::from_raw_os_error(e.raw_os_error()))
}

#[allow(unsafe_code)]
fn install_handler(signum: libc::c_int) -> io::Result<()> {
    // SAFETY: `sigaction` is zero-initializable; the handler only performs async-signal-safe work.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = on_termination_signal as *const () as libc::sighandler_t;
        action.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&raw mut action.sa_mask);
        if libc::sigaction(signum, &raw const action, std::ptr::null_mut()) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Registers the self-pipe on `calloop` and installs the termination handlers.
///
/// Must be called once per process, after the compositor state owns everything that needs to be
/// released on shutdown.
pub fn listen_for_termination(handle: &LoopHandle<'static, KomaState>) -> anyhow::Result<()> {
    let (read_end, write_end) = create_self_pipe().context("failed creating signal self-pipe")?;

    handle
        .insert_source(
            Generic::new(File::from(read_end), Interest::READ, Mode::Level),
            |_, pipe, state| {
                let mut buf = [0_u8; 16];
                let mut last = None;
                loop {
                    match (&**pipe).read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => last = buf[..n].last().copied(),
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                        Err(e) => {
                            warn!("failed reading signal self-pipe: {e}");
                            break;
                        }
                    }
                }
                if let Some(signum) = last {
                    let signum = i32::from(signum);
                    info!(
                        signal = signal_name(signum),
                        "received termination signal; stopping Koma gracefully"
                    );
                    state.loop_signal.stop();
                    state.loop_signal.wakeup();
                }
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| anyhow::anyhow!("failed registering signal self-pipe on calloop: {e}"))?;

    // The write end must outlive every possible signal delivery, so it is intentionally leaked
    // for the remaining process lifetime.
    let write_fd = std::os::fd::IntoRawFd::into_raw_fd(write_end);
    PIPE_WRITE_FD.store(write_fd, Ordering::Relaxed);

    for signum in TERMINATION_SIGNALS {
        install_handler(signum)
            .with_context(|| format!("failed installing {} handler", signal_name(signum)))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_names_cover_termination_set() {
        for signum in TERMINATION_SIGNALS {
            assert_ne!(signal_name(signum), "unknown signal");
        }
        assert_eq!(signal_name(libc::SIGUSR1), "unknown signal");
    }

    #[test]
    fn self_pipe_is_nonblocking_and_cloexec() {
        let (read_end, write_end) = create_self_pipe().expect("pipe2");
        for fd in [&read_end, &write_end] {
            let fl = rustix::fs::fcntl_getfl(fd).expect("fcntl_getfl");
            let fd_flags = rustix::io::fcntl_getfd(fd).expect("fcntl_getfd");
            assert!(fl.contains(rustix::fs::OFlags::NONBLOCK));
            assert!(fd_flags.contains(rustix::io::FdFlags::CLOEXEC));
        }
        let mut reader = File::from(read_end);
        let mut buf = [0_u8; 1];
        assert_eq!(
            reader.read(&mut buf).map_err(|e| e.kind()),
            Err(io::ErrorKind::WouldBlock)
        );
    }
}
