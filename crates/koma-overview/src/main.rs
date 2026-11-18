#![forbid(unsafe_code)]

//! Standalone `wlr-layer-shell` companion daemon & CLI trigger for `Koma` (`ARCHITECTURE.md` §3.3).
//!
//! Commands:
//! - `koma-overview --daemon` : Runs the persistent `calloop` + `zwlr_layer_shell_v1` daemon
//! - `koma-overview hover <ws_id> <anchor_x> [bar_height]` : Triggers 150ms status-bar hover popup
//! - `koma-overview leave` : Cancels hover dwell or hides active thumbnail popup
//! - `koma-overview toggle` : Toggles the full-screen live multi-workspace overview grid (`Mod + Tab`)
//!
//! `toggle` self-heals: when the control socket has no listener it starts one daemon (single-flight,
//! bounded by a shared respawn budget) and retries the command once, so `Mod + Tab` recovers from a
//! daemon that died instead of failing with `ECONNREFUSED` forever (komawm/koma#75).

pub mod compositor_card;
pub mod grid_layout;
pub mod hover_fsm;
pub mod ipc_bridge;
pub mod lifecycle;

#[cfg(feature = "wayland-layer-shell")]
pub mod wayland_layer;

use std::{env, panic::PanicHookInfo};

use tracing::error;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    install_panic_hook();

    let args: Vec<String> = env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--daemon") | None => run_daemon_command(),
        Some("hover") => {
            let ws = args.get(1).map(String::as_str).unwrap_or("ws_01");
            let x = args.get(2).map(String::as_str).unwrap_or("16");
            let bar_h = args.get(3).map(String::as_str).unwrap_or("32");
            let cmd = format!("hover {ws} {x} {bar_h}");
            if let Err(e) = ipc_bridge::send_daemon_control_line(&cmd) {
                eprintln!("failed notifying koma-overview daemon: {e}");
                std::process::exit(1);
            }
        }
        Some("leave") => {
            let _ = ipc_bridge::send_daemon_control_line("leave");
        }
        Some("toggle") => run_toggle_command(),
        _ => {
            eprintln!(
                "Usage:
  koma-overview --daemon
  koma-overview hover <ws_id> <anchor_x> [bar_height]
  koma-overview leave
  koma-overview toggle"
            );
        }
    }
}

/// Reports the daemon's exit reason on stderr and in the journal, then exits with the matching code.
///
/// Every path out of [`wayland_layer::run_overview_daemon`] is described by a
/// [`wayland_layer::DaemonExit`], so a daemon that stops never does so silently again.
#[cfg(feature = "wayland-layer-shell")]
fn run_daemon_command() {
    use wayland_layer::DaemonExit;

    let reason = wayland_layer::run_overview_daemon();
    match &reason {
        DaemonExit::ControlQuit { .. } => {
            tracing::info!("koma-overview daemon exiting: {}", reason.describe());
        }
        _ => {
            error!("koma-overview daemon exiting: {}", reason.describe());
            eprintln!("koma-overview daemon error: {}", reason.describe());
        }
    }
    std::process::exit(reason.exit_code());
}

/// Explains why `wayland-layer-shell` was compiled out instead of exiting without a word.
#[cfg(not(feature = "wayland-layer-shell"))]
fn run_daemon_command() {
    eprintln!("koma-overview built without `wayland-layer-shell` feature");
}

/// `toggle` with one self-healing retry against a dead control socket.
fn run_toggle_command() {
    let paths = match ipc_bridge::runtime_lifecycle_paths() {
        Ok(paths) => paths,
        Err(err) => {
            eprintln!("failed toggling koma-overview daemon: {err}");
            std::process::exit(1);
        }
    };
    let mut spawner = lifecycle::CurrentExeSpawner::detect();
    let mut waiter = lifecycle::ThreadWaiter;
    let delivery = lifecycle::send_with_recovery(
        &paths,
        "toggle",
        &mut spawner,
        &mut waiter,
        lifecycle::RecoveryPolicy::default(),
    );
    if let Some(message) = delivery.failure_message() {
        eprintln!("failed toggling koma-overview daemon: {message}");
        std::process::exit(1);
    }
}

/// Routes panics to the journal (and stderr) instead of letting the process vanish.
///
/// `koma-overview` is spawned with the compositor's inherited stderr, so both `tracing` and the
/// default hook reach journald. The panic is also logged explicitly because the tracing subscriber
/// may be filtered above `error` in a user configuration.
fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info: &PanicHookInfo<'_>| {
        let payload = panic_payload(info);
        let location = info
            .location()
            .map_or_else(|| "<unknown>".to_owned(), ToString::to_string);
        error!(panic = %payload, location = %location, "koma-overview panicked");
        eprintln!("koma-overview panic at {location}: {payload}");
        default_hook(info);
    }));
}

/// Extracts a printable message from a panic payload.
fn panic_payload(info: &PanicHookInfo<'_>) -> String {
    message_from_payload(info.payload())
}

/// Renders the `&str`/`String` payload forms a panic can carry.
fn message_from_payload(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panic_payload_is_readable_for_both_string_forms() {
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let static_message = std::panic::catch_unwind(|| panic!("static payload"))
            .expect_err("catch the panic payload");
        let owned_message = std::panic::catch_unwind(|| panic!("{}", "owned payload".to_owned()))
            .expect_err("catch the panic payload");
        std::panic::set_hook(hook);

        assert_eq!(
            message_from_payload(static_message.as_ref()),
            "static payload"
        );
        assert_eq!(
            message_from_payload(owned_message.as_ref()),
            "owned payload"
        );
        assert_eq!(message_from_payload(&7_u32), "non-string panic payload");
    }
}
