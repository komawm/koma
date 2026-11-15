#![forbid(unsafe_code)]

//! Standalone `wlr-layer-shell` companion daemon & CLI trigger for `Koma` (`ARCHITECTURE.md` §3.3).
//!
//! Commands:
//! - `koma-overview --daemon` : Runs the persistent `calloop` + `zwlr_layer_shell_v1` daemon
//! - `koma-overview hover <ws_id> <anchor_x> [bar_height]` : Triggers 150ms status-bar hover popup
//! - `koma-overview leave` : Cancels hover dwell or hides active thumbnail popup
//! - `koma-overview toggle` : Toggles the full-screen live multi-workspace overview grid (`Mod + Tab`)

pub mod compositor_card;
pub mod grid_layout;
pub mod hover_fsm;
pub mod ipc_bridge;

#[cfg(feature = "wayland-layer-shell")]
pub mod wayland_layer;

use std::env;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args: Vec<String> = env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--daemon") | None => {
            #[cfg(feature = "wayland-layer-shell")]
            if let Err(err) = wayland_layer::run_overview_daemon() {
                eprintln!("koma-overview daemon error: {err:#}");
                std::process::exit(1);
            }
            #[cfg(not(feature = "wayland-layer-shell"))]
            eprintln!("koma-overview built without `wayland-layer-shell` feature");
        }
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
        Some("toggle") => {
            if let Err(e) = ipc_bridge::send_daemon_control_line("toggle") {
                eprintln!("failed toggling koma-overview daemon: {e}");
                std::process::exit(1);
            }
        }
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
