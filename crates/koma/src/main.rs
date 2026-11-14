//! Ring 0 Wayland compositor entry point (`koma`).
//!
//! ```text
//!                                      ╭──────╮
//!                                    ╭─╯      ╰╮
//!                                   ╭╯ K O M A ╰╮                             ╭───────────╮
//!                                 ╭─╯  WAYLAND  ╰╮                         ╭──╯ DUAL-MODE ╰─╮
//!                                ╭╯ Ring 0 Core  │                     ╭───╯  Headed+Agent  ╰╮
//!                              ╭─╯  Compositor  ╭╯                   ╭─╯    Desktop & AI     ╰╮
//!                             ╭╯   koma-model   │                 ╭──╯    Symmetrical FBO     │
//!                           ╭─╯  koma-render   ╭╯               ╭─╯   Dwindle   ╭───╮         │
//!                         ╭─╯   koma-wayland   │              ╭─╯ Scrolling  ╭──╯  ╭╯         │
//!                        ╭╯  koma-seat/vSeat  ╭╯            ╭─╯ Deck/BSP  ╭──╯   ╭─╯         ╭╯
//!                      ╭─╯ koma-lua/Phodopus  │           ╭─╯  Grid225  ╭─╯     ╭╯  15x15   ╭╯
//!                    ╭─╯ koma-ipc / koma-mcp ╭╯         ╭─╯ 9-Anchor  ╭─╯    ╭──╯  vSeat   ╭╯
//!                  ╭─╯ 240Hz KMS ╭╮ Smithay  │        ╭─╯ Hyprland  ╭─╯   ╭──╯   Epoch   ╭─╯
//!                ╭─╯ DRM/Atom  ╭─╯│ GLES/GL ╭╯       ╭╯ Keybinds  ╭─╯ ╭───╯    Lease    ╭╯
//!              ╭─╯  Libseat  ╭─╯ ╭╯ DMABUF ╭╯      ╭─╯ Overrides  ╰───╯  Shield Mode  ╭─╯
//!           ╭──╯ Udev/GPU  ╭─╯   │ Kawase  │     ╭─╯ AI-NATIVE WAYLAND COMPOSITOR  ╭──╯
//!         ╭─╯  Libinput  ╭─╯    ╭╯ Spring ╭╯    ╭╯ Headed KMS + Headless Agent  ╭──╯
//!      ╭──╯ Winit Dev  ╭─╯     ╭╯ Shadow  │    ╭╯ Configurable Lua Defaults ╭───╯
//!   ╭──╯  XWayland   ╭─╯       │ Border  ╭╯  ╭─╯ Live Shadow Arena Swap ╭───╯
//! ╭─╯  Satellite  ╭──╯        ╭╯ Dimming │  ╭╯  Protected Blackout  ╭───╯
//! │  Calloop   ╭──╯           │ HotSwap ╭╯ ╭╯ Zero-Token Macro ╭────╯
//! ╰╮        ╭──╯ Lua 5.4 VM  ╭╯        ╭╯╭─╯ Spring Motion ╭───╯
//!  ╰─╮   ╭──╯ 4MiB Sandbox  ╭╯         │╭╯ Kawase Blur ╭───╯
//!    ╰───╯   Fuel Metered   │          ╰╯ VRR Scan  ╭──╯
//!                          ╭╯ EPOCH LEASE ARBITER ╭─╯
//!                          │ Mod+G <1ms Preempt  ╭╯
//!                         ╭╯ Zero-Copy Screencap ╰╮
//!                        ╭╯ Isolated vSeat Focus  ╰╮
//!                        │  3-Tier Text Injection  ╰╮
//!                       ╭╯   Dynamic Window Rules   ╰╮
//!                       │   Multi-Finger Gestures    ╰╮
//!                      ╭╯    xdg-shell    ╭─╮         ╰╮
//!                      │   layer-shell   ╭╯ │          ╰╮
//!                     ╭╯   screencopy   ╭╯  ╰─╮         ╰╮
//!                    ╭╯   fractional   ╭╯     ╰╮         ╰─╮
//!                    │   viewporter    │       ╰╮   Jiku   ╰─╮             ╭───────────────────────╮
//!                   ╭╯   drm-lease    ╭╯        ╰─╮  Bridge  ╰─╮         ╭─╯  JIKU DESKTOP AGENT   ╰─╮
//!                   │  session-lock  ╭╯           ╰╮  MCP+CLI  ╰──╮     ╭╯ gemini-decision-1 Router  ╰╮
//!                  ╭╯  virtual-in   ╭╯             ╰─╮  komactl   ╰──╮  ╰╮  Turn SSE & Live Bidi WS   │
//!                  │   text-in-v3   │                ╰─╮  koma-mcp   ╰───╯ Structured Text Fallback  ╭╯
//!                 ╭╯ security-ctx  ╭╯                  ╰─╮     koma --session | --tty | --winit     ╭╯
//!                 │  data-control  │                     ╰──╮     komactl CLI & koma-mcp Server     │
//!                ╭╯ idle-inhibit  ╭╯                        ╰──╮  LuaLS Stubs: komactl gen-types   ╭╯
//!                │  presentation  │                            ╰────╮   ~/.config/koma/init.lua   ╭╯
//!               ╭╯ linux-dmabuf  ╭╯                                 ╰──────╮        ╭──╮  240Hz   │
//!               │  single-pixel  │                                         ╰────────╯  │ VRR/HDR ╭╯
//!              ╭╯   xdg-dialog   │                                                    ╭╯ KMS/GL ╭╯
//!              │   xdg-output   ╭╯                                                   ╭╯ Wayland │
//!              │   xdg-decor   ╭╯                                                    ╰╮ Ring 0 ╭╯
//!              ╰╮ gestures-v1 ╭╯                                                      ╰╮      ╭╯
//!               ╰╮ GPL-3.0+ ╭─╯                                                        ╰──────╯
//!                ╰─╮      ╭─╯
//!                  ╰──────╯
//! ```
//!
//! Launch modes (matching `niri/src/main.rs`):
//! - Managed login session (`koma-session` / `koma.service` `Type=notify` / `greetd`):
//!   `koma --session`
//! - Direct bare-metal Linux VT (`seatd` / `systemd-logind` + `udev` + `DRM/KMS` + `libinput`):
//!   `koma --tty` (or simply `koma` when neither `WAYLAND_DISPLAY` nor `DISPLAY` is set)
//! - Nested development window inside an existing Wayland/X11 session:
//!   `koma --winit [--spawn ghostty]`

#[cfg(feature = "smithay-tty")]
mod backend_tty;
#[cfg(feature = "smithay-winit")]
mod backend_winit;
#[cfg(feature = "smithay-winit")]
mod config_watcher;
#[cfg(feature = "smithay-winit")]
mod cursor;
#[cfg(feature = "smithay-winit")]
mod gesture_dispatcher;
#[cfg(feature = "smithay-winit")]
mod handlers;
#[cfg(feature = "smithay-winit")]
mod ipc_server;
#[cfg(feature = "smithay-winit")]
mod keybind_dispatcher;
#[cfg(feature = "smithay-winit")]
mod macro_runtime;
#[cfg(feature = "smithay-winit")]
mod pointer_grabs;
#[cfg(feature = "smithay-winit")]
mod security_context;
#[cfg(feature = "smithay-winit")]
mod signals;
#[cfg(feature = "smithay-winit")]
mod state;
#[cfg(feature = "smithay-winit")]
mod xwayland_satellite;

mod cli;

use std::env;

use crate::cli::{CliAction, LaunchMode, RunOptions};

/// Synchronously imports compositor environment variables into `systemd --user` and D-Bus
/// before sending `READY=1`, ensuring `graphical-session.target` services (`waybar`, `fcitx5`,
/// `xdg-desktop-portal`, `koma-overview`) receive valid `WAYLAND_DISPLAY` and `DISPLAY` values
/// (`niri/src/main.rs` `import_environment`).
#[cfg(feature = "smithay-winit")]
fn import_session_environment() {
    use std::process::Command;
    use tracing::warn;

    const VARS: &str =
        "WAYLAND_DISPLAY DISPLAY XDG_CURRENT_DESKTOP XDG_SESSION_TYPE KOMA_INSTANCE_SIGNATURE";

    let script = format!(
        "if command -v systemctl >/dev/null 2>&1; then              systemctl --user import-environment {VARS};          fi;          if command -v dbus-update-activation-environment >/dev/null 2>&1; then              dbus-update-activation-environment {VARS};          fi"
    );

    match Command::new("/bin/sh").args(["-c", &script]).spawn() {
        Ok(mut child) => match child.wait() {
            Ok(status) if !status.success() => {
                warn!(%status, "session environment import shell returned non-zero status");
            }
            Ok(_) => {}
            Err(err) => warn!("failed waiting on session environment import shell: {err}"),
        },
        Err(err) => warn!("failed spawning /bin/sh for session environment import: {err}"),
    }
}

/// Parses `$NOTIFY_SOCKET`, supporting Linux `@` abstract sockets and pathname sockets on Unix.
#[cfg(feature = "smithay-winit")]
fn parse_notify_socket_addr(
    raw_sock: &std::ffi::OsStr,
) -> std::io::Result<std::os::unix::net::SocketAddr> {
    let sock_str = raw_sock.to_string_lossy();
    if let Some(abstract_name) = sock_str.strip_prefix('@') {
        #[cfg(target_os = "linux")]
        {
            use std::os::linux::net::SocketAddrExt as _;
            return std::os::unix::net::SocketAddr::from_abstract_name(abstract_name.as_bytes());
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = abstract_name;
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "abstract NOTIFY_SOCKET (@name) is only supported on Linux",
            ));
        }
    }
    std::os::unix::net::SocketAddr::from_pathname(std::path::Path::new(raw_sock))
}

/// Sends `READY=1\n` over `$NOTIFY_SOCKET` (`sd_notify` protocol, supporting both path & abstract
/// `@` Linux sockets) and `$NOTIFY_FD` (`dinit` readiness pipe) with zero external crate bloat.
#[cfg(feature = "smithay-winit")]
fn notify_service_manager_ready() {
    use std::{
        fs::File,
        io::Write as _,
        os::{fd::FromRawFd as _, unix::net::UnixDatagram},
    };
    use tracing::{debug, warn};

    if env::var_os("KOMA_DISABLE_SYSTEM_MANAGER_NOTIFY").is_some_and(|v| v == "1") {
        return;
    }

    if let Some(raw_sock) = env::var_os("NOTIFY_SOCKET") {
        let sock_str = raw_sock.to_string_lossy();
        let addr_res = parse_notify_socket_addr(&raw_sock);

        match (UnixDatagram::unbound(), addr_res) {
            (Ok(dgram), Ok(addr)) => {
                if let Err(err) = dgram.send_to_addr(b"READY=1\n", &addr) {
                    warn!("failed sending READY=1 to NOTIFY_SOCKET ({sock_str}): {err}");
                } else {
                    debug!(notify_socket = %sock_str, "notified systemd user manager READY=1");
                }
            }
            (Err(err), _) => warn!("failed creating unbound UnixDatagram for sd_notify: {err}"),
            (_, Err(err)) => warn!("invalid NOTIFY_SOCKET address `{sock_str}`: {err}"),
        }
    }

    if let Ok(fd_str) = env::var("NOTIFY_FD") {
        // SAFETY: Single-threaded startup prior to spawning background worker threads.
        unsafe {
            env::remove_var("NOTIFY_FD");
        }
        if let Ok(raw_fd) = fd_str.parse::<i32>() {
            // SAFETY: `NOTIFY_FD` is an open pipe passed by `dinit` exclusively for readiness notification.
            let mut pipe = unsafe { File::from_raw_fd(raw_fd) };
            if let Err(err) = pipe.write_all(b"READY=1\n") {
                warn!("failed writing READY=1 to NOTIFY_FD ({raw_fd}): {err}");
            }
        }
    }
}

/// Exports `WAYLAND_DISPLAY` and `DISPLAY` in the compositor process environment, imports them
/// into `systemd --user` / D-Bus when `--session` is active, and signals `READY=1`.
#[cfg(feature = "smithay-winit")]
fn finalize_session_startup(state: &crate::state::KomaState, is_session: bool) {
    // SAFETY: Called on the single-threaded compositor main thread before worker threads read env.
    unsafe {
        env::set_var("WAYLAND_DISPLAY", &state.socket_name);
        if let Some(sat) = &state.primary_satellite {
            env::set_var("DISPLAY", sat.display_name());
        } else {
            env::remove_var("DISPLAY");
        }
    }

    if is_session {
        import_session_environment();
    }
    notify_service_manager_ready();
}

#[cfg(feature = "smithay-winit")]
fn bind_wayland_listening_socket(
    event_loop: &calloop::EventLoop<'static, crate::state::KomaState>,
    display: smithay::reexports::wayland_server::Display<crate::state::KomaState>,
) -> anyhow::Result<std::ffi::OsString> {
    use anyhow::Context as _;
    use calloop::{Interest, Mode, PostAction, generic::Generic};
    use smithay::wayland::socket::ListeningSocketSource;

    use crate::security_context::NewClient;

    let socket_source =
        ListeningSocketSource::new_auto().context("failed creating Wayland listening socket")?;
    let socket_name = socket_source.socket_name().to_os_string();

    event_loop
        .handle()
        .insert_source(socket_source, |client_stream, _, state| {
            state.insert_wayland_client(NewClient::unrestricted(client_stream));
        })
        .map_err(|e| anyhow::anyhow!("failed registering Wayland socket source: {e}"))?;

    event_loop
        .handle()
        .insert_source(
            Generic::new(display, Interest::READ, Mode::Level),
            |_, display, state| {
                // SAFETY: `Generic` wraps `Display` without moving or dropping its FD while registered.
                unsafe {
                    display
                        .get_mut()
                        .dispatch_clients(state)
                        .expect("Wayland client dispatch error");
                }
                state.reconcile_session_lock();
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| anyhow::anyhow!("failed registering Wayland Display FD on calloop: {e}"))?;

    Ok(socket_name)
}

#[cfg(feature = "smithay-winit")]
fn run_nested_winit(
    spawn_cmd: Option<String>,
    config_override: Option<std::path::PathBuf>,
    is_session: bool,
) -> anyhow::Result<()> {
    use std::{process::Command, time::Duration};

    use anyhow::Context as _;
    use calloop::EventLoop;
    use smithay::reexports::wayland_server::Display;
    use tracing::info;

    use crate::{
        backend_winit::{WinitBackendState, render_winit_frame},
        state::{ActiveBackend, KomaState},
    };

    let mut event_loop: EventLoop<'static, KomaState> =
        EventLoop::try_new().context("failed creating calloop EventLoop")?;
    let display: Display<KomaState> =
        Display::new().context("failed creating wayland-server Display")?;
    let display_handle = display.handle();

    let socket_name = bind_wayland_listening_socket(&event_loop, display)?;

    let (mut winit_backend, initial_viewport) = WinitBackendState::init(&event_loop.handle())?;
    let mut state = KomaState::new(
        display_handle,
        event_loop.get_signal(),
        socket_name.clone(),
        "seat0",
        initial_viewport,
    );
    state.loop_handle = Some(event_loop.handle());
    crate::config_watcher::setup_config_watcher(
        &mut state,
        crate::config_watcher::KomaConfigPath::resolve(config_override),
    );
    winit_backend.bind_wayland_globals(&mut state);
    state.backend = Some(ActiveBackend::Winit(winit_backend));

    let ipc_paths =
        crate::ipc_server::start_dual_socket_server(&event_loop.handle(), &socket_name)?;
    state.instance_signature = Some(ipc_paths.instance_signature.clone());
    crate::signals::listen_for_termination(&event_loop.handle())?;
    crate::xwayland_satellite::setup_primary_satellite(&mut state);
    finalize_session_startup(&state, is_session);

    info!(
        wayland_display = ?socket_name,
        viewport = ?initial_viewport,
        "Koma nested Winit compositor initialized (Alt+Return=terminal, Alt+[ / ]=Deck cycle, Alt+G=Preempt)"
    );

    render_winit_frame(&mut state)?;
    state.queue_redraw();

    if let Some(cmd_name) = spawn_cmd {
        let active_ws = state.active_ws_id.clone();
        let mut child_cmd = Command::new(&cmd_name);
        state.configure_child_env(&mut child_cmd, Some(&active_ws));
        let _ = child_cmd.spawn();
    }

    event_loop
        .run(Some(Duration::from_millis(16)), &mut state, |state| {
            state.popups.cleanup();
            state.cleanup_ime_popups();
            let _ = state.display_handle.flush_clients();
        })
        .context("Koma nested Winit calloop failed")?;

    info!("Koma nested Winit event loop stopped; removing IPC sockets");
    drop(state);
    drop(ipc_paths);
    Ok(())
}

#[cfg(feature = "smithay-tty")]
fn run_bare_metal_tty(
    spawn_cmd: Option<String>,
    config_override: Option<std::path::PathBuf>,
    is_session: bool,
) -> anyhow::Result<()> {
    use std::{process::Command, time::Duration};

    use anyhow::Context as _;
    use calloop::EventLoop;
    use koma_model::LogicalViewport;
    use smithay::reexports::wayland_server::Display;
    use tracing::info;

    use crate::{
        backend_tty::{TtyBackendState, render_pending_tty_outputs},
        state::{ActiveBackend, KomaState},
    };

    let mut event_loop: EventLoop<'static, KomaState> =
        EventLoop::try_new().context("failed creating calloop EventLoop")?;
    let display: Display<KomaState> =
        Display::new().context("failed creating wayland-server Display")?;
    let display_handle = display.handle();

    let socket_name = bind_wayland_listening_socket(&event_loop, display)?;

    // Initialize unprivileged `LibSeatSession`, `UdevBackend`, `LibinputInputBackend`, & `GpuManager`
    let mut tty_backend = TtyBackendState::new(&event_loop.handle())?;
    let seat_name = tty_backend.seat_name.clone();

    let mut state = KomaState::new(
        display_handle,
        event_loop.get_signal(),
        socket_name.clone(),
        &seat_name,
        LogicalViewport::new(1920, 1080, 1.0),
    );
    state.loop_handle = Some(event_loop.handle());

    // Load initial `~/.config/koma/init.lua` + start watcher BEFORE KMS connector mode selection!
    crate::config_watcher::setup_config_watcher(
        &mut state,
        crate::config_watcher::KomaConfigPath::resolve(config_override),
    );

    // Enumerate primary iGPU + secondary dGPU KMS connectors and create `DrmCompositor` surfaces.
    // Running without any driven output is fatal: clients would see no `wl_output` and the VT
    // would keep showing the text console (komawm/koma#2).
    tty_backend.enumerate_initial_devices(&mut state)?;
    state.backend = Some(ActiveBackend::Tty(tty_backend));
    let initial_reg = state.lua_sandbox.config_snapshot();
    state.apply_runtime_input_config(&initial_reg);

    let ipc_paths =
        crate::ipc_server::start_dual_socket_server(&event_loop.handle(), &socket_name)?;
    state.instance_signature = Some(ipc_paths.instance_signature.clone());
    crate::signals::listen_for_termination(&event_loop.handle())?;
    crate::xwayland_satellite::setup_primary_satellite(&mut state);
    finalize_session_startup(&state, is_session);

    info!(
        wayland_display = ?socket_name,
        seat = %seat_name,
        is_session,
        "Koma bare-metal KMS/DRM compositor initialized (Super+Return=terminal, Super+G=Preempt, Ctrl+Alt+F1..F12=VT)"
    );

    // Submit Frame 1 (`#111216` backdrop) to active KMS CRTCs
    render_pending_tty_outputs(&mut state);

    if let Some(cmd_name) = spawn_cmd {
        let active_ws = state.active_ws_id.clone();
        let mut child_cmd = Command::new(&cmd_name);
        state.configure_child_env(&mut child_cmd, Some(&active_ws));
        let _ = child_cmd.spawn();
    }

    event_loop
        .run(Some(Duration::from_millis(16)), &mut state, |state| {
            if state.redraw_queued {
                render_pending_tty_outputs(state);
            }
            state.popups.cleanup();
            state.cleanup_ime_popups();
            let _ = state.display_handle.flush_clients();
        })
        .context("Koma bare-metal TTY calloop failed")?;

    // Drop order: the backend (DRM surfaces, DRM master, libseat session) goes first, then the
    // IPC sockets and finally the Wayland listening socket owned by the event loop.
    info!("Koma TTY event loop stopped; releasing DRM master, seat and IPC sockets");
    drop(state);
    drop(ipc_paths);
    Ok(())
}

fn main() {
    let action = match cli::parse_args(env::args().skip(1)) {
        Ok(action) => action,
        Err(err) => {
            eprintln!("koma: {err}\n\n{}", cli::usage());
            std::process::exit(cli::USAGE_EXIT_CODE);
        }
    };
    let opts = match action {
        CliAction::Help => {
            print!("{}", cli::usage());
            return;
        }
        CliAction::Version => {
            println!("{}", cli::version());
            return;
        }
        CliAction::Run(opts) => opts,
    };

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let RunOptions {
        mode,
        is_session,
        spawn_cmd,
        config_override,
    } = opts;

    // Match `niri/src/main.rs` `--session` semantics: scrub stale host display variables unless WSL,
    // and export `XDG_CURRENT_DESKTOP=koma` + `XDG_SESSION_TYPE=wayland` for portals and autostart.
    if is_session {
        // SAFETY: Single-threaded CLI initialization before spawning any worker threads.
        unsafe {
            if env::var_os("WSL_DISTRO_NAME").is_none() && mode == LaunchMode::Auto {
                env::remove_var("DISPLAY");
                env::remove_var("WAYLAND_DISPLAY");
                env::remove_var("WAYLAND_SOCKET");
            }
            env::set_var("XDG_CURRENT_DESKTOP", "koma");
            env::set_var("XDG_SESSION_TYPE", "wayland");
        }
    }

    let resolved_mode = match mode {
        LaunchMode::Auto => {
            if env::var_os("WAYLAND_DISPLAY").is_some() || env::var_os("DISPLAY").is_some() {
                LaunchMode::Winit
            } else {
                LaunchMode::Tty
            }
        }
        explicit => explicit,
    };

    match resolved_mode {
        LaunchMode::Tty => {
            #[cfg(feature = "smithay-tty")]
            {
                if let Err(err) = run_bare_metal_tty(spawn_cmd, config_override, is_session) {
                    eprintln!("koma bare-metal TTY fatal error: {err:#}");
                    std::process::exit(1);
                }
            }
            #[cfg(not(feature = "smithay-tty"))]
            {
                let _ = (&spawn_cmd, &config_override, is_session);
                eprintln!("koma built without `smithay-tty` feature");
                std::process::exit(1);
            }
        }
        LaunchMode::Winit => {
            #[cfg(feature = "smithay-winit")]
            {
                if let Err(err) = run_nested_winit(spawn_cmd, config_override, is_session) {
                    eprintln!("koma nested Winit fatal error: {err:#}");
                    std::process::exit(1);
                }
            }
            #[cfg(not(feature = "smithay-winit"))]
            {
                let _ = (&spawn_cmd, &config_override, is_session);
                eprintln!("koma built without `smithay-winit` feature");
                std::process::exit(1);
            }
        }
        LaunchMode::HeadlessCheck | LaunchMode::Auto => {
            let _ = (&spawn_cmd, &config_override, is_session);
            println!(
                "koma v{} (headless state-machine check ok; supports `--session`, `--tty`, `--winit`)",
                env!("CARGO_PKG_VERSION")
            );
        }
    }
}
