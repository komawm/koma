//! Socket-activated `xwayland-satellite` supervisor & `DISPLAY` allocator
//! (modeled directly on `niri/src/utils/xwayland/{mod,satellite}.rs`).
//!
//! Architecture & Security Invariants:
//! 1. **Zero Startup Cost**: Binds `/tmp/.X<N>-lock`, `/tmp/.X11-unix/X<N>`, and Linux abstract
//!    socket `@/tmp/.X11-unix/X<N>` passively on `calloop` (`Interest::READ`), exporting `DISPLAY=:<N>`
//!    immediately at `t = 0 ms` without spawning `xwayland-satellite` until an X11 app actually connects!
//! 2. **Nested & Bare-Metal Safe**: Explicitly passes `WAYLAND_DISPLAY=<koma_wayland_socket>` and
//!    strips `DISPLAY` when spawning `xwayland-satellite`, preventing X11 clients from escaping to
//!    a host compositor during `--winit` nested sessions.
//! 3. **Crash Self-Healing & Busy-Loop Guard**: Monitors `child.wait()` on a background thread and
//!    drains unaccepted socket backlog (`clear_out_pending_connections`) before re-arming `calloop`
//!    watches via `calloop::channel` if `xwayland-satellite` exits unexpectedly.
//! 4. **Per-Workspace Headless X11 Isolation**: Supports allocating dedicated `:M` `xwayland-satellite`
//!    slots for isolated `Headless` Agent workspaces (`ws.spawn_bound`) so legacy X11 office/CAD apps
//!    never share an X11 root window or X11 clipboard with the human's primary `Headed` `:N` display!

use std::{
    ffi::OsString,
    os::{
        fd::{AsRawFd as _, BorrowedFd, OwnedFd},
        unix::{
            net::{SocketAddr, UnixListener},
            process::CommandExt as _,
        },
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
};

use anyhow::{Context as _, anyhow, ensure};
use calloop::{
    Interest, LoopHandle, Mode, PostAction, RegistrationToken,
    channel::{self, Sender},
    generic::Generic,
};
use koma_proto::WorkspaceId;
use rustix::{
    fs::{OFlags, lstat, mkdir, open, unlink},
    io::{Errno, FdFlags, fcntl_setfd, write},
    process::{getpid, getuid},
};
use tracing::{debug, info, warn};

use crate::state::KomaState;

const TMP_UNIX_DIR: &str = "/tmp";
const X11_TMP_UNIX_DIR: &str = "/tmp/.X11-unix";

/// Identifies whether an `xwayland-satellite` instance serves the primary desktop or an isolated workspace.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SatelliteTarget {
    Primary,
    Workspace(WorkspaceId),
}

enum SatelliteToMain {
    RearmWatch(SatelliteTarget),
}

struct UnlinkGuard(String);

impl Drop for UnlinkGuard {
    fn drop(&mut self) {
        let _ = unlink(self.0.as_str());
    }
}

/// Passive X11 `:N` display lock & dual Unix/Abstract socket pair (`niri/src/utils/xwayland/mod.rs`).
pub struct X11SocketSlot {
    pub display_number: u32,
    pub display_name: String,
    pub abstract_fd: Option<OwnedFd>,
    pub unix_fd: OwnedFd,
    _unix_guard: UnlinkGuard,
    _lock_guard: UnlinkGuard,
}

/// Live `calloop`-watched `xwayland-satellite` slot.
pub struct SatelliteSlot {
    pub target: SatelliteTarget,
    pub binary_path: PathBuf,
    pub wayland_socket_name: OsString,
    pub x11: X11SocketSlot,
    abstract_token: Option<RegistrationToken>,
    unix_token: Option<RegistrationToken>,
    to_main: Sender<SatelliteToMain>,
}

impl SatelliteSlot {
    #[must_use]
    pub fn display_name(&self) -> &str {
        &self.x11.display_name
    }
}

/// Initializes the primary `xwayland-satellite` socket-activation watch on `KomaState` if the
/// binary is installed and supports `-listenfd` (`--test-listenfd-support`).
pub fn setup_primary_satellite(state: &mut KomaState) {
    if state.primary_satellite.is_some() {
        return;
    }

    let xwl_cfg = state.lua_sandbox.config_snapshot().xwayland;
    if !xwl_cfg.enabled {
        info!("xwayland-satellite disabled by Lua configuration");
        return;
    }

    let bin_path = expand_tilde_path(&xwl_cfg.path);
    if !probe_listenfd_support(&bin_path) {
        info!(
            binary = %bin_path.display(),
            "xwayland-satellite not found or lacks -listenfd support; running Wayland-only"
        );
        return;
    }

    let Some(loop_handle) = state.loop_handle.clone() else {
        return;
    };

    let x11 = match allocate_x11_socket_slot(0) {
        Ok(slot) => slot,
        Err(err) => {
            warn!("failed allocating X11 display socket for xwayland-satellite: {err:#}");
            return;
        }
    };

    let (tx, rx) = channel::channel::<SatelliteToMain>();
    if let Err(err) = loop_handle.insert_source(rx, |event, _, state| {
        if let channel::Event::Msg(SatelliteToMain::RearmWatch(target)) = event {
            rearm_satellite_watch(state, &target);
        }
    }) {
        warn!("failed registering xwayland-satellite channel on calloop: {err}");
        return;
    }

    let display_name = x11.display_name.clone();
    state.primary_satellite = Some(SatelliteSlot {
        target: SatelliteTarget::Primary,
        binary_path: bin_path,
        wayland_socket_name: state.socket_name.clone(),
        x11,
        abstract_token: None,
        unix_token: None,
        to_main: tx,
    });

    rearm_satellite_watch(state, &SatelliteTarget::Primary);

    info!(
        x11_display = %display_name,
        wayland_display = ?state.socket_name,
        "xwayland-satellite socket activation armed (zero-cost until first X11 client connects)"
    );
}

/// Allocates (or returns the existing) isolated `xwayland-satellite` `:M` display slot for a
/// `Headless` Agent workspace when `isolate_agent_workspaces` is enabled.
pub fn ensure_workspace_satellite(state: &mut KomaState, ws_id: &WorkspaceId) -> Option<String> {
    if let Some(existing) = state.workspace_satellites.get(ws_id) {
        return Some(existing.display_name().to_owned());
    }

    let xwl_cfg = state.lua_sandbox.config_snapshot().xwayland;
    if !xwl_cfg.enabled {
        return None;
    }
    if !xwl_cfg.isolate_agent_workspaces {
        return state
            .primary_satellite
            .as_ref()
            .map(|s| s.display_name().to_owned());
    }

    let bin_path = expand_tilde_path(&xwl_cfg.path);
    if !probe_listenfd_support(&bin_path) {
        return state
            .primary_satellite
            .as_ref()
            .map(|s| s.display_name().to_owned());
    }

    let loop_handle = state.loop_handle.clone()?;
    let start_num = state
        .primary_satellite
        .as_ref()
        .map_or(10, |s| s.x11.display_number + 1);
    let x11 = allocate_x11_socket_slot(start_num).ok()?;
    let display_name = x11.display_name.clone();

    let to_main = if let Some(prim) = &state.primary_satellite {
        prim.to_main.clone()
    } else {
        let (tx, rx) = channel::channel::<SatelliteToMain>();
        loop_handle
            .insert_source(rx, |event, _, state| {
                if let channel::Event::Msg(SatelliteToMain::RearmWatch(target)) = event {
                    rearm_satellite_watch(state, &target);
                }
            })
            .ok()?;
        tx
    };

    let target = SatelliteTarget::Workspace(ws_id.clone());
    state.workspace_satellites.insert(
        ws_id.clone(),
        SatelliteSlot {
            target: target.clone(),
            binary_path: bin_path,
            wayland_socket_name: state.socket_name.clone(),
            x11,
            abstract_token: None,
            unix_token: None,
            to_main,
        },
    );
    rearm_satellite_watch(state, &target);

    info!(%ws_id, x11_display = %display_name, "armed isolated workspace xwayland-satellite slot");
    Some(display_name)
}

fn rearm_satellite_watch(state: &mut KomaState, target: &SatelliteTarget) {
    let Some(loop_handle) = state.loop_handle.clone() else {
        return;
    };
    let slot = match target {
        SatelliteTarget::Primary => state.primary_satellite.as_mut(),
        SatelliteTarget::Workspace(ws_id) => state.workspace_satellites.get_mut(ws_id),
    };
    let Some(satellite) = slot else {
        return;
    };

    if let Some(tok) = satellite.abstract_token.take() {
        loop_handle.remove(tok);
    }
    if let Some(tok) = satellite.unix_token.take() {
        loop_handle.remove(tok);
    }

    if let Some(abs_fd) = &satellite.x11.abstract_fd {
        if let Ok(cloned) = abs_fd.try_clone() {
            let drained = clear_out_pending_connections(cloned);
            let target_cb = target.clone();
            if let Ok(tok) = loop_handle.insert_source(
                Generic::new(drained, Interest::READ, Mode::Level),
                move |_, _, state| {
                    trigger_satellite_on_demand(state, &target_cb, true);
                    Ok(PostAction::Remove)
                },
            ) {
                satellite.abstract_token = Some(tok);
            }
        }
    }

    if let Ok(cloned) = satellite.x11.unix_fd.try_clone() {
        let drained = clear_out_pending_connections(cloned);
        let target_cb = target.clone();
        if let Ok(tok) = loop_handle.insert_source(
            Generic::new(drained, Interest::READ, Mode::Level),
            move |_, _, state| {
                trigger_satellite_on_demand(state, &target_cb, false);
                Ok(PostAction::Remove)
            },
        ) {
            satellite.unix_token = Some(tok);
        }
    }
}

fn trigger_satellite_on_demand(
    state: &mut KomaState,
    target: &SatelliteTarget,
    from_abstract: bool,
) {
    let Some(loop_handle) = state.loop_handle.clone() else {
        return;
    };
    let slot = match target {
        SatelliteTarget::Primary => state.primary_satellite.as_mut(),
        SatelliteTarget::Workspace(ws_id) => state.workspace_satellites.get_mut(ws_id),
    };
    let Some(satellite) = slot else {
        return;
    };

    if from_abstract {
        if let Some(other) = satellite.unix_token.take() {
            loop_handle.remove(other);
        }
        satellite.abstract_token = None;
    } else {
        if let Some(other) = satellite.abstract_token.take() {
            loop_handle.remove(other);
        }
        satellite.unix_token = None;
    }

    debug!(
        x11_display = %satellite.x11.display_name,
        from_abstract,
        "X11 client connection detected; spawning xwayland-satellite on demand"
    );
    spawn_satellite_supervisor_thread(&loop_handle, satellite);
}

fn spawn_satellite_supervisor_thread(
    _loop_handle: &LoopHandle<'static, KomaState>,
    satellite: &SatelliteSlot,
) {
    let abstract_fd = satellite
        .x11
        .abstract_fd
        .as_ref()
        .and_then(|fd| fd.try_clone().ok());
    let Ok(unix_fd) = satellite.x11.unix_fd.try_clone() else {
        warn!("failed cloning X11 unix_fd for xwayland-satellite");
        return;
    };

    let path = satellite.binary_path.clone();
    let display_name = satellite.x11.display_name.clone();
    let wayland_socket = satellite.wayland_socket_name.clone();
    let target = satellite.target.clone();
    let to_main = satellite.to_main.clone();

    let mut process = Command::new(&path);
    process
        .arg(&display_name)
        .env("WAYLAND_DISPLAY", &wayland_socket)
        .env_remove("DISPLAY")
        .env_remove("RUST_BACKTRACE")
        .env_remove("RUST_LIB_BACKTRACE")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let spawn_res = thread::Builder::new()
        .name(format!("koma-xwl-{display_name}"))
        .spawn(move || {
            spawn_and_wait_listenfd(&path, process, abstract_fd, unix_fd);
            // Re-arm passive socket activation if xwayland-satellite exits or crashes (`satellite.rs` L224)
            let _ = to_main.send(SatelliteToMain::RearmWatch(target));
        });

    if let Err(err) = spawn_res {
        warn!("failed spawning xwayland-satellite supervisor thread: {err}");
        let _ = satellite
            .to_main
            .send(SatelliteToMain::RearmWatch(satellite.target.clone()));
    }
}

fn spawn_and_wait_listenfd(
    path: &Path,
    mut process: Command,
    abstract_fd: Option<OwnedFd>,
    unix_fd: OwnedFd,
) {
    let abstract_raw = abstract_fd.as_ref().map(|fd| fd.as_raw_fd());
    let unix_raw = unix_fd.as_raw_fd();

    process.arg("-listenfd").arg(unix_raw.to_string());
    if let Some(abs_raw) = abstract_raw {
        process.arg("-listenfd").arg(abs_raw.to_string());
    }

    // SAFETY: Inside forked child right before `execve`, clear `FD_CLOEXEC` ONLY on the two
    // X11 listener FDs handed to `xwayland-satellite` (`niri/src/utils/xwayland/satellite.rs` L250).
    unsafe {
        process.pre_exec(move || {
            let u_fd = BorrowedFd::borrow_raw(unix_raw);
            fcntl_setfd(u_fd, FdFlags::empty())
                .map_err(|e| std::io::Error::from_raw_os_error(e.raw_os_error()))?;
            if let Some(abs_raw) = abstract_raw {
                let a_fd = BorrowedFd::borrow_raw(abs_raw);
                fcntl_setfd(a_fd, FdFlags::empty())
                    .map_err(|e| std::io::Error::from_raw_os_error(e.raw_os_error()))?;
            }
            Ok(())
        });
    }

    let mut child = match process.spawn() {
        Ok(c) => c,
        Err(err) => {
            warn!(binary = %path.display(), "failed spawning xwayland-satellite: {err}");
            return;
        }
    };

    // Parent drops its duplicated handles immediately after spawn
    drop(abstract_fd);
    drop(unix_fd);

    match child.wait() {
        Ok(status) => warn!(binary = %path.display(), %status, "xwayland-satellite exited"),
        Err(err) => warn!(binary = %path.display(), "error waiting on xwayland-satellite: {err}"),
    }
}

// ==============================================================================
// Low-Level X11 `/tmp/.X11-unix` Directory Verification & Lock Allocation
// (`niri/src/utils/xwayland/mod.rs`, adapted from GNOME Mutter)
// ==============================================================================

pub fn allocate_x11_socket_slot(start_display: u32) -> anyhow::Result<X11SocketSlot> {
    ensure_x11_unix_dir()?;

    let mut n = start_display;
    for attempt in 0..50 {
        let (display, lock_fd, lock_guard) = pick_x11_display(n)?;

        let pid_string = format!(
            "{:>10}
",
            getpid().as_raw_nonzero()
        );
        write(&lock_fd, pid_string.as_bytes())
            .map_err(|e| anyhow!("failed writing PID to /tmp/.X{display}-lock: {e}"))?;
        drop(lock_fd);

        match open_display_sockets(display) {
            Ok((abs_listener, unix_listener, unix_guard)) => {
                return Ok(X11SocketSlot {
                    display_number: display,
                    display_name: format!(":{display}"),
                    abstract_fd: abs_listener.map(OwnedFd::from),
                    unix_fd: OwnedFd::from(unix_listener),
                    _unix_guard: unix_guard,
                    _lock_guard: lock_guard,
                });
            }
            Err(err) if attempt < 49 => {
                debug!(
                    display,
                    "X11 socket bind race on :{display}, retrying next ({err})"
                );
                n = display + 1;
            }
            Err(err) => {
                return Err(err).context("exhausted 50 attempts binding X11 display sockets");
            }
        }
    }

    Err(anyhow!("unable to allocate free X11 display slot"))
}

fn ensure_x11_unix_dir() -> anyhow::Result<()> {
    match mkdir(X11_TMP_UNIX_DIR, 0o1777.into()) {
        Ok(()) => Ok(()),
        Err(Errno::EXIST) => ensure_x11_unix_perms(),
        Err(err) => Err(anyhow!("failed creating {X11_TMP_UNIX_DIR}: {err}")),
    }
}

fn ensure_x11_unix_perms() -> anyhow::Result<()> {
    let x11_stat = lstat(X11_TMP_UNIX_DIR).context("lstat /tmp/.X11-unix failed")?;
    let tmp_stat = lstat(TMP_UNIX_DIR).context("lstat /tmp failed")?;

    ensure!(
        x11_stat.st_uid == tmp_stat.st_uid || x11_stat.st_uid == getuid().as_raw(),
        "unsafe ownership on /tmp/.X11-unix"
    );
    ensure!(
        (x11_stat.st_mode & 0o022) == 0o022,
        "/tmp/.X11-unix is not world-writable"
    );
    ensure!(
        (x11_stat.st_mode & 0o1000) == 0o1000,
        "/tmp/.X11-unix is missing sticky bit (01000)"
    );
    Ok(())
}

fn pick_x11_display(start: u32) -> anyhow::Result<(u32, OwnedFd, UnlinkGuard)> {
    for n in start..start + 50 {
        let lock_path = format!("/tmp/.X{n}-lock");
        let flags = OFlags::WRONLY | OFlags::CLOEXEC | OFlags::CREATE | OFlags::EXCL;
        if let Ok(lock_fd) = open(lock_path.as_str(), flags, 0o444.into()) {
            return Ok((n, lock_fd, UnlinkGuard(lock_path)));
        }
    }
    Err(anyhow!(
        "no free X11 display lock found in range {start}..{}",
        start + 50
    ))
}

fn open_display_sockets(
    display: u32,
) -> anyhow::Result<(Option<UnixListener>, UnixListener, UnlinkGuard)> {
    #[cfg(target_os = "linux")]
    let abs = Some(bind_abstract_x11_socket(display)?);
    #[cfg(not(target_os = "linux"))]
    let abs = None;

    let (unix_sock, guard) = bind_pathname_x11_socket(display)?;
    Ok((abs, unix_sock, guard))
}

#[cfg(target_os = "linux")]
fn bind_abstract_x11_socket(display: u32) -> anyhow::Result<UnixListener> {
    use std::os::linux::net::SocketAddrExt as _;
    let name = format!("/tmp/.X11-unix/X{display}");
    let addr = SocketAddr::from_abstract_name(name).context("invalid abstract X11 socket name")?;
    UnixListener::bind_addr(&addr).context("failed binding abstract X11 socket")
}

fn bind_pathname_x11_socket(display: u32) -> anyhow::Result<(UnixListener, UnlinkGuard)> {
    let path = format!("/tmp/.X11-unix/X{display}");
    let addr = SocketAddr::from_pathname(&path).context("invalid pathname X11 socket")?;
    let _ = unlink(path.as_str());
    let guard = UnlinkGuard(path);
    let listener = UnixListener::bind_addr(&addr).context("failed binding pathname X11 socket")?;
    Ok((listener, guard))
}

fn clear_out_pending_connections(fd: OwnedFd) -> OwnedFd {
    let listener = UnixListener::from(fd);
    if listener.set_nonblocking(true).is_err() {
        return OwnedFd::from(listener);
    }
    while listener.accept().is_ok() {}
    let _ = listener.set_nonblocking(false);
    OwnedFd::from(listener)
}

fn probe_listenfd_support(path: &Path) -> bool {
    let mut cmd = Command::new(path);
    cmd.args([":0", "--test-listenfd-support"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_remove("DISPLAY")
        .env_remove("RUST_BACKTRACE")
        .env_remove("RUST_LIB_BACKTRACE");

    cmd.spawn()
        .and_then(|mut child| child.wait())
        .is_ok_and(|status| status.success())
}

fn expand_tilde_path(raw: &str) -> PathBuf {
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(raw)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn allocates_and_cleans_up_x11_lock_and_sockets_via_raii() {
        let slot = allocate_x11_socket_slot(40).expect("must allocate X11 display slot >= :40");
        let num = slot.display_number;
        assert!(num >= 40);
        assert_eq!(slot.display_name, format!(":{num}"));
        assert!(slot.abstract_fd.is_some());

        let lock_file = format!("/tmp/.X{num}-lock");
        let sock_file = format!("/tmp/.X11-unix/X{num}");
        assert!(Path::new(&lock_file).exists());
        assert!(Path::new(&sock_file).exists());

        drop(slot);
        assert!(
            !Path::new(&lock_file).exists(),
            "UnlinkGuard must remove /tmp/.X<N>-lock"
        );
        assert!(
            !Path::new(&sock_file).exists(),
            "UnlinkGuard must remove /tmp/.X11-unix/X<N>"
        );
    }
}
