//! Safe child-process environment overrides without mutating process-global `environ`.
//!
//! Tracks session launch flags (`--session`), `WAYLAND_DISPLAY`, XWayland `DISPLAY`, and
//! `KOMA_INSTANCE_SIGNATURE` in process state and applies them explicitly via
//! `std::process::Command::env` / `env_remove` on every child process spawned by Koma.

use std::{
    ffi::{OsStr, OsString},
    process::Command,
    sync::OnceLock,
};

/// Compositor-level environment values inherited by spawned desktop/agent child processes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionChildEnv {
    pub is_session: bool,
    pub wayland_display: OsString,
    pub x11_display: Option<String>,
    pub instance_signature: Option<String>,
}

static SESSION_MODE: OnceLock<bool> = OnceLock::new();
static COMPOSITOR_CHILD_ENV: OnceLock<SessionChildEnv> = OnceLock::new();

/// Records whether Koma was started with `--session`.
pub fn init_session_mode(is_session: bool) {
    let _ = SESSION_MODE.set(is_session);
}

/// Returns whether Koma was started with `--session`.
#[must_use]
pub fn is_session_mode() -> bool {
    SESSION_MODE.get().copied().unwrap_or(false)
}

/// Publishes the finalized compositor socket/display/signature metadata for background helpers
/// (such as the `xwayland-satellite` supervisor thread) without mutating `environ`.
pub fn publish_compositor_env(env: SessionChildEnv) {
    let _ = COMPOSITOR_CHILD_ENV.set(env);
}

/// Applies base compositor environment overrides (`WAYLAND_DISPLAY`, `DISPLAY` or removal,
/// `KOMA_INSTANCE_SIGNATURE`, `--session` XDG variables, and `NOTIFY_FD`/`WAYLAND_SOCKET`
/// scrubbing) onto `cmd`.
pub fn apply_base_compositor_env_to_command(
    cmd: &mut Command,
    is_session: bool,
    wayland_display: &OsStr,
    x11_display: Option<&str>,
    instance_signature: Option<&str>,
) {
    cmd.env("WAYLAND_DISPLAY", wayland_display);
    cmd.env_remove("WAYLAND_SOCKET");
    cmd.env_remove("NOTIFY_FD");

    if let Some(sig) = instance_signature {
        cmd.env("KOMA_INSTANCE_SIGNATURE", sig);
    }

    if let Some(disp) = x11_display {
        cmd.env("DISPLAY", disp);
    } else {
        cmd.env_remove("DISPLAY");
    }

    if is_session {
        cmd.env("XDG_CURRENT_DESKTOP", "koma");
        cmd.env("XDG_SESSION_TYPE", "wayland");
    }
}

/// Applies session & signature environment overrides for `xwayland-satellite` without inheriting
/// stale `DISPLAY`, `WAYLAND_SOCKET`, or `NOTIFY_FD`.
pub fn apply_satellite_env_to_command(cmd: &mut Command, wayland_socket: &OsStr) {
    cmd.env("WAYLAND_DISPLAY", wayland_socket);
    cmd.env_remove("DISPLAY");
    cmd.env_remove("WAYLAND_SOCKET");
    cmd.env_remove("NOTIFY_FD");

    let session = COMPOSITOR_CHILD_ENV
        .get()
        .map_or_else(is_session_mode, |env| env.is_session);
    if session {
        cmd.env("XDG_CURRENT_DESKTOP", "koma");
        cmd.env("XDG_SESSION_TYPE", "wayland");
    }
    if let Some(sig) = COMPOSITOR_CHILD_ENV
        .get()
        .and_then(|env| env.instance_signature.as_deref())
    {
        cmd.env("KOMA_INSTANCE_SIGNATURE", sig);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_child_env_sets_and_scrubs_without_mutating_process_environ() {
        let mut cmd = Command::new("true");
        apply_base_compositor_env_to_command(
            &mut cmd,
            true,
            OsStr::new("wayland-9"),
            None,
            Some("123_wayland-9"),
        );

        let envs: Vec<(&OsStr, Option<&OsStr>)> = cmd.get_envs().collect();
        assert!(envs.contains(&(OsStr::new("WAYLAND_DISPLAY"), Some(OsStr::new("wayland-9")))));
        assert!(envs.contains(&(
            OsStr::new("KOMA_INSTANCE_SIGNATURE"),
            Some(OsStr::new("123_wayland-9"))
        )));
        assert!(envs.contains(&(OsStr::new("XDG_CURRENT_DESKTOP"), Some(OsStr::new("koma")))));
        assert!(envs.contains(&(OsStr::new("XDG_SESSION_TYPE"), Some(OsStr::new("wayland")))));
        assert!(envs.contains(&(OsStr::new("DISPLAY"), None)));
        assert!(envs.contains(&(OsStr::new("NOTIFY_FD"), None)));
        assert!(envs.contains(&(OsStr::new("WAYLAND_SOCKET"), None)));
    }

    #[test]
    fn satellite_env_keeps_display_removed_and_sets_wayland_display() {
        let mut cmd = Command::new("true");
        apply_satellite_env_to_command(&mut cmd, OsStr::new("wayland-5"));

        let envs: Vec<(&OsStr, Option<&OsStr>)> = cmd.get_envs().collect();
        assert!(envs.contains(&(OsStr::new("WAYLAND_DISPLAY"), Some(OsStr::new("wayland-5")))));
        assert!(envs.contains(&(OsStr::new("DISPLAY"), None)));
        assert!(envs.contains(&(OsStr::new("NOTIFY_FD"), None)));
        assert!(envs.contains(&(OsStr::new("WAYLAND_SOCKET"), None)));
    }
}
