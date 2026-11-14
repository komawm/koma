//! Wayland `wp_security_context_v1` sandboxed client isolation, privileged global protocol
//! filtering, and per-workspace jailed container sockets (modeled on `niri/src/niri.rs` &
//! `niri/src/handlers/mod.rs`).
//!
//! ### Security Model & Privileged Protocol Boundary
//! Clients marked `ClientState { restricted: true, .. }` (either connected via an upstream
//! `wp_security_context_v1` listener from Flatpak / Bubblewrap / `pw-container`, or via Koma's
//! per-workspace `jail-<ws_id>.sock` container socket) are **denied** visibility (`can_view = false`)
//! and binding access to all privileged compositor globals:
//! 1. `wp_security_context_manager_v1` — prevents recursive nested security-context privilege escalation.
//! 2. `zwlr_layer_shell_v1` — prevents sandboxed apps from mapping `Overlay`/`Top` lock-screen phishing
//!    surfaces or input-capturing desktop bars.
//! 3. `zwlr_data_control_manager_v1` & `ext_data_control_manager_v1` — prevents background clipboard
//!    sniffers inside Flatpak or AI Agent containers from reading host clipboard secrets without focus.
//! 4. `zwp_primary_selection_device_manager_v1` — isolates middle-click primary selection buffers.
//! 5. `zwp_virtual_keyboard_manager_v1` — prevents synthetic host keystroke injection.
//! 6. `ext_session_lock_manager_v1` — prevents screen-lock hijacking.

use std::{
    env, fs, io,
    os::unix::{
        fs::PermissionsExt as _,
        net::{UnixListener, UnixStream},
    },
    path::PathBuf,
    process,
};

use anyhow::Context as _;
use calloop::{Interest, LoopHandle, Mode, PostAction, RegistrationToken, generic::Generic};
use koma_proto::WorkspaceId;
use smithay::{
    reexports::wayland_server::{Client, Resource as _, protocol::wl_surface::WlSurface},
    wayland::security_context::{
        SecurityContext, SecurityContextHandler, SecurityContextListenerSource,
    },
};
use tracing::{debug, info, trace, warn};

use crate::state::{ClientState, KomaState};

/// Kernel-verified peer identity (`SO_PEERCRED` / `LOCAL_PEERCRED` / `getpeereid`) captured when a
/// Wayland client connects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerSocketCredentials {
    pub pid: Option<i32>,
    pub uid: u32,
    pub gid: u32,
}

/// Connection descriptor passed to `KomaState::insert_wayland_client`
/// (modeled directly on `niri/src/niri.rs` `NewClient` lines 7497-7501).
pub struct NewClient {
    pub client: UnixStream,
    /// When `true`, all privileged Wayland globals (`security-context-v1`, `wlr-layer-shell`,
    /// `wlr/ext-data-control`, `primary-selection`, `virtual-keyboard`, `ext-session-lock`) are
    /// hidden from `wl_registry` and rejected by `GlobalDispatch::can_view`.
    pub restricted: bool,
    /// Set when peer credentials cannot be attributed (e.g. proxied D-Bus socket pairs).
    pub credentials_unknown: bool,
    /// Upstream `wp_security_context_v1` metadata (`sandbox_engine`, `app_id`, `instance_id`).
    pub security_context: Option<SecurityContext>,
    /// Optional workspace jail pinning all client surfaces strictly to a `Headless` `WorkspaceId`.
    pub jailed_workspace: Option<WorkspaceId>,
    /// Human-readable container/sandbox label for audit logging (`"flatpak"`, `"opencode-bwrap"`, etc.).
    pub sandbox_label: Option<String>,
}

impl NewClient {
    /// Host-trusted Wayland client arriving on the primary `WAYLAND_DISPLAY` socket (`wayland-X`).
    #[must_use]
    pub fn unrestricted(client: UnixStream) -> Self {
        Self {
            client,
            restricted: false,
            credentials_unknown: false,
            security_context: None,
            jailed_workspace: None,
            sandbox_label: None,
        }
    }

    /// Sandboxed Wayland client arriving via `wp_security_context_v1` (`Flatpak`, `bwrap`, `pw-container`).
    #[must_use]
    pub fn from_security_context(client: UnixStream, context: SecurityContext) -> Self {
        Self {
            client,
            restricted: true,
            credentials_unknown: false,
            security_context: Some(context),
            jailed_workspace: None,
            sandbox_label: Some("wp_security_context_v1".to_owned()),
        }
    }

    /// Sandboxed AI Agent / container client arriving on a per-workspace `jail-<ws_id>.sock` socket.
    #[must_use]
    pub fn jailed_to_workspace(client: UnixStream, ws_id: WorkspaceId, label: String) -> Self {
        Self {
            client,
            restricted: true,
            credentials_unknown: false,
            security_context: None,
            jailed_workspace: Some(ws_id),
            sandbox_label: Some(label),
        }
    }
}

/// Active per-workspace jailed Wayland socket registered on `calloop` for Podman/bwrap containers.
pub struct JailedWaylandSocket {
    // Workspace/sandbox ownership metadata retained on active jailed socket.
    #[allow(dead_code)]
    pub ws_id: WorkspaceId,
    pub socket_path: PathBuf,
    #[allow(dead_code)]
    pub label: String,
    pub reg_token: RegistrationToken,
}

/// Global `can_view` predicate restricting privileged Wayland protocols strictly to trusted host
/// clients (`niri/src/niri.rs` `client_is_unrestricted` line 2620).
#[must_use]
pub fn client_is_unrestricted(client: &Client) -> bool {
    client
        .get_data::<ClientState>()
        .is_some_and(|cs| !cs.restricted)
}

/// Global `can_view` predicate for `zwp_primary_selection_device_manager_v1`: blocks both explicit
/// `primary_selection_disabled` configs and all `restricted` sandboxed clients.
#[must_use]
pub fn client_can_view_primary_selection(client: &Client) -> bool {
    client
        .get_data::<ClientState>()
        .is_some_and(|cs| !cs.primary_selection_disabled && !cs.restricted)
}

/// Reads kernel-verified peer credentials (`pid`, `uid`, `gid`) from an accepted Wayland client
/// `UnixStream`.
#[must_use]
pub fn query_peer_credentials(stream: &UnixStream) -> Option<PeerSocketCredentials> {
    let creds = koma_proto::scm_unix::query_socket_peer_creds(stream).ok()?;
    Some(PeerSocketCredentials {
        pid: creds.pid,
        uid: creds.uid,
        gid: creds.gid,
    })
}

/// Returns `true` if `surface` belongs to a `restricted` (`security-context-v1` or jailed) client.
#[must_use]
pub fn is_surface_from_restricted_client(state: &KomaState, surface: &WlSurface) -> bool {
    state
        .display_handle
        .get_client(surface.id())
        .ok()
        .and_then(|client| client.get_data::<ClientState>().map(|cs| cs.restricted))
        .unwrap_or(true)
}

/// Returns `Some(ws_id)` if `surface`'s owning Wayland client is pinned to an isolated workspace jail.
#[must_use]
pub fn resolve_surface_jailed_workspace(
    state: &KomaState,
    surface: &WlSurface,
) -> Option<WorkspaceId> {
    state
        .display_handle
        .get_client(surface.id())
        .ok()
        .and_then(|client| {
            client
                .get_data::<ClientState>()
                .and_then(|cs| cs.jailed_workspace.clone())
        })
}

impl SecurityContextHandler for KomaState {
    /// Registers a committed `wp_security_context_v1` `listen_fd` onto `calloop`
    /// (`niri/src/handlers/mod.rs` lines 490-504).
    ///
    /// Per the Wayland `security-context-v1` specification, `SecurityContextListenerSource`
    /// continues accepting sandboxed client connections on `listen_fd` even after the sandbox
    /// launcher disconnects, stopping only when `close_fd` signals hangup (`EPOLLHUP`).
    fn context_created(&mut self, source: SecurityContextListenerSource, context: SecurityContext) {
        let Some(loop_handle) = self.loop_handle.clone() else {
            warn!("dropping SecurityContextListenerSource: missing calloop LoopHandle");
            return;
        };

        info!(
            ?context,
            "registered wp_security_context_v1 sandbox listener on calloop"
        );

        if let Err(err) = loop_handle.insert_source(source, move |client_stream, _, state| {
            trace!(
                ?context,
                "accepting restricted Wayland client via wp_security_context_v1"
            );
            state.insert_wayland_client(NewClient::from_security_context(
                client_stream,
                context.clone(),
            ));
        }) {
            warn!("failed inserting SecurityContextListenerSource into calloop: {err}");
        }
    }
}

/// Binds a dedicated `0600` Wayland socket (`$XDG_RUNTIME_DIR/koma/<sig>/jail-<ws_id>.sock`) for
/// a Podman / Bubblewrap AI Agent container. Every client connecting through this socket is
/// automatically marked `restricted = true` and pinned exclusively to `ws_id`.
pub fn create_jailed_wayland_socket(
    loop_handle: &LoopHandle<'static, KomaState>,
    state: &mut KomaState,
    ws_id: WorkspaceId,
    label: String,
) -> anyhow::Result<PathBuf> {
    if let Some(existing) = state.jailed_wayland_sockets.get(&ws_id) {
        return Ok(existing.socket_path.clone());
    }

    let base_runtime = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(format!(
                "/tmp/koma-runtime-{}",
                rustix::process::getuid().as_raw()
            ))
        });
    let sig = state
        .instance_signature
        .clone()
        .unwrap_or_else(|| format!("{}_wayland", process::id()));
    let runtime_dir = base_runtime.join("koma").join(sig);
    fs::create_dir_all(&runtime_dir)
        .with_context(|| format!("failed creating jail runtime dir {}", runtime_dir.display()))?;
    fs::set_permissions(&runtime_dir, fs::Permissions::from_mode(0o700))?;

    let sanitized_ws: String = ws_id
        .as_str()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let socket_path = runtime_dir.join(format!("jail-{sanitized_ws}.sock"));
    if socket_path.exists() {
        let _ = fs::remove_file(&socket_path);
    }

    let listener = UnixListener::bind(&socket_path).with_context(|| {
        format!(
            "failed binding jailed Wayland socket {}",
            socket_path.display()
        )
    })?;
    fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;

    let ws_for_cb = ws_id.clone();
    let label_for_cb = label.clone();
    let reg_token = loop_handle
        .insert_source(
            Generic::new(listener, Interest::READ, Mode::Level),
            move |_, listener, state| {
                loop {
                    match listener.accept() {
                        Ok((stream, _addr)) => {
                            debug!(
                                ws_id = %ws_for_cb,
                                sandbox = %label_for_cb,
                                "accepted jailed container Wayland client"
                            );
                            state.insert_wayland_client(NewClient::jailed_to_workspace(
                                stream,
                                ws_for_cb.clone(),
                                label_for_cb.clone(),
                            ));
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                        Err(e) => {
                            warn!(ws_id = %ws_for_cb, "jailed Wayland socket accept error: {e}");
                            break;
                        }
                    }
                }
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| anyhow::anyhow!("failed registering jailed Wayland socket on calloop: {e}"))?;

    info!(
        ws_id = %ws_id,
        sandbox = %label,
        socket = %socket_path.display(),
        "created restricted Wayland jail socket for workspace"
    );

    state.jailed_wayland_sockets.insert(
        ws_id,
        JailedWaylandSocket {
            ws_id: WorkspaceId::new(sanitized_ws),
            socket_path: socket_path.clone(),
            label,
            reg_token,
        },
    );

    Ok(socket_path)
}

/// Revokes and unlinks a per-workspace jailed Wayland socket from `calloop`.
pub fn revoke_jailed_wayland_socket(state: &mut KomaState, ws_id: &WorkspaceId) -> bool {
    let Some(jail) = state.jailed_wayland_sockets.remove(ws_id) else {
        return false;
    };
    if let Some(lh) = &state.loop_handle {
        lh.remove(jail.reg_token);
    }
    let _ = fs::remove_file(&jail.socket_path);
    info!(
        ws_id = %ws_id,
        socket = %jail.socket_path.display(),
        "revoked jailed Wayland socket"
    );
    true
}
