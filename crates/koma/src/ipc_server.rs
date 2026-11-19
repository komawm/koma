//! Non-blocking dual-socket `calloop` IPC server (`koma-admin.sock` & `koma-agent.sock`)
//! with `SO_PEERCRED` admin verification, `CapabilityToken` RBAC, 4-step `Epoch` fencing,
//! and zero-copy sealed `memfd` `SCM_RIGHTS` frame transfer (`docs/IPC_SPEC.md`).

use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    env,
    ffi::OsStr,
    fs::{self, File},
    io::{self, Read as _},
    os::{
        fd::OwnedFd,
        unix::{
            fs::PermissionsExt as _,
            net::{UnixListener, UnixStream},
        },
    },
    path::{Path, PathBuf},
    process,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::Context as _;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use calloop::{
    EventSource, Interest, LoopHandle, Mode, Poll, PostAction, Readiness, RegistrationToken, Token,
    TokenFactory, generic::Generic,
};
use koma_model::{
    LayoutMode, LogicalViewport, PresentationTarget, WindowMatchContext, WorkspaceLeaseState,
    WorkspaceState,
};
use koma_proto::{
    ADMIN_SOCKET_NAME, AGENT_SOCKET_NAME, AncillaryFdRef, CapabilityToken, DeliverySemantics,
    Epoch, ErrorSymbol, EventRingBuffer, EventTopic, IpcError, IpcRequest, IpcResponse,
    OverlayKind, PrivilegeLevel, SeatId, SessionId, TokenAuthorizer as _, TokenClaims,
    TransportKind, WorkspaceEvent, WorkspaceId,
    rpc::{
        CaptureFrameParams, CaptureFrameResult, CursorLabelParams, HitTestParams, HitTestResult,
        LuaExecTxParams, RelativeMoveParams, WheelScrollParams, WorkspaceModeSpec,
        WorkspaceSetModeParams,
    },
    scm_unix::{
        FrameInbox, FrameOutbox, InboxPoll, create_sealed_readonly_memfd, verify_same_uid_peer,
    },
};
use koma_render::{
    RenderTargetKind,
    frame_composer::{WindowSceneItem, assemble_workspace_elements, prepend_agent_cursor_elements},
    offscreen_fbo::LazyOffscreenSurface,
};
use koma_seat::VirtualSeatLedger;
use serde_json::{Value, json};
use smithay::utils::Scale;
use tracing::{debug, info, warn};

use crate::state::KomaState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SocketChannel {
    Admin,
    Agent,
}

#[derive(Debug, Clone)]
pub enum PeerAuthState {
    AdminVerified {
        // `SO_PEERCRED` identity retained for admin audit diagnostics.
        #[allow(dead_code)]
        uid: u32,
        // `SO_PEERCRED` identity retained for admin audit diagnostics.
        #[allow(dead_code)]
        gid: u32,
        session_id: SessionId,
    },
    AgentUnauthenticated,
    AgentAuthenticated {
        session_id: SessionId,
        token: CapabilityToken,
    },
}

/// `Generic` socket wrapper whose `reregister` updates `EPOLLOUT` interest from an atomic flag,
/// avoiding `Dispatcher::as_source_mut` `RefCell` re-entrancy inside its own callback.
struct IpcClientSource {
    inner: Generic<UnixStream, io::Error>,
    want_write: Arc<AtomicBool>,
}

impl EventSource for IpcClientSource {
    type Event = Readiness;
    type Metadata = calloop::generic::NoIoDrop<UnixStream>;
    type Ret = Result<PostAction, io::Error>;
    type Error = io::Error;

    fn process_events<F>(
        &mut self,
        readiness: Readiness,
        token: Token,
        callback: F,
    ) -> Result<PostAction, Self::Error>
    where
        F: FnMut(Self::Event, &mut Self::Metadata) -> Self::Ret,
    {
        self.inner.process_events(readiness, token, callback)
    }

    fn register(
        &mut self,
        poll: &mut Poll,
        token_factory: &mut TokenFactory,
    ) -> calloop::Result<()> {
        self.inner.register(poll, token_factory)
    }

    fn reregister(
        &mut self,
        poll: &mut Poll,
        token_factory: &mut TokenFactory,
    ) -> calloop::Result<()> {
        self.inner.interest = if self.want_write.load(Ordering::Relaxed) {
            Interest::BOTH
        } else {
            Interest::READ
        };
        self.inner.reregister(poll, token_factory)
    }

    fn unregister(&mut self, poll: &mut Poll) -> calloop::Result<()> {
        self.inner.unregister(poll)
    }
}

/// Per-connection non-blocking state registered on `calloop` (`Interest::READ` or `Interest::BOTH`).
pub struct IpcClientConn {
    // Connection ID and calloop token retained for IPC lifecycle/diagnostics.
    #[allow(dead_code)]
    pub conn_id: u64,
    pub stream: UnixStream,
    pub channel: SocketChannel,
    pub auth: PeerAuthState,
    #[allow(dead_code)]
    pub reg_token: Option<RegistrationToken>,
    pub inbox: FrameInbox,
    pub outbox: FrameOutbox,
    pub event_ring: EventRingBuffer,
    pub subscribed_workspaces: BTreeSet<WorkspaceId>,
    pub subscribed_topics: HashSet<EventTopic>,
    pub armed_writable: bool,
}

impl IpcClientConn {
    pub fn drain_event_ring_into_outbox(&mut self) -> Result<(), String> {
        while let Some(env) = self.event_ring.pop_next() {
            self.outbox.enqueue(&env, Vec::new())?;
        }
        Ok(())
    }
}

pub struct IpcServerPaths {
    pub instance_signature: String,
    pub runtime_dir: PathBuf,
    pub admin_socket: PathBuf,
    pub agent_socket: PathBuf,
}

impl Drop for IpcServerPaths {
    /// Removes both IPC sockets and the per-instance runtime directory on graceful shutdown so
    /// stale `KOMA_INSTANCE_SIGNATURE` directories do not accumulate (komawm/koma#4).
    fn drop(&mut self) {
        for sock in [&self.admin_socket, &self.agent_socket] {
            if let Err(err) = fs::remove_file(sock)
                && err.kind() != io::ErrorKind::NotFound
            {
                warn!(socket = %sock.display(), "failed removing IPC socket: {err}");
            }
        }
        // Only removes the directory when empty; foreign files are left in place.
        let _ = fs::remove_dir(&self.runtime_dir);
        info!(runtime_dir = %self.runtime_dir.display(), "removed IPC sockets");
    }
}

/// Initializes `$XDG_RUNTIME_DIR/koma/<instance_signature>/` (`0700`) and registers both
/// `koma-admin.sock` and `koma-agent.sock` non-blocking listeners onto `calloop`.
pub fn start_dual_socket_server(
    loop_handle: &LoopHandle<'static, KomaState>,
    wayland_socket_name: &OsStr,
) -> anyhow::Result<IpcServerPaths> {
    let base_runtime = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(format!(
                "/tmp/koma-runtime-{}",
                rustix::process::getuid().as_raw()
            ))
        });

    let instance_signature = format!(
        "{}_{}",
        process::id(),
        wayland_socket_name.to_string_lossy()
    );
    let runtime_dir = base_runtime.join("koma").join(&instance_signature);
    fs::create_dir_all(&runtime_dir)
        .with_context(|| format!("failed creating IPC runtime dir {}", runtime_dir.display()))?;
    fs::set_permissions(&runtime_dir, fs::Permissions::from_mode(0o700))?;

    let admin_socket = runtime_dir.join(ADMIN_SOCKET_NAME);
    let agent_socket = runtime_dir.join(AGENT_SOCKET_NAME);

    bind_nonblocking_listener(loop_handle, &admin_socket, SocketChannel::Admin)?;
    bind_nonblocking_listener(loop_handle, &agent_socket, SocketChannel::Agent)?;

    // Delivered to child processes via `KomaState::configure_child_env` / `child_env` without mutating `environ`.

    info!(
        admin_sock = %admin_socket.display(),
        agent_sock = %agent_socket.display(),
        "bound non-blocking dual IPC sockets on calloop"
    );

    Ok(IpcServerPaths {
        instance_signature,
        runtime_dir,
        admin_socket,
        agent_socket,
    })
}

fn bind_nonblocking_listener(
    loop_handle: &LoopHandle<'static, KomaState>,
    sock_path: &Path,
    channel: SocketChannel,
) -> anyhow::Result<()> {
    if sock_path.exists() {
        let _ = fs::remove_file(sock_path);
    }
    let listener = UnixListener::bind(sock_path)
        .with_context(|| format!("failed binding {}", sock_path.display()))?;
    fs::set_permissions(sock_path, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;

    let handle_for_accept = loop_handle.clone();
    loop_handle
        .insert_source(
            Generic::new(listener, Interest::READ, Mode::Level),
            move |_, listener, state| {
                loop {
                    match listener.accept() {
                        Ok((stream, _addr)) => {
                            if let Err(err) =
                                accept_ipc_peer(&handle_for_accept, state, stream, channel)
                            {
                                warn!(?channel, "rejected incoming IPC connection: {err}");
                            }
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                        Err(e) => {
                            warn!(?channel, "IPC listener accept error: {e}");
                            break;
                        }
                    }
                }
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| anyhow::anyhow!("failed inserting {channel:?} listener into calloop: {e}"))?;

    Ok(())
}

fn accept_ipc_peer(
    loop_handle: &LoopHandle<'static, KomaState>,
    state: &mut KomaState,
    stream: UnixStream,
    channel: SocketChannel,
) -> io::Result<()> {
    stream.set_nonblocking(true)?;
    let conn_id = state.allocate_ipc_conn_id();

    let auth = match channel {
        SocketChannel::Admin => {
            let creds = verify_same_uid_peer(&stream)?;
            debug!(
                conn_id,
                uid = creds.uid,
                gid = creds.gid,
                "accepted admin IPC peer"
            );
            PeerAuthState::AdminVerified {
                uid: creds.uid,
                gid: creds.gid,
                session_id: SessionId::new(format!("admin_{conn_id}")),
            }
        }
        SocketChannel::Agent => PeerAuthState::AgentUnauthenticated,
    };

    let stream_clone = stream.try_clone()?;
    let loop_for_io = loop_handle.clone();
    let want_write_flag = Arc::new(AtomicBool::new(false));
    let want_write_cb = Arc::clone(&want_write_flag);
    let reg_token = loop_handle
        .insert_source(
            IpcClientSource {
                inner: Generic::new(stream, Interest::READ, Mode::Level),
                want_write: want_write_flag,
            },
            move |readiness, generic_stream, state| {
                let disconnect = drive_client_io(
                    &loop_for_io,
                    state,
                    conn_id,
                    generic_stream.as_ref(),
                    readiness.readable,
                    readiness.writable,
                );
                if disconnect {
                    cleanup_disconnected_ipc_peer(state, conn_id);
                    return Ok(PostAction::Remove);
                }
                // Toggle EPOLLOUT (`Interest::BOTH`) ONLY when outbox has pending bytes so idle
                // sockets never busy-spin Level-triggered readiness!
                if let Some(conn) = state.ipc_clients.get_mut(&conn_id) {
                    let want_write = !conn.outbox.is_empty();
                    if want_write != conn.armed_writable {
                        conn.armed_writable = want_write;
                        want_write_cb.store(want_write, Ordering::Relaxed);
                        return Ok(PostAction::Reregister);
                    }
                }
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| io::Error::other(format!("calloop insert client failed: {e}")))?;

    state.ipc_clients.insert(
        conn_id,
        IpcClientConn {
            conn_id,
            stream: stream_clone,
            channel,
            auth,
            reg_token: Some(reg_token),
            inbox: FrameInbox::default(),
            outbox: FrameOutbox::default(),
            event_ring: EventRingBuffer::default(),
            subscribed_workspaces: BTreeSet::new(),
            subscribed_topics: HashSet::new(),
            armed_writable: false,
        },
    );

    Ok(())
}

fn drive_client_io(
    _loop_handle: &LoopHandle<'static, KomaState>,
    state: &mut KomaState,
    conn_id: u64,
    stream: &UnixStream,
    readable: bool,
    writable: bool,
) -> bool {
    let mut saw_eof = false;
    if readable {
        let read_res = state
            .ipc_clients
            .get_mut(&conn_id)
            .map(|c| c.inbox.read_available(stream));
        match read_res {
            Some(Ok(eof)) => saw_eof = eof,
            Some(Err(err)) => {
                warn!(conn_id, "dropping IPC peer on read error: {err}");
                return true;
            }
            None => return true,
        }

        loop {
            let next = state
                .ipc_clients
                .get_mut(&conn_id)
                .map(|c| c.inbox.next_frame(saw_eof));
            match next {
                Some(Ok(InboxPoll::Frame { json_bytes, fds })) => {
                    let (resp, resp_fds) = handle_raw_frame(state, conn_id, &json_bytes, fds);
                    if let Some(conn) = state.ipc_clients.get_mut(&conn_id)
                        && let Err(err) = conn.outbox.enqueue(&resp, resp_fds)
                    {
                        warn!(conn_id, "dropping wedged IPC peer ({err})");
                        return true;
                    }
                }
                Some(Ok(InboxPoll::NeedMoreData)) => break,
                Some(Ok(InboxPoll::PeerClosed)) => return true,
                Some(Err(decode_err)) => {
                    let sym = match decode_err {
                        koma_proto::FrameDecodeError::FrameTooLarge(_) => {
                            ErrorSymbol::ErrFrameTooLarge
                        }
                        _ => ErrorSymbol::ErrInvalidParams,
                    };
                    let err_resp = IpcResponse::err(0, IpcError::new(sym, decode_err.to_string()));
                    if let Some(conn) = state.ipc_clients.get_mut(&conn_id) {
                        let _ = conn.outbox.enqueue(&err_resp, Vec::new());
                        let _ = conn.outbox.try_flush(stream);
                    }
                    return true;
                }
                None => return true,
            }
        }
    }

    if (writable
        || state
            .ipc_clients
            .get(&conn_id)
            .is_some_and(|c| !c.outbox.is_empty()))
        && let Some(conn) = state.ipc_clients.get_mut(&conn_id)
    {
        if conn.drain_event_ring_into_outbox().is_err() {
            return true;
        }
        match conn.outbox.try_flush(stream) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) => {
                debug!(conn_id, "IPC write closed: {e}");
                return true;
            }
        }
    }

    saw_eof
}

fn handle_raw_frame(
    state: &mut KomaState,
    conn_id: u64,
    json_bytes: &[u8],
    inbound_fds: Vec<OwnedFd>,
) -> (IpcResponse, Vec<OwnedFd>) {
    let req: IpcRequest = match serde_json::from_slice(json_bytes) {
        Ok(r) => r,
        Err(e) => {
            return (
                IpcResponse::err(
                    0,
                    IpcError::new(ErrorSymbol::ErrInvalidParams, format!("invalid JSON: {e}")),
                ),
                Vec::new(),
            );
        }
    };

    let req_id = req.id;
    match dispatch_rpc(state, conn_id, req, inbound_fds) {
        Ok((val, out_fds)) => (IpcResponse::ok(req_id, val), out_fds),
        Err(ipc_err) => (IpcResponse::err(req_id, ipc_err), Vec::new()),
    }
}

/// Fail-closed IPC gate: blocks frame capture, scene introspection, and state mutation
/// whenever the compositor is `Locked` or `Abandoned`, permitting only read-only `config.status`.
/// Renders the effective `libinput` knobs reported by `config.status`.
///
/// A cursor that travels a different distance than another compositor at the same physical
/// movement is almost always one of these values rather than the motion path: an unset
/// `accel_profile` keeps libinput's adaptive curve, while `"flat"` removes acceleration entirely
/// (komawm/koma#34). Reporting the *applied* values makes that inspectable instead of guessed.
#[must_use]
pub(crate) fn config_status_input_json(
    input: &koma_lua::schema::InputConfigSpec,
) -> serde_json::Value {
    use koma_lua::schema::AccelProfileSpec;

    json!({
        // XKB keymap inputs. `options` is the same knob as Hyprland's `kb_options`, so a
        // Caps-to-Escape remap is `options = { "caps:escape" }` in either compositor.
        "keyboard": {
            "layout": input.keyboard.layout,
            "variant": input.keyboard.variant,
            "model": input.keyboard.model,
            "options": input.keyboard.options,
            "repeat_delay_ms": input.keyboard.repeat_delay,
            "repeat_rate_hz": input.keyboard.repeat_rate,
        },
        "pointer": {
            "accel_speed": input.pointer.accel_speed,
            "accel_profile": input.pointer.accel_profile.map(|p| match p {
                AccelProfileSpec::Adaptive => "adaptive",
                AccelProfileSpec::Flat => "flat",
            }),
            "natural_scroll": input.pointer.natural_scroll,
            "scroll_factor": input.pointer.scroll_factor,
            "follow_mouse": input.pointer.follow_mouse,
        },
        "touchpad": {
            "natural_scroll": input.touchpad.natural_scroll,
            "tap_to_click": input.touchpad.tap,
            "disable_while_typing": input.touchpad.dwt,
            "scroll_factor": input.touchpad.scroll_factor,
        },
    })
}

pub(crate) fn check_ipc_lock_gate(
    is_locked_or_abandoned: bool,
    method: &str,
) -> Result<(), IpcError> {
    if is_locked_or_abandoned && method != "config.status" {
        return Err(IpcError::new(
            ErrorSymbol::ErrCapabilityDenied,
            "session is locked",
        ));
    }
    Ok(())
}

/// Authorizes and executes a parsed `IpcRequest` against `KomaState`.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn dispatch_rpc(
    state: &mut KomaState,
    conn_id: u64,
    req: IpcRequest,
    _inbound_fds: Vec<OwnedFd>,
) -> Result<(Value, Vec<OwnedFd>), IpcError> {
    state.reconcile_session_lock();
    let now_ms = state.elapsed_ms();
    let (channel, auth_snapshot) = {
        let conn = state
            .ipc_clients
            .get(&conn_id)
            .ok_or_else(|| IpcError::new(ErrorSymbol::ErrNotFound, "disconnected session"))?;
        (conn.channel, conn.auth.clone())
    };

    // Gate 0: `koma-agent.sock` connections MUST call `auth.authenticate` before any other RPC.
    if matches!(auth_snapshot, PeerAuthState::AgentUnauthenticated)
        && req.method != "auth.authenticate"
    {
        return Err(IpcError::new(
            ErrorSymbol::ErrUnauthenticated,
            "first frame on koma-agent.sock must be auth.authenticate",
        ));
    }

    // Gate 1: Fail-closed session lock gate blocks capture/inventory/mutation in `Locked`/`Abandoned`.
    check_ipc_lock_gate(state.is_locked_or_abandoned(), &req.method)?;

    match req.method.as_str() {
        "auth.mint_token" => {
            if channel != SocketChannel::Admin {
                return Err(IpcError::new(
                    ErrorSymbol::ErrCapabilityDenied,
                    "auth.mint_token is restricted to koma-admin.sock",
                ));
            }
            let label = req
                .params
                .get("label")
                .and_then(Value::as_str)
                .unwrap_or("agent")
                .to_owned();
            let ttl_secs = req
                .params
                .get("ttl_secs")
                .and_then(Value::as_u64)
                .unwrap_or(28_800);
            let can_create_headless = req
                .params
                .get("can_create_headless")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let max_owned_workspaces = req
                .params
                .get("max_owned_workspaces")
                .and_then(Value::as_u64)
                .unwrap_or(4) as usize;

            let mut grants = BTreeMap::new();
            if let Some(obj) = req
                .params
                .get("workspace_grants")
                .and_then(Value::as_object)
            {
                for (ws_k, lvl_v) in obj {
                    let lvl: PrivilegeLevel = serde_json::from_value(lvl_v.clone())
                        .map_err(|e| IpcError::new(ErrorSymbol::ErrInvalidParams, e.to_string()))?;
                    grants.insert(WorkspaceId::new(ws_k.clone()), lvl);
                }
            }

            let token = generate_ephemeral_capability_token()?;
            let claims = TokenClaims {
                label,
                expires_at_ms: now_ms.saturating_add(ttl_secs.saturating_mul(1_000)),
                can_create_headless,
                max_owned_workspaces,
                workspace_grants: grants,
                allow_clipboard_bridge_from: BTreeSet::new(),
            };
            state.token_store.insert(token.clone(), claims);
            Ok((json!({ "token": token.0 }), Vec::new()))
        }

        "auth.authenticate" => {
            let token_str = req
                .params
                .get("token")
                .and_then(Value::as_str)
                .ok_or_else(|| IpcError::new(ErrorSymbol::ErrInvalidParams, "missing token"))?;
            let token = CapabilityToken(token_str.to_owned());
            let claims = state.token_store.get(&token).cloned().ok_or_else(|| {
                IpcError::new(ErrorSymbol::ErrUnauthenticated, "unknown capability token")
            })?;
            if now_ms >= claims.expires_at_ms {
                return Err(IpcError::new(
                    ErrorSymbol::ErrUnauthenticated,
                    "capability token expired",
                ));
            }
            let session_id = SessionId::new(format!("sess_{conn_id:04x}"));
            if let Some(conn) = state.ipc_clients.get_mut(&conn_id) {
                conn.auth = PeerAuthState::AgentAuthenticated {
                    session_id: session_id.clone(),
                    token,
                };
            }
            Ok((
                json!({
                    "session_id": session_id,
                    "can_create_headless": claims.can_create_headless,
                    "grants": claims.workspace_grants,
                }),
                Vec::new(),
            ))
        }

        "ws.create" => {
            let (session_id, agent_token) = resolve_caller_identity(&auth_snapshot)?;
            if let Some(tok) = &agent_token {
                let claims = state.token_store.get(tok).ok_or_else(|| {
                    IpcError::new(ErrorSymbol::ErrUnauthenticated, "revoked token")
                })?;
                if !claims.can_create_headless {
                    return Err(IpcError::new(
                        ErrorSymbol::ErrCapabilityDenied,
                        "token lacks can_create_headless permission",
                    ));
                }
                let owned_count = claims
                    .workspace_grants
                    .values()
                    .filter(|&&g| g == PrivilegeLevel::Owner)
                    .count();
                if owned_count >= claims.max_owned_workspaces {
                    return Err(IpcError::new(
                        ErrorSymbol::ErrQuotaExceeded,
                        format!(
                            "exceeded max_owned_workspaces ({})",
                            claims.max_owned_workspaces
                        ),
                    ));
                }
            }

            let name = req
                .params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("headless")
                .to_owned();
            let viewport: LogicalViewport = req
                .params
                .get("viewport")
                .cloned()
                .and_then(|v| serde_json::from_value(v).ok())
                .unwrap_or_default();
            let acquire_lease = req
                .params
                .get("acquire_lease")
                .and_then(Value::as_bool)
                .unwrap_or(true);

            let idx = state.workspaces.len() + 1;
            let ws_id = WorkspaceId::new(format!("ws_{idx:02}"));
            let vseat_id = SeatId::new(format!("vseat_{idx:02}"));
            let fbo_slot = state.fbo_pool.acquire_or_borrow(&ws_id);

            let mut ws = WorkspaceState::new(
                ws_id.clone(),
                name.clone(),
                vseat_id.clone(),
                viewport,
                PresentationTarget::Offscreen {
                    lazy_fbo_slot: fbo_slot,
                },
                LayoutMode::Deck,
                now_ms,
            );
            let epoch = if acquire_lease {
                ws.lease.acquire(session_id, now_ms, 60_000)?
            } else {
                ws.lease.epoch()
            };

            state.offscreen_surfaces.insert(
                ws_id.clone(),
                LazyOffscreenSurface::new(
                    viewport.width as i32,
                    viewport.height as i32,
                    viewport.scale,
                ),
            );
            state
                .vseats
                .insert(vseat_id.clone(), VirtualSeatLedger::new(vseat_id.clone()));
            state.workspaces.insert(ws_id.clone(), ws);

            if let Some(tok) = agent_token
                && let Some(claims) = state.token_store.get_mut(&tok)
            {
                claims
                    .workspace_grants
                    .insert(ws_id.clone(), PrivilegeLevel::Owner);
            }

            state.broadcast_workspace_event(
                EventTopic::WorkspaceLifecycle,
                &ws_id,
                epoch,
                WorkspaceEvent::Created { name },
            );

            Ok((
                json!({
                    "ws_id": ws_id,
                    "epoch": epoch,
                    "vseat_id": vseat_id,
                }),
                Vec::new(),
            ))
        }

        "lease.acquire" => {
            let ws_id = parse_ws_id(&req.params)?;
            let ttl_ms = req
                .params
                .get("ttl_ms")
                .and_then(Value::as_u64)
                .unwrap_or(60_000);
            let (session_id, _) = authorize_peer_for_ws(
                state,
                &auth_snapshot,
                &ws_id,
                PrivilegeLevel::Control,
                now_ms,
            )?;
            let ws = state
                .workspaces
                .get_mut(&ws_id)
                .ok_or_else(|| IpcError::new(ErrorSymbol::ErrNotFound, "unknown workspace"))?;
            let epoch = ws.lease.acquire(session_id.clone(), now_ms, ttl_ms)?;
            state.broadcast_workspace_event(
                EventTopic::WorkspaceLease,
                &ws_id,
                epoch,
                WorkspaceEvent::EpochChanged {
                    new_epoch: epoch,
                    holder: Some(session_id),
                },
            );
            Ok((
                json!({
                    "epoch": epoch,
                    "expires_at_ms": now_ms.saturating_add(ttl_ms),
                }),
                Vec::new(),
            ))
        }

        "lease.release" => {
            let ws_id = parse_ws_id(&req.params)?;
            let expected_epoch = parse_expected_epoch(&req.params)?;
            let (session_id, _) = authorize_peer_for_ws(
                state,
                &auth_snapshot,
                &ws_id,
                PrivilegeLevel::Control,
                now_ms,
            )?;
            let new_epoch = {
                let ws = state
                    .workspaces
                    .get_mut(&ws_id)
                    .ok_or_else(|| IpcError::new(ErrorSymbol::ErrNotFound, "unknown workspace"))?;
                ws.lease.release(&session_id, expected_epoch)?
            };
            crate::macro_runtime::release_all_vseat_held_inputs(state, &ws_id);
            state.pointer_animations.retain(|a| a.ws_id != ws_id);
            state.broadcast_workspace_event(
                EventTopic::WorkspaceLease,
                &ws_id,
                new_epoch,
                WorkspaceEvent::EpochChanged {
                    new_epoch,
                    holder: None,
                },
            );
            Ok((json!({ "epoch": new_epoch }), Vec::new()))
        }

        "ws.set_mode" => {
            let params: WorkspaceSetModeParams = serde_json::from_value(req.params)
                .map_err(|e| IpcError::new(ErrorSymbol::ErrInvalidParams, e.to_string()))?;
            if params.mode == WorkspaceModeSpec::Takeover && channel != SocketChannel::Admin {
                return Err(IpcError::new(
                    ErrorSymbol::ErrCapabilityDenied,
                    "human takeover mode requires koma-admin.sock",
                ));
            }
            let _ = authorize_peer_for_ws(
                state,
                &auth_snapshot,
                &params.ws_id,
                PrivilegeLevel::ObserveOnly,
                now_ms,
            )?;
            if params.mode == WorkspaceModeSpec::Takeover {
                let fx = state
                    .workspaces
                    .get_mut(&params.ws_id)
                    .ok_or_else(|| IpcError::new(ErrorSymbol::ErrNotFound, "unknown workspace"))?
                    .lease
                    .human_toggle_takeover(60_000, now_ms);
                if fx.flush_virtual_seat {
                    crate::macro_runtime::release_all_vseat_held_inputs(state, &params.ws_id);
                }
                if fx.cancel_lua_coroutines {
                    state.pointer_animations.retain(|a| a.ws_id != params.ws_id);
                    if let Some(ac) = state.agent_cursors.get_mut(&params.ws_id) {
                        ac.visual.on_preempted(now_ms);
                    }
                    state.queue_redraw();
                }
                return Ok((json!({ "epoch": fx.new_epoch }), Vec::new()));
            }
            let epoch = state
                .workspaces
                .get(&params.ws_id)
                .ok_or_else(|| IpcError::new(ErrorSymbol::ErrNotFound, "unknown workspace"))?
                .lease
                .epoch();
            // Peek mode preserves epoch unchanged (`IPC_SPEC.md` §4.1)!
            Ok((json!({ "epoch": epoch, "mode": params.mode }), Vec::new()))
        }

        "ws.focus" => {
            // Workspace navigation for local UI helpers (status bars, scripts).
            // Same door as `config.reload`: the admin socket, which is already gated
            // on peer credentials, plus the session lock gate applied above.
            if channel != SocketChannel::Admin {
                return Err(IpcError::new(
                    ErrorSymbol::ErrCapabilityDenied,
                    "ws.focus is restricted to koma-admin.sock",
                ));
            }
            let query = req
                .params
                .get("ws_id")
                .and_then(Value::as_str)
                .or_else(|| req.params.get("name").and_then(Value::as_str))
                .ok_or_else(|| IpcError::new(ErrorSymbol::ErrInvalidParams, "missing ws_id"))?
                .to_owned();

            // Resolve existing workspaces only: a bar click must not silently create
            // one. The keybind path keeps its create-on-demand behaviour. Exact ids win over
            // names, so a workspace merely *named* `ws_01` cannot shadow workspace id `ws_01`.
            if state.find_workspace_by_id_or_name(&query).is_none() {
                return Err(IpcError::new(
                    ErrorSymbol::ErrNotFound,
                    format!("unknown workspace {query}"),
                ));
            }

            crate::keybind_dispatcher::execute_action_descriptor(
                state,
                koma_lua::ActionDescriptor::WorkspaceFocus(query.clone()),
                false,
            );

            Ok((
                json!({
                    "focused": query,
                    "active_ws_id": state.active_ws_id.as_str(),
                }),
                Vec::new(),
            ))
        }

        "win.move_to_workspace" | "win.move" => {
            // Mirrors `ActionDescriptor::WindowMoveToWorkspace` for status bars: `lua.exec_tx` is
            // lease gated, so a bar cannot run the keybind action itself. Same door as `ws.focus`
            // (admin socket, peer credentials, plus the session lock gate applied above).
            if channel != SocketChannel::Admin {
                return Err(IpcError::new(
                    ErrorSymbol::ErrCapabilityDenied,
                    "win.move_to_workspace is restricted to koma-admin.sock",
                ));
            }
            let target = req
                .params
                .get("workspace")
                .and_then(Value::as_str)
                .or_else(|| req.params.get("ws_id").and_then(Value::as_str))
                .ok_or_else(|| IpcError::new(ErrorSymbol::ErrInvalidParams, "missing workspace"))?
                .to_owned();
            let follow = req
                .params
                .get("follow")
                .and_then(Value::as_bool)
                .unwrap_or(false);

            // Existing workspaces only: a bar click must not create one. Exact ids win over
            // names, matching `ws.focus`.
            let Some(dst_ws_id) = state.find_workspace_by_id_or_name(&target) else {
                return Err(IpcError::new(
                    ErrorSymbol::ErrNotFound,
                    format!("unknown workspace {target}"),
                ));
            };

            let src_ws_id = state.effective_interactive_ws_id();
            let active_win = state
                .workspaces
                .get(&src_ws_id)
                .and_then(WorkspaceState::active_window);
            let Some(win_id) = active_win else {
                return Err(IpcError::new(
                    ErrorSymbol::ErrNotFound,
                    format!("workspace {} has no active window", src_ws_id.as_str()),
                ));
            };

            let moved = state
                .move_active_window_between_workspaces_with_follow(&src_ws_id, &dst_ws_id, follow);
            Ok((
                json!({
                    "win_id": win_id.to_string(),
                    "from_ws_id": src_ws_id.as_str(),
                    "to_ws_id": dst_ws_id.as_str(),
                    "followed": follow && moved,
                    "moved": moved,
                    "active_ws_id": state.active_ws_id.as_str(),
                }),
                Vec::new(),
            ))
        }

        "scene.inventory" => {
            let (caller_sess, agent_tok) = resolve_caller_identity(&auth_snapshot)?;
            let mut list = Vec::new();
            for ws_id in state.ordered_workspace_ids() {
                let Some(ws) = state.workspaces.get(&ws_id) else {
                    continue;
                };
                if let Some(tok) = &agent_tok {
                    let allowed = state.token_store.get(tok).is_some_and(|c| {
                        c.authorize_workspace(&ws_id, PrivilegeLevel::ObserveOnly, now_ms, false)
                            .is_ok()
                    });
                    if !allowed {
                        continue;
                    }
                }
                let active_win = ws.active_window().and_then(|id| {
                    state
                        .mapped_windows
                        .iter()
                        .find(|m| m.win_id == id)
                        .map(|m| {
                            json!({
                                "win_id": m.win_id.to_string(),
                                "app_id": m.app_id,
                                "title": m.title,
                                "is_floating": m.is_floating,
                                "is_sandboxed": m.is_sandboxed,
                                "rect": m.rect,
                            })
                        })
                });
                let stash_wins: Vec<Value> = ws
                    .deck
                    .stash_windows()
                    .iter()
                    .filter_map(|id| {
                        state.mapped_windows.iter().find(|m| &m.win_id == id).map(|m| {
                            json!({ "win_id": m.win_id.to_string(), "app_id": m.app_id, "title": m.title, "is_sandboxed": m.is_sandboxed })
                        })
                    })
                    .collect();

                list.push(json!({
                    "ws_id": ws_id,
                    "name": ws.name,
                    "focused": ws_id == state.active_ws_id,
                    "is_special": ws.is_special,
                    "layout_mode": ws.layout_mode,
                    "frame_tier": ws.frame_clock.current_tier(now_ms),
                    "epoch": ws.lease.epoch(),
                    "is_human_preempted": ws.lease.is_human_preempted(),
                    "caller_session": caller_sess,
                    "viewport": ws.viewport,
                    "active_window": active_win,
                    "stash_windows": stash_wins,
                }));
            }
            // Output to workspace mapping, so "which workspace is monitor 2 on?" is answerable
            // without moving windows around (komawm/koma#95). Sorted for a stable document.
            let mut outputs: Vec<(&String, &WorkspaceId)> =
                state.output_workspaces.iter().collect();
            outputs.sort_by(|a, b| a.0.cmp(b.0));
            let outputs: Vec<Value> = outputs
                .into_iter()
                .map(|(name, ws_id)| {
                    json!({
                        "name": name,
                        "ws_id": ws_id,
                        "ws_name": state.workspaces.get(ws_id).map(|ws| ws.name.clone()),
                        "focused": state.focused_output_name.as_deref() == Some(name.as_str()),
                    })
                })
                .collect();
            Ok((
                json!({ "workspaces": list, "outputs": outputs }),
                Vec::new(),
            ))
        }

        "capture.frame" => {
            let params: CaptureFrameParams = serde_json::from_value(req.params)
                .map_err(|e| IpcError::new(ErrorSymbol::ErrInvalidParams, e.to_string()))?;
            let _ = authorize_peer_for_ws(
                state,
                &auth_snapshot,
                &params.ws_id,
                PrivilegeLevel::ObserveOnly,
                now_ms,
            )?;

            execute_zero_copy_capture_frame(state, &params)
        }

        "lease.heartbeat" => {
            let ws_id = parse_ws_id(&req.params)?;
            let expected_epoch = parse_expected_epoch(&req.params)?;
            let ttl_ms = req
                .params
                .get("ttl_ms")
                .and_then(Value::as_u64)
                .unwrap_or(60_000);
            let (session_id, grant) = authorize_peer_for_ws(
                state,
                &auth_snapshot,
                &ws_id,
                PrivilegeLevel::Control,
                now_ms,
            )?;
            let ws = state
                .workspaces
                .get_mut(&ws_id)
                .ok_or_else(|| IpcError::new(ErrorSymbol::ErrNotFound, "unknown workspace"))?;
            ws.lease
                .validate_mutation_gate(&session_id, grant, expected_epoch, None, now_ms)?;
            let epoch = ws.lease.acquire(session_id, now_ms, ttl_ms)?;
            Ok((json!({ "epoch": epoch }), Vec::new()))
        }

        "win.activate" => {
            let ws_id = parse_ws_id(&req.params)?;
            let expected_epoch = parse_expected_epoch(&req.params)?;
            let slot = req
                .params
                .get("win_slot")
                .and_then(Value::as_u64)
                .ok_or_else(|| IpcError::new(ErrorSymbol::ErrInvalidParams, "missing win_slot"))?;
            let generation = req
                .params
                .get("win_generation")
                .and_then(Value::as_u64)
                .unwrap_or(1) as u32;
            let win_id = koma_proto::WindowId::new(slot, generation);

            let (session_id, grant) = authorize_peer_for_ws(
                state,
                &auth_snapshot,
                &ws_id,
                PrivilegeLevel::Control,
                now_ms,
            )?;
            let activated = {
                let ws = state
                    .workspaces
                    .get_mut(&ws_id)
                    .ok_or_else(|| IpcError::new(ErrorSymbol::ErrNotFound, "unknown workspace"))?;
                ws.lease.validate_mutation_gate(
                    &session_id,
                    grant,
                    expected_epoch,
                    Some(60_000),
                    now_ms,
                )?;
                ws.activate_window(win_id, now_ms)
            };
            state.sync_workspace_geometries(&ws_id);
            if !activated {
                return Err(IpcError::new(
                    ErrorSymbol::ErrNotFound,
                    format!("window {win_id} not found in workspace {ws_id}"),
                ));
            }
            state.focus_active_deck_window();
            state.queue_redraw();
            let cur_epoch = state.workspaces[&ws_id].lease.epoch();
            Ok((
                json!({ "activated": win_id.to_string(), "epoch": cur_epoch }),
                Vec::new(),
            ))
        }

        "vseat.pointer_move" => {
            let ws_id = parse_ws_id(&req.params)?;
            let expected_epoch = parse_expected_epoch(&req.params)?;
            let auto_ttl = req
                .params
                .get("auto_acquire")
                .and_then(Value::as_bool)
                .unwrap_or(true)
                .then_some(60_000);
            let target = parse_ipc_anchor(req.params.get("target").unwrap_or(&req.params))?;
            let kinematics = parse_ipc_kinematics(&req.params);
            let active_epoch = validate_vseat_mutation_gate(
                state,
                &auth_snapshot,
                &ws_id,
                expected_epoch,
                auto_ttl,
                now_ms,
            )?;
            crate::macro_runtime::queue_pointer_trajectory(
                state,
                ws_id.clone(),
                active_epoch,
                target,
                &kinematics,
                None,
            )?;
            let disposition = vseat_response_disposition(state, &ws_id);
            Ok((
                json!({ "ok": true, "epoch": active_epoch, "disposition": disposition }),
                Vec::new(),
            ))
        }

        "vseat.relative_move" => {
            let params: RelativeMoveParams = serde_json::from_value(req.params)
                .map_err(|e| IpcError::new(ErrorSymbol::ErrInvalidParams, e.to_string()))?;
            let active_epoch = validate_vseat_mutation_gate(
                state,
                &auth_snapshot,
                &params.ws_id,
                params.expected_epoch,
                Some(60_000),
                now_ms,
            )?;
            let cursor_pos = crate::macro_runtime::apply_vseat_relative_move(
                state,
                &params.ws_id,
                active_epoch,
                params.dx,
                params.dy,
                params.dx_unaccel,
                params.dy_unaccel,
                &params.modifiers,
            )?;
            Ok((
                json!({
                    "ok": true,
                    "epoch": active_epoch,
                    "cursor_pos": [cursor_pos.x, cursor_pos.y],
                    "disposition": DeliverySemantics::delivered_verified(),
                }),
                Vec::new(),
            ))
        }

        "vseat.wheel" => {
            let params: WheelScrollParams = serde_json::from_value(req.params)
                .map_err(|e| IpcError::new(ErrorSymbol::ErrInvalidParams, e.to_string()))?;
            let target = params.target.as_ref().map(parse_ipc_anchor).transpose()?;
            let active_epoch = validate_vseat_mutation_gate(
                state,
                &auth_snapshot,
                &params.ws_id,
                params.expected_epoch,
                Some(60_000),
                now_ms,
            )?;
            crate::macro_runtime::apply_vseat_wheel(
                state,
                &params.ws_id,
                active_epoch,
                target,
                params.dx,
                params.dy,
                params.discrete_x,
                params.discrete_y,
                &params.modifiers,
            )?;
            Ok((
                json!({
                    "ok": true,
                    "epoch": active_epoch,
                    "disposition": DeliverySemantics::delivered_verified(),
                }),
                Vec::new(),
            ))
        }

        "vseat.click" => {
            let ws_id = parse_ws_id(&req.params)?;
            let expected_epoch = parse_expected_epoch(&req.params)?;
            let auto_ttl = req
                .params
                .get("auto_acquire")
                .and_then(Value::as_bool)
                .unwrap_or(true)
                .then_some(60_000);
            let target = if let Some(cell) = req.params.get("grid_225").and_then(Value::as_str) {
                koma_model::AnchorPoint::Grid225(cell.to_owned())
            } else if let Some(target_val) = req.params.get("target") {
                parse_ipc_anchor(target_val)?
            } else {
                let x = req.params.get("x").and_then(Value::as_f64).unwrap_or(0.0);
                let y = req.params.get("y").and_then(Value::as_f64).unwrap_or(0.0);
                koma_model::AnchorPoint::AnchorOffset {
                    anchor: koma_model::Anchor9::TopLeft,
                    dx: x,
                    dy: y,
                }
            };
            let button = req
                .params
                .get("button")
                .and_then(Value::as_str)
                .unwrap_or("left")
                .to_owned();
            let clicks = req
                .params
                .get("clicks")
                .and_then(Value::as_u64)
                .unwrap_or(1) as u8;
            let modifiers = parse_ipc_modifiers(&req.params)?;
            let label = parse_ipc_label(&req.params)?;

            let active_epoch = validate_vseat_mutation_gate(
                state,
                &auth_snapshot,
                &ws_id,
                expected_epoch,
                auto_ttl,
                now_ms,
            )?;
            crate::macro_runtime::apply_vseat_click_with_modifiers(
                state,
                &ws_id,
                active_epoch,
                target,
                &button,
                clicks,
                &modifiers,
                label,
            )?;
            Ok((
                json!({
                    "ok": true,
                    "epoch": active_epoch,
                    "disposition": DeliverySemantics::delivered_verified(),
                }),
                Vec::new(),
            ))
        }

        "vseat.drag" => {
            let ws_id = parse_ws_id(&req.params)?;
            let expected_epoch = parse_expected_epoch(&req.params)?;
            let from_pt = parse_ipc_anchor(req.params.get("from").unwrap_or(&Value::Null))?;
            let to_pt = parse_ipc_anchor(req.params.get("to").unwrap_or(&Value::Null))?;
            let button = req
                .params
                .get("button")
                .and_then(Value::as_str)
                .unwrap_or("left")
                .to_owned();
            let modifiers = parse_ipc_modifiers(&req.params)?;
            let label = parse_ipc_label(&req.params)?;
            let modulation = parse_ipc_modulation(&req.params)?;
            let kinematics = parse_ipc_kinematics(&req.params);

            let active_epoch = validate_vseat_mutation_gate(
                state,
                &auth_snapshot,
                &ws_id,
                expected_epoch,
                Some(60_000),
                now_ms,
            )?;
            crate::macro_runtime::queue_pointer_path_trajectory(
                state,
                ws_id.clone(),
                active_epoch,
                vec![from_pt, to_pt],
                crate::macro_runtime::PointerPathLaunchOpts {
                    spline: koma_model::PathSplineKind::Auto,
                    kinematics,
                    press_button_at_start: Some(button.clone()),
                    release_button_on_finish: Some(button),
                    modifiers,
                    label,
                    modulation,
                    ..Default::default()
                },
            )?;
            let disposition = vseat_response_disposition(state, &ws_id);
            Ok((
                json!({ "ok": true, "epoch": active_epoch, "disposition": disposition }),
                Vec::new(),
            ))
        }

        "vseat.path" | "vseat.stroke" => {
            let ws_id = parse_ws_id(&req.params)?;
            let expected_epoch = parse_expected_epoch(&req.params)?;
            let auto_ttl = req
                .params
                .get("auto_acquire")
                .and_then(Value::as_bool)
                .unwrap_or(true)
                .then_some(60_000);

            let raw_waypoints = req
                .params
                .get("waypoints")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    IpcError::new(ErrorSymbol::ErrInvalidParams, "waypoints array is required")
                })?;
            if !(2..=128).contains(&raw_waypoints.len()) {
                return Err(IpcError::new(
                    ErrorSymbol::ErrInvalidParams,
                    format!(
                        "waypoints must contain 2..=128 anchors (got {})",
                        raw_waypoints.len()
                    ),
                ));
            }
            let mut waypoints = Vec::with_capacity(raw_waypoints.len());
            for item in raw_waypoints {
                waypoints.push(parse_ipc_anchor(item)?);
            }

            let spline: koma_model::PathSplineKind = match req.params.get("spline") {
                None | Some(Value::Null) => koma_model::PathSplineKind::default(),
                Some(v) => serde_json::from_value(v.clone()).map_err(|e| {
                    IpcError::new(
                        ErrorSymbol::ErrInvalidParams,
                        format!("invalid spline: {e}"),
                    )
                })?,
            };
            let explicit_button = req
                .params
                .get("button")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            let button = if req.method == "vseat.stroke" {
                Some(explicit_button.unwrap_or_else(|| "left".to_owned()))
            } else {
                explicit_button
            };
            let hold_start_ms = req
                .params
                .get("hold_start_ms")
                .or_else(|| req.params.get("hold_before_move_ms"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let hold_end_ms = req
                .params
                .get("hold_end_ms")
                .or_else(|| req.params.get("hold_before_release_ms"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let modifiers = parse_ipc_modifiers(&req.params)?;
            let label = parse_ipc_label(&req.params)?;
            let modulation = parse_ipc_modulation(&req.params)?;
            let kinematics = parse_ipc_kinematics(&req.params);

            let active_epoch = validate_vseat_mutation_gate(
                state,
                &auth_snapshot,
                &ws_id,
                expected_epoch,
                auto_ttl,
                now_ms,
            )?;

            let count = crate::macro_runtime::queue_pointer_path_trajectory(
                state,
                ws_id.clone(),
                active_epoch,
                waypoints,
                crate::macro_runtime::PointerPathLaunchOpts {
                    spline,
                    kinematics,
                    hold_start_ms,
                    hold_end_ms,
                    press_button_at_start: button.clone(),
                    release_button_on_finish: button,
                    modifiers,
                    label,
                    modulation,
                },
            )?;
            let disposition = vseat_response_disposition(state, &ws_id);
            Ok((
                json!({
                    "ok": true,
                    "epoch": active_epoch,
                    "waypoints": count,
                    "disposition": disposition,
                }),
                Vec::new(),
            ))
        }

        "vseat.hit_test" => {
            let params: HitTestParams = serde_json::from_value(req.params)
                .map_err(|e| IpcError::new(ErrorSymbol::ErrInvalidParams, e.to_string()))?;
            let _ = authorize_peer_for_ws(
                state,
                &auth_snapshot,
                &params.ws_id,
                PrivilegeLevel::ObserveOnly,
                now_ms,
            )?;
            let ws = state
                .workspaces
                .get(&params.ws_id)
                .ok_or_else(|| IpcError::new(ErrorSymbol::ErrNotFound, "unknown workspace"))?;
            let target_anchor = parse_ipc_anchor(&params.target)?;
            let active_win_id = ws.active_window();
            let ws_full_rect = ws.viewport.full_rect();
            let target_mapped =
                active_win_id.and_then(|wid| state.mapped_windows.iter().find(|m| m.win_id == wid));
            let target_rect = target_mapped.map(|m| m.rect).unwrap_or(ws_full_rect);
            let target_pt = target_anchor.resolve_in(target_rect).map_err(|e| {
                IpcError::new(
                    ErrorSymbol::ErrInvalidParams,
                    format!("invalid target anchor: {e}"),
                )
            })?;

            let margin_x = if target_rect.w >= 3 { 1.0 } else { 0.0 };
            let margin_y = if target_rect.h >= 3 { 1.0 } else { 0.0 };
            let min_x = f64::from(target_rect.x) + margin_x;
            let min_y = f64::from(target_rect.y) + margin_y;
            let max_x = (f64::from(target_rect.x + target_rect.w as i32) - margin_x).max(min_x);
            let max_y = (f64::from(target_rect.y + target_rect.h as i32) - margin_y).max(min_y);
            let (cx, cy) = (target_pt.x, target_pt.y);
            let sampled_points = vec![
                [cx, cy],
                [
                    (cx - 16.0).clamp(min_x, max_x),
                    (cy - 16.0).clamp(min_y, max_y),
                ],
                [
                    (cx + 16.0).clamp(min_x, max_x),
                    (cy - 16.0).clamp(min_y, max_y),
                ],
                [
                    (cx - 16.0).clamp(min_x, max_x),
                    (cy + 16.0).clamp(min_y, max_y),
                ],
                [
                    (cx + 16.0).clamp(min_x, max_x),
                    (cy + 16.0).clamp(min_y, max_y),
                ],
            ];

            let mut primary_hit_target = false;
            let mut blocker = None;
            for (idx, [px, py]) in sampled_points.iter().enumerate() {
                let hit_wid = state
                    .surface_under_in_workspace(
                        &params.ws_id,
                        smithay::utils::Point::from((*px, *py)),
                    )
                    .map(|(wid, _, _)| wid);
                if idx == 0 {
                    primary_hit_target = hit_wid.is_some() && hit_wid == active_win_id;
                }
                if blocker.is_none()
                    && let Some(wid) = hit_wid
                    && Some(wid) != active_win_id
                {
                    blocker = Some(wid);
                }
            }

            let blocker_mapped =
                blocker.and_then(|wid| state.mapped_windows.iter().find(|m| m.win_id == wid));
            let result = HitTestResult {
                reaches_target: primary_hit_target && blocker.is_none(),
                target_win_id: target_mapped.map(|m| m.win_id),
                target_app_id: target_mapped.map(|m| m.app_id.clone()),
                blocking_win_id: blocker,
                blocking_app_id: blocker_mapped.map(|m| m.app_id.clone()),
                blocking_title: blocker_mapped.map(|m| m.title.clone()),
                blocking_rect: blocker_mapped
                    .map(|m| [m.rect.x, m.rect.y, m.rect.w as i32, m.rect.h as i32]),
                geometry_stable: !state.ws_has_active_layout_animation(&params.ws_id),
                sampled_points,
                cursor_pos: state
                    .agent_cursors
                    .get(&params.ws_id)
                    .map(|ac| [ac.ws_local_pos.x, ac.ws_local_pos.y]),
            };
            let json_res = serde_json::to_value(result).map_err(|e| {
                IpcError::new(
                    ErrorSymbol::ErrInvalidParams,
                    format!("failed serializing hit_test result: {e}"),
                )
            })?;
            Ok((json_res, Vec::new()))
        }

        "vseat.cursor_label" => {
            let params: CursorLabelParams = serde_json::from_value(req.params)
                .map_err(|e| IpcError::new(ErrorSymbol::ErrInvalidParams, e.to_string()))?;
            let _ = authorize_peer_for_ws(
                state,
                &auth_snapshot,
                &params.ws_id,
                PrivilegeLevel::Control,
                now_ms,
            )?;
            if !state.workspaces.contains_key(&params.ws_id) {
                return Err(IpcError::new(ErrorSymbol::ErrNotFound, "unknown workspace"));
            }
            let caret_rect = match params.caret_rect {
                None => None,
                Some([x, y, w, h]) => {
                    if !x.is_finite()
                        || !y.is_finite()
                        || !w.is_finite()
                        || !h.is_finite()
                        || w <= 0.0
                        || h <= 0.0
                    {
                        return Err(IpcError::new(
                            ErrorSymbol::ErrInvalidParams,
                            "caret_rect must have finite [x, y, w, h] with w > 0 and h > 0",
                        ));
                    }
                    Some(koma_model::LogicalRect::new(
                        x.round() as i32,
                        y.round() as i32,
                        (w.round() as u32).max(1),
                        (h.round() as u32).max(1),
                    ))
                }
            };
            crate::macro_runtime::set_agent_cursor_label(
                state,
                &params.ws_id,
                params.label,
                caret_rect,
            );
            Ok((json!({ "ok": true }), Vec::new()))
        }

        "vseat.cursor_settle" => {
            let ws_id = parse_ws_id(&req.params)?;
            let immediate = req
                .params
                .get("immediate")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let _ = authorize_peer_for_ws(
                state,
                &auth_snapshot,
                &ws_id,
                PrivilegeLevel::Control,
                now_ms,
            )?;
            if !state.workspaces.contains_key(&ws_id) {
                return Err(IpcError::new(ErrorSymbol::ErrNotFound, "unknown workspace"));
            }
            crate::macro_runtime::settle_agent_cursor(state, &ws_id, immediate);
            Ok((json!({ "ok": true }), Vec::new()))
        }

        "vseat.key" => {
            let ws_id = parse_ws_id(&req.params)?;
            let expected_epoch = parse_expected_epoch(&req.params)?;
            let chord = req
                .params
                .get("chord")
                .and_then(Value::as_str)
                .ok_or_else(|| IpcError::new(ErrorSymbol::ErrInvalidParams, "missing chord"))?
                .to_owned();
            let (session_id, grant) = authorize_peer_for_ws(
                state,
                &auth_snapshot,
                &ws_id,
                PrivilegeLevel::Control,
                now_ms,
            )?;
            let active_epoch = {
                let ws = state
                    .workspaces
                    .get_mut(&ws_id)
                    .ok_or_else(|| IpcError::new(ErrorSymbol::ErrNotFound, "unknown workspace"))?;
                ws.lease.validate_mutation_gate(
                    &session_id,
                    grant,
                    expected_epoch,
                    Some(60_000),
                    now_ms,
                )?
            };

            crate::macro_runtime::dispatch_single_host_action(
                state,
                koma_lua::phodopus_bridge::QueuedHostAction::TapKeyChord {
                    ws_id,
                    expected_epoch: active_epoch,
                    chord,
                    hold_ms: 18,
                },
            );
            Ok((
                json!({
                    "ok": true,
                    "epoch": active_epoch,
                    "disposition": DeliverySemantics::delivered_verified(),
                }),
                Vec::new(),
            ))
        }

        "vseat.inject_text" => {
            let ws_id = parse_ws_id(&req.params)?;
            let expected_epoch = parse_expected_epoch(&req.params)?;
            let text = req
                .params
                .get("text")
                .and_then(Value::as_str)
                .ok_or_else(|| IpcError::new(ErrorSymbol::ErrInvalidParams, "missing text"))?
                .to_owned();
            let strategy: koma_proto::TextInjectStrategy = req
                .params
                .get("strategy")
                .cloned()
                .and_then(|v| serde_json::from_value(v).ok())
                .unwrap_or(koma_proto::TextInjectStrategy::Auto);

            let (session_id, grant) = authorize_peer_for_ws(
                state,
                &auth_snapshot,
                &ws_id,
                PrivilegeLevel::Control,
                now_ms,
            )?;
            let active_epoch = {
                let ws = state
                    .workspaces
                    .get_mut(&ws_id)
                    .ok_or_else(|| IpcError::new(ErrorSymbol::ErrNotFound, "unknown workspace"))?;
                ws.lease.validate_mutation_gate(
                    &session_id,
                    grant,
                    expected_epoch,
                    Some(60_000),
                    now_ms,
                )?
            };

            crate::macro_runtime::dispatch_single_host_action(
                state,
                koma_lua::phodopus_bridge::QueuedHostAction::InjectText {
                    ws_id,
                    expected_epoch: active_epoch,
                    utf8: text,
                    strategy,
                },
            );
            Ok((
                json!({
                    "ok": true,
                    "epoch": active_epoch,
                    "disposition": DeliverySemantics::delivered_verified(),
                }),
                Vec::new(),
            ))
        }

        "vseat.paste_mime" => {
            let ws_id = parse_ws_id(&req.params)?;
            let expected_epoch = parse_expected_epoch(&req.params)?;
            let mime_type = req
                .params
                .get("mime_type")
                .and_then(Value::as_str)
                .unwrap_or("text/tab-separated-values")
                .to_owned();
            let data_utf8 = req
                .params
                .get("data_utf8")
                .and_then(Value::as_str)
                .ok_or_else(|| IpcError::new(ErrorSymbol::ErrInvalidParams, "missing data_utf8"))?
                .to_owned();

            let (session_id, grant) = authorize_peer_for_ws(
                state,
                &auth_snapshot,
                &ws_id,
                PrivilegeLevel::Control,
                now_ms,
            )?;
            let active_epoch = {
                let ws = state
                    .workspaces
                    .get_mut(&ws_id)
                    .ok_or_else(|| IpcError::new(ErrorSymbol::ErrNotFound, "unknown workspace"))?;
                ws.lease.validate_mutation_gate(
                    &session_id,
                    grant,
                    expected_epoch,
                    Some(60_000),
                    now_ms,
                )?
            };

            crate::macro_runtime::dispatch_single_host_action(
                state,
                koma_lua::phodopus_bridge::QueuedHostAction::PasteMime {
                    ws_id,
                    expected_epoch: active_epoch,
                    mime_type,
                    payload: data_utf8.into_bytes(),
                },
            );
            Ok((json!({ "ok": true, "epoch": active_epoch }), Vec::new()))
        }

        "macro.list" => {
            let cfg = state.lua_sandbox.config_snapshot();
            let macros: Vec<_> = cfg.macros.values().filter(|m| m.enabled).cloned().collect();
            Ok((json!({ "macros": macros }), Vec::new()))
        }

        "lua.exec_tx" => {
            let params: LuaExecTxParams = serde_json::from_value(req.params)
                .map_err(|e| IpcError::new(ErrorSymbol::ErrInvalidParams, e.to_string()))?;
            let (session_id, grant) = authorize_peer_for_ws(
                state,
                &auth_snapshot,
                &params.ws_id,
                PrivilegeLevel::Control,
                now_ms,
            )?;
            {
                let ws = state
                    .workspaces
                    .get_mut(&params.ws_id)
                    .ok_or_else(|| IpcError::new(ErrorSymbol::ErrNotFound, "unknown workspace"))?;
                ws.lease.validate_mutation_gate(
                    &session_id,
                    grant,
                    params.expected_epoch,
                    None,
                    now_ms,
                )?;
            }

            let snap = state.capture_lua_desktop_snapshot(Some(&params.ws_id));
            state.lua_sandbox.set_desktop_snapshot(snap);
            let (_stashed_exec, mode) = state.lua_sandbox.start_macro_tx(
                params.ws_id.clone(),
                params.expected_epoch,
                &params.macro_id,
                &params.args,
                now_ms,
            )?;
            crate::macro_runtime::execute_queued_host_actions(state);

            Ok((
                json!({
                    "status": format!("{mode:?}"),
                    "epoch": params.expected_epoch,
                }),
                Vec::new(),
            ))
        }

        "events.subscribe" => {
            let ws_list: Vec<WorkspaceId> = req
                .params
                .get("workspaces")
                .cloned()
                .and_then(|v| serde_json::from_value(v).ok())
                .unwrap_or_default();
            let topics: Vec<EventTopic> = req
                .params
                .get("topics")
                .cloned()
                .and_then(|v| serde_json::from_value(v).ok())
                .unwrap_or_default();

            for ws_id in &ws_list {
                let _ = authorize_peer_for_ws(
                    state,
                    &auth_snapshot,
                    ws_id,
                    PrivilegeLevel::ObserveOnly,
                    now_ms,
                )?;
            }

            if let Some(conn) = state.ipc_clients.get_mut(&conn_id) {
                conn.subscribed_workspaces = ws_list.into_iter().collect();
                conn.subscribed_topics = topics.into_iter().collect();
            }
            Ok((json!({ "subscribed": true }), Vec::new()))
        }

        "sandbox.create_wayland_jail" => {
            if channel != SocketChannel::Admin {
                return Err(IpcError::new(
                    ErrorSymbol::ErrCapabilityDenied,
                    "sandbox.create_wayland_jail is restricted to koma-admin.sock",
                ));
            }
            let ws_id = parse_ws_id(&req.params)?;
            if !state.workspaces.contains_key(&ws_id) {
                return Err(IpcError::new(
                    ErrorSymbol::ErrNotFound,
                    format!("unknown workspace `{ws_id}`"),
                ));
            }
            let label = req
                .params
                .get("label")
                .and_then(Value::as_str)
                .unwrap_or("agent-container")
                .to_owned();
            let loop_handle = state.loop_handle.clone().ok_or_else(|| {
                IpcError::new(ErrorSymbol::ErrInvalidParams, "missing calloop LoopHandle")
            })?;
            let sock_path = crate::security_context::create_jailed_wayland_socket(
                &loop_handle,
                state,
                ws_id.clone(),
                label.clone(),
            )
            .map_err(|e| IpcError::new(ErrorSymbol::ErrInvalidParams, e.to_string()))?;

            Ok((
                json!({
                    "ws_id": ws_id,
                    "label": label,
                    "restricted": true,
                    "socket_path": sock_path,
                }),
                Vec::new(),
            ))
        }

        "sandbox.revoke_wayland_jail" => {
            if channel != SocketChannel::Admin {
                return Err(IpcError::new(
                    ErrorSymbol::ErrCapabilityDenied,
                    "sandbox.revoke_wayland_jail is restricted to koma-admin.sock",
                ));
            }
            let ws_id = parse_ws_id(&req.params)?;
            let revoked = crate::security_context::revoke_jailed_wayland_socket(state, &ws_id);
            Ok((json!({ "ws_id": ws_id, "revoked": revoked }), Vec::new()))
        }

        "config.reload" => {
            if channel != SocketChannel::Admin {
                return Err(IpcError::new(
                    ErrorSymbol::ErrCapabilityDenied,
                    "config.reload is restricted to koma-admin.sock",
                ));
            }
            let explicit_path = req
                .params
                .get("path")
                .and_then(Value::as_str)
                .map(PathBuf::from);
            let report = crate::config_watcher::reload_active_config_now(state, explicit_path)
                .map_err(|msg| IpcError::new(ErrorSymbol::ErrInvalidParams, msg))?;
            Ok((
                json!({
                    "ok": true,
                    "generation": report.generation,
                    "path": report.init_path,
                    "binds": report.binds_count,
                    "gestures": report.gestures_count,
                    "window_rules": report.window_rules_count,
                    "macros": report.macros_count,
                    "monitors": report.monitors_count,
                    "deferred_vm_swap": report.deferred_vm_swap,
                }),
                Vec::new(),
            ))
        }

        "config.status" => {
            let active_path = state.config_path.active_init_path();
            let input = state.lua_sandbox.config_snapshot().input;
            Ok((
                json!({
                    "generation": state.config_generation,
                    "reload_failures": state.config_reload_failures,
                    "active_path": active_path,
                    "last_error": state.config_error_banner,
                    "deferred_vm_pending": state.deferred_lua_sandbox.is_some(),
                    "locked": state.is_locked_or_abandoned(),
                    "lock_phase": state.session_lock.phase_label(),
                    "input": config_status_input_json(&input),
                    "frame_perf": state.frame_perf.snapshot_json(),
                }),
                Vec::new(),
            ))
        }

        other => Err(IpcError::new(
            ErrorSymbol::ErrNotFound,
            format!("unknown IPC method `{other}`"),
        )),
    }
}

/// Executes **Render-on-Read** (`LazyOffscreenSurface::capture_rgba8_pixels`), enforces the
/// Protected Surface `#000000` blackout invariant, seals the pixels into a read-only `memfd`,
/// and attaches the `OwnedFd` at `SCM_RIGHTS` `fd_index = 0` (`IPC_SPEC.md` §5.4).
fn execute_zero_copy_capture_frame(
    state: &mut KomaState,
    params: &CaptureFrameParams,
) -> Result<(Value, Vec<OwnedFd>), IpcError> {
    state.reconcile_session_lock();
    if state.is_locked_or_abandoned() {
        return Err(IpcError::new(
            ErrorSymbol::ErrCapabilityDenied,
            "session is locked",
        ));
    }
    let (viewport, epoch, lease_state) = {
        let ws = state
            .workspaces
            .get(&params.ws_id)
            .ok_or_else(|| IpcError::new(ErrorSymbol::ErrNotFound, "unknown workspace"))?;
        (ws.viewport, ws.lease.epoch(), ws.lease.state().clone())
    };

    // Temporarily move out `backend` and `offscreen_surfaces` (matching `render_pending_tty_outputs`)
    // so `scene_items` can borrow `&*state` without aliasing mutable renderer/FBO state.
    let mut backend = state.backend.take();
    let mut offscreen_surfaces = std::mem::take(&mut state.offscreen_surfaces);
    let scene_items: Vec<WindowSceneItem<'_>> = if let Some(wid) = params.win_id {
        state
            .mapped_windows
            .iter()
            .find(|m| m.win_id == wid)
            .map(|mapped| {
                let is_protected = state
                    .rules
                    .resolve_dynamic(&WindowMatchContext {
                        app_id: &mapped.app_id,
                        title: &mapped.title,
                        is_floating: mapped.is_floating,
                        is_focused: true,
                        is_xwayland: false,
                        is_modal_dialog: false,
                        is_screencast_target: true,
                        is_agent_workspace: true,
                    })
                    .block_out_from
                    .is_some_and(|p| p.blocks_agent_capture());
                let appearance = &state.lua_sandbox.config_snapshot().appearance;
                vec![WindowSceneItem {
                    win_id: mapped.win_id,
                    window: &mapped.window,
                    rect: mapped.rect,
                    is_floating: mapped.is_floating,
                    is_focused: true,
                    focus_factor: 1.0,
                    is_protected_surface: is_protected,
                    opacity: 1.0,
                    corner_radius: 0.0,
                    clip_to_geometry: true,
                    wants_blur: false,
                    no_shadow: false,
                    border_color_override: None,
                    gradient_time_ms: 0,
                    border: appearance.border.clone(),
                    focus_ring: appearance.focus_ring.clone(),
                    shadow: appearance.shadow.clone(),
                    dim: appearance.effective_dim(),
                }]
            })
            .unwrap_or_default()
    } else {
        state.collect_workspace_scene_items(&params.ws_id, true)
    };
    let has_protected_blackout = scene_items.iter().any(|it| it.is_protected_surface);

    let gpu_readout = backend.as_mut().and_then(|active_backend| {
        let offscreen = offscreen_surfaces
            .entry(params.ws_id.clone())
            .or_insert_with(|| {
                LazyOffscreenSurface::new(
                    viewport.width as i32,
                    viewport.height as i32,
                    viewport.scale,
                )
            });

        active_backend.with_primary_renderer(|renderer| {
            let mut elements = assemble_workspace_elements(
                renderer,
                viewport,
                Scale::from(viewport.scale),
                &lease_state,
                RenderTargetKind::AgentCapture,
                params.overlay_kind,
                None,
                None,
                &scene_items,
                None,
            );
            prepend_agent_cursor_elements(
                renderer,
                RenderTargetKind::AgentCapture,
                Scale::from(viewport.scale),
                None,
                None,
                &mut elements,
            );
            offscreen.capture_rgba8_pixels(renderer, &elements)
        })
    });
    drop(scene_items);
    state.backend = backend;
    state.offscreen_surfaces = offscreen_surfaces;

    let (width, height, raw_rgba) = if let Some(res) = gpu_readout {
        res.map_err(|e| {
            IpcError::new(
                ErrorSymbol::ErrInvalidParams,
                format!("GPU readout failed: {e}"),
            )
        })?
    } else {
        // Software fallback buffer when running without an active GLES backend
        let w = viewport.width;
        let h = viewport.height;
        (w, h, vec![0_u8; (w as usize) * (h as usize) * 4])
    };

    match params.transport {
        TransportKind::ShmFd => {
            let memfd =
                create_sealed_readonly_memfd(params.ws_id.as_str(), &raw_rgba).map_err(|e| {
                    IpcError::new(ErrorSymbol::ErrInvalidParams, format!("memfd error: {e}"))
                })?;
            let res = CaptureFrameResult {
                epoch,
                width,
                height,
                byte_len: raw_rgba.len(),
                fd_index: Some(AncillaryFdRef(0)),
                has_protected_blackout,
                grid_cells: (params.overlay_kind == OverlayKind::Grid225).then_some(225),
            };
            let json_val = serde_json::to_value(res)
                .map_err(|e| IpcError::new(ErrorSymbol::ErrInvalidParams, e.to_string()))?;
            Ok((json_val, vec![memfd]))
        }
        TransportKind::InlineBase64 => {
            let encoded = URL_SAFE_NO_PAD.encode(&raw_rgba);
            Ok((
                json!({
                    "epoch": epoch,
                    "width": width,
                    "height": height,
                    "byte_len": raw_rgba.len(),
                    "data_base64url": encoded,
                    "has_protected_blackout": has_protected_blackout,
                }),
                Vec::new(),
            ))
        }
    }
}

fn resolve_caller_identity(
    auth: &PeerAuthState,
) -> Result<(SessionId, Option<CapabilityToken>), IpcError> {
    match auth {
        PeerAuthState::AdminVerified { session_id, .. } => Ok((session_id.clone(), None)),
        PeerAuthState::AgentAuthenticated { session_id, token } => {
            Ok((session_id.clone(), Some(token.clone())))
        }
        PeerAuthState::AgentUnauthenticated => Err(IpcError::new(
            ErrorSymbol::ErrUnauthenticated,
            "unauthenticated agent connection",
        )),
    }
}

fn authorize_peer_for_ws(
    state: &KomaState,
    auth: &PeerAuthState,
    ws_id: &WorkspaceId,
    required: PrivilegeLevel,
    now_ms: u64,
) -> Result<(SessionId, PrivilegeLevel), IpcError> {
    match auth {
        PeerAuthState::AdminVerified { session_id, .. } => {
            Ok((session_id.clone(), PrivilegeLevel::Owner))
        }
        PeerAuthState::AgentAuthenticated { session_id, token } => {
            let claims = state.token_store.get(token).ok_or_else(|| {
                IpcError::new(ErrorSymbol::ErrUnauthenticated, "revoked capability token")
            })?;
            let is_headed_human_preempted = state.workspaces.get(ws_id).is_some_and(|ws| {
                matches!(ws.presentation, PresentationTarget::Physical(_))
                    && ws.lease.is_human_preempted()
            });
            claims.authorize_workspace(ws_id, required, now_ms, is_headed_human_preempted)?;
            let granted = claims
                .workspace_grants
                .get(ws_id)
                .copied()
                .unwrap_or(PrivilegeLevel::ObserveOnly);
            Ok((session_id.clone(), granted))
        }
        PeerAuthState::AgentUnauthenticated => Err(IpcError::new(
            ErrorSymbol::ErrUnauthenticated,
            "unauthenticated agent connection",
        )),
    }
}

fn parse_ws_id(params: &Value) -> Result<WorkspaceId, IpcError> {
    let s = params
        .get("ws_id")
        .and_then(Value::as_str)
        .ok_or_else(|| IpcError::new(ErrorSymbol::ErrInvalidParams, "missing ws_id"))?;
    Ok(WorkspaceId::new(s.to_owned()))
}

fn parse_expected_epoch(params: &Value) -> Result<Epoch, IpcError> {
    let n = params
        .get("expected_epoch")
        .and_then(Value::as_u64)
        .ok_or_else(|| IpcError::new(ErrorSymbol::ErrInvalidParams, "missing expected_epoch"))?;
    Ok(Epoch(n))
}

fn parse_ipc_anchor(val: &Value) -> Result<koma_model::AnchorPoint, IpcError> {
    if val.is_null() {
        return Ok(koma_model::AnchorPoint::AnchorOffset {
            anchor: koma_model::Anchor9::TopLeft,
            dx: 0.0,
            dy: 0.0,
        });
    }
    if let Some(arr) = val.as_array() {
        if arr.len() != 2 {
            return Err(IpcError::new(
                ErrorSymbol::ErrInvalidParams,
                format!(
                    "array anchor waypoint must have [x, y] length 2, got {}",
                    arr.len()
                ),
            ));
        }
        let x = arr[0].as_f64().filter(|v| v.is_finite()).ok_or_else(|| {
            IpcError::new(
                ErrorSymbol::ErrInvalidParams,
                "array anchor x must be finite",
            )
        })?;
        let y = arr[1].as_f64().filter(|v| v.is_finite()).ok_or_else(|| {
            IpcError::new(
                ErrorSymbol::ErrInvalidParams,
                "array anchor y must be finite",
            )
        })?;
        return Ok(koma_model::AnchorPoint::AnchorOffset {
            anchor: koma_model::Anchor9::TopLeft,
            dx: x,
            dy: y,
        });
    }
    if !val.is_object() {
        return Err(IpcError::new(
            ErrorSymbol::ErrInvalidParams,
            "invalid anchor: expected object or [x, y] array",
        ));
    }
    let mut input: koma_lua::schema::LuaAnchorPointInput = serde_json::from_value(val.clone())
        .map_err(|e| {
            IpcError::new(
                ErrorSymbol::ErrInvalidParams,
                format!("invalid anchor: {e}"),
            )
        })?;
    if input.x.is_none()
        && let Some(dx_val) = val.get("dx").filter(|v| !v.is_null())
    {
        let dx = dx_val.as_f64().filter(|v| v.is_finite()).ok_or_else(|| {
            IpcError::new(ErrorSymbol::ErrInvalidParams, "anchor dx must be finite")
        })?;
        input.x = Some(dx);
    }
    if input.y.is_none()
        && let Some(dy_val) = val.get("dy").filter(|v| !v.is_null())
    {
        let dy = dy_val.as_f64().filter(|v| v.is_finite()).ok_or_else(|| {
            IpcError::new(ErrorSymbol::ErrInvalidParams, "anchor dy must be finite")
        })?;
        input.y = Some(dy);
    }
    input
        .into_anchor_point()
        .map_err(|e| IpcError::new(ErrorSymbol::ErrInvalidParams, e))
}

fn parse_ipc_kinematics(params: &Value) -> koma_lua::schema::LuaKinematicsInput {
    let mut kin = params
        .get("kinematics")
        .and_then(|v| {
            serde_json::from_value::<koma_lua::schema::LuaKinematicsInput>(v.clone()).ok()
        })
        .unwrap_or_default();
    if kin.duration_ms.is_none()
        && let Some(dur) = params.get("duration_ms").and_then(Value::as_u64)
    {
        kin.duration_ms = Some(dur);
    }
    kin
}

fn validate_vseat_mutation_gate(
    state: &mut KomaState,
    auth: &PeerAuthState,
    ws_id: &WorkspaceId,
    expected_epoch: Epoch,
    auto_ttl_ms: Option<u64>,
    now_ms: u64,
) -> Result<Epoch, IpcError> {
    let (session_id, grant) =
        authorize_peer_for_ws(state, auth, ws_id, PrivilegeLevel::Control, now_ms)?;
    let ws = state
        .workspaces
        .get_mut(ws_id)
        .ok_or_else(|| IpcError::new(ErrorSymbol::ErrNotFound, "unknown workspace"))?;
    ws.lease
        .validate_mutation_gate(&session_id, grant, expected_epoch, auto_ttl_ms, now_ms)
}

fn parse_ipc_modifiers(params: &Value) -> Result<Vec<String>, IpcError> {
    match params.get("modifiers") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(v) => serde_json::from_value::<Vec<String>>(v.clone()).map_err(|e| {
            IpcError::new(
                ErrorSymbol::ErrInvalidParams,
                format!("invalid modifiers array: {e}"),
            )
        }),
    }
}

fn parse_ipc_label(params: &Value) -> Result<Option<String>, IpcError> {
    match params.get("label") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(IpcError::new(
            ErrorSymbol::ErrInvalidParams,
            "label must be a string or null",
        )),
    }
}

fn parse_ipc_modulation(params: &Value) -> Result<koma_model::HandPathModulation, IpcError> {
    let parse_opt_f64 = |key: &str| -> Result<f64, IpcError> {
        match params.get(key) {
            None | Some(Value::Null) => Ok(0.0),
            Some(v) => v.as_f64().filter(|x| x.is_finite()).ok_or_else(|| {
                IpcError::new(
                    ErrorSymbol::ErrInvalidParams,
                    format!("{key} must be a finite number"),
                )
            }),
        }
    };
    Ok(koma_model::HandPathModulation {
        bow: parse_opt_f64("bow")?,
        overshoot: parse_opt_f64("overshoot")?,
        tremor: parse_opt_f64("tremor")?,
    }
    .sanitized())
}

fn vseat_response_disposition(state: &KomaState, ws_id: &WorkspaceId) -> DeliverySemantics {
    if state.pointer_animations.iter().any(|a| &a.ws_id == ws_id) {
        DeliverySemantics::delivered_unverified_unsafe()
    } else {
        DeliverySemantics::delivered_verified()
    }
}

pub(crate) fn cleanup_disconnected_ipc_peer(state: &mut KomaState, conn_id: u64) {
    let Some(removed) = state.ipc_clients.remove(&conn_id) else {
        return;
    };
    let session_id = match &removed.auth {
        PeerAuthState::AdminVerified { session_id, .. }
        | PeerAuthState::AgentAuthenticated { session_id, .. } => session_id.clone(),
        PeerAuthState::AgentUnauthenticated => return,
    };
    let now_ms = state.elapsed_ms();
    let owned_workspaces: Vec<(WorkspaceId, Epoch)> = state
        .workspaces
        .iter()
        .filter_map(|(ws_id, ws)| match ws.lease.state() {
            WorkspaceLeaseState::AgentExclusive { holder, .. } if holder == &session_id => {
                Some((ws_id.clone(), ws.lease.epoch()))
            }
            _ => None,
        })
        .collect();
    for (ws_id, expected_epoch) in owned_workspaces {
        let _ = crate::macro_runtime::release_all_vseat_held_inputs(state, &ws_id);
        state.pointer_animations.retain(|a| a.ws_id != ws_id);
        if let Some(ac) = state.agent_cursors.get_mut(&ws_id) {
            ac.visual.begin_settle(now_ms, false);
        }
        if let Some(ws) = state.workspaces.get_mut(&ws_id)
            && let Ok(new_epoch) = ws.lease.release(&session_id, expected_epoch)
        {
            state.broadcast_workspace_event(
                EventTopic::WorkspaceLease,
                &ws_id,
                new_epoch,
                WorkspaceEvent::EpochChanged {
                    new_epoch,
                    holder: None,
                },
            );
        }
        state.queue_redraw();
    }
}

fn generate_ephemeral_capability_token() -> Result<CapabilityToken, IpcError> {
    let mut raw = [0_u8; 32];
    let mut f = File::open("/dev/urandom")
        .map_err(|e| IpcError::new(ErrorSymbol::ErrInvalidParams, format!("urandom error: {e}")))?;
    f.read_exact(&mut raw)
        .map_err(|e| IpcError::new(ErrorSymbol::ErrInvalidParams, format!("urandom read: {e}")))?;
    Ok(CapabilityToken(format!(
        "{}{}",
        CapabilityToken::PREFIX,
        URL_SAFE_NO_PAD.encode(raw)
    )))
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixStream;

    use super::*;

    /// Registers an admin-socket connection so RPC tests exercise the real authorization path.
    fn insert_admin_conn(state: &mut KomaState, conn_id: u64) {
        let (client_stream, _peer) = UnixStream::pair().expect("unix stream pair");
        state.ipc_clients.insert(
            conn_id,
            IpcClientConn {
                conn_id,
                stream: client_stream,
                channel: SocketChannel::Admin,
                auth: PeerAuthState::AdminVerified {
                    uid: 1000,
                    gid: 1000,
                    session_id: SessionId("test-admin".to_owned()),
                },
                reg_token: None,
                inbox: FrameInbox::default(),
                outbox: FrameOutbox::default(),
                event_ring: EventRingBuffer::default(),
                subscribed_workspaces: BTreeSet::new(),
                subscribed_topics: HashSet::new(),
                armed_writable: false,
            },
        );
    }

    /// Inserts a tiled window as the active window of `ws_id` without a Wayland surface.
    fn insert_window(state: &mut KomaState, ws_id: &WorkspaceId) -> koma_proto::WindowId {
        let win_id = koma_proto::WindowId::new(1, 1);
        let now_ms = state.elapsed_ms();
        state
            .workspaces
            .get_mut(ws_id)
            .expect("workspace exists")
            .insert_window(win_id, false, None, None, None, None, false, true, now_ms);
        win_id
    }

    #[test]
    fn ipc_lock_gate_denies_capture_and_sensitive_methods_when_locked_or_abandoned() {
        assert!(check_ipc_lock_gate(false, "capture.frame").is_ok());
        assert!(check_ipc_lock_gate(false, "scene.inventory").is_ok());
        assert!(check_ipc_lock_gate(true, "config.status").is_ok());

        for blocked_method in [
            "capture.frame",
            "scene.inventory",
            "win.activate",
            "win.close",
            "ws.create",
            "lease.acquire",
            "macro.run",
            "events.subscribe",
            "vseat.key",
            "vseat.path",
            "vseat.stroke",
            "vseat.cursor_settle",
            "lua.exec_tx",
            "config.reload",
        ] {
            let err = check_ipc_lock_gate(true, blocked_method)
                .expect_err("sensitive IPC method must fail closed when locked");
            assert_eq!(err.symbol, ErrorSymbol::ErrCapabilityDenied);
            assert_eq!(err.message, "session is locked");
        }
    }

    #[test]
    fn ws_focus_switches_the_active_workspace_and_rejects_unknown_names() {
        let (_event_loop, _display, mut state) = crate::state::tests::make_headless_test_state();
        let start = state.active_ws_id.clone();
        let target = state.ensure_workspace_by_name_or_id("2");
        assert_ne!(start, target, "test needs a second workspace");

        let conn_id = 77_u64;
        insert_admin_conn(&mut state, conn_id);

        let (res, _) = dispatch_rpc(
            &mut state,
            conn_id,
            IpcRequest {
                id: 1,
                method: "ws.focus".to_owned(),
                params: json!({ "ws_id": "2" }),
            },
            Vec::new(),
        )
        .expect("ws.focus must switch to an existing workspace");
        assert_eq!(state.active_ws_id, target);
        assert_eq!(res["focused"], json!("2"));
        assert_eq!(res["active_ws_id"], json!(target.as_str()));

        // A status-bar click must never create a workspace as a side effect.
        let before = state.workspaces.len();
        let err = dispatch_rpc(
            &mut state,
            conn_id,
            IpcRequest {
                id: 2,
                method: "ws.focus".to_owned(),
                params: json!({ "ws_id": "no-such-workspace" }),
            },
            Vec::new(),
        )
        .expect_err("unknown workspace must be rejected");
        assert_eq!(err.symbol, ErrorSymbol::ErrNotFound);
        assert_eq!(state.workspaces.len(), before);
        assert_eq!(state.active_ws_id, target);
    }

    #[test]
    fn lock_gate_refuses_workspace_switch_rpcs_without_mutating_state() {
        let (_event_loop, _display, mut state) = crate::state::tests::make_headless_test_state();
        let start = state.active_ws_id.clone();
        let target = state.ensure_workspace_by_name_or_id("2");
        let win_id = insert_window(&mut state, &start);
        let conn_id = 71_u64;
        insert_admin_conn(&mut state, conn_id);
        assert!(
            state.session_lock.try_lock(None),
            "test must lock the session"
        );

        let focus_err = dispatch_rpc(
            &mut state,
            conn_id,
            IpcRequest {
                id: 1,
                method: "ws.focus".to_owned(),
                params: json!({ "ws_id": "2" }),
            },
            Vec::new(),
        )
        .expect_err("ws.focus must fail closed while the session is locked");
        assert_eq!(focus_err.symbol, ErrorSymbol::ErrCapabilityDenied);
        assert_eq!(focus_err.message, "session is locked");

        let move_err = dispatch_rpc(
            &mut state,
            conn_id,
            IpcRequest {
                id: 2,
                method: "win.move_to_workspace".to_owned(),
                params: json!({ "workspace": "2", "follow": true }),
            },
            Vec::new(),
        )
        .expect_err("win.move_to_workspace must fail closed while the session is locked");
        assert_eq!(move_err.symbol, ErrorSymbol::ErrCapabilityDenied);
        assert_eq!(move_err.message, "session is locked");

        assert_eq!(state.active_ws_id, start, "locked RPCs must not switch");
        assert_eq!(
            state.workspaces[&start].active_window(),
            Some(win_id),
            "locked RPCs must not move the window"
        );
        assert_eq!(state.workspaces[&target].active_window(), None);
    }

    #[test]
    fn win_move_to_workspace_moves_the_active_window_following_or_silently() {
        let (_event_loop, _display, mut state) = crate::state::tests::make_headless_test_state();
        let src = state.active_ws_id.clone();
        let follow_target = state.ensure_workspace_by_name_or_id("2");
        let silent_target = state.ensure_workspace_by_name_or_id("3");
        let win_id = insert_window(&mut state, &src);
        let conn_id = 72_u64;
        insert_admin_conn(&mut state, conn_id);

        let (followed, _) = dispatch_rpc(
            &mut state,
            conn_id,
            IpcRequest {
                id: 1,
                method: "win.move_to_workspace".to_owned(),
                params: json!({ "workspace": "2", "follow": true }),
            },
            Vec::new(),
        )
        .expect("followed move must succeed");
        assert_eq!(followed["win_id"], json!(win_id.to_string()));
        assert_eq!(followed["from_ws_id"], json!(src.as_str()));
        assert_eq!(followed["to_ws_id"], json!(follow_target.as_str()));
        assert_eq!(followed["moved"], json!(true));
        assert_eq!(followed["followed"], json!(true));
        assert_eq!(state.active_ws_id, follow_target);
        assert_eq!(
            state.workspaces[&follow_target].active_window(),
            Some(win_id)
        );

        // `win.move` is the documented alias and never follows unless asked to.
        let (silent, _) = dispatch_rpc(
            &mut state,
            conn_id,
            IpcRequest {
                id: 2,
                method: "win.move".to_owned(),
                params: json!({ "workspace": "3" }),
            },
            Vec::new(),
        )
        .expect("silent move must succeed");
        assert_eq!(silent["to_ws_id"], json!(silent_target.as_str()));
        assert_eq!(silent["followed"], json!(false));
        assert_eq!(
            state.active_ws_id, follow_target,
            "a silent move keeps the source workspace focused"
        );
        assert_eq!(
            state.workspaces[&silent_target].active_window(),
            Some(win_id)
        );
        assert_eq!(state.workspaces[&follow_target].active_window(), None);

        // Existing destinations only, and an empty source is an explicit `ErrNotFound`.
        let before = state.workspaces.len();
        let unknown = dispatch_rpc(
            &mut state,
            conn_id,
            IpcRequest {
                id: 3,
                method: "win.move_to_workspace".to_owned(),
                params: json!({ "workspace": "no-such-workspace" }),
            },
            Vec::new(),
        )
        .expect_err("unknown destination workspace must be rejected");
        assert_eq!(unknown.symbol, ErrorSymbol::ErrNotFound);
        assert_eq!(
            state.workspaces.len(),
            before,
            "no workspace may be created"
        );

        let empty_source = dispatch_rpc(
            &mut state,
            conn_id,
            IpcRequest {
                id: 4,
                method: "win.move_to_workspace".to_owned(),
                params: json!({ "workspace": "1" }),
            },
            Vec::new(),
        )
        .expect_err("a workspace without an active window has nothing to move");
        assert_eq!(empty_source.symbol, ErrorSymbol::ErrNotFound);

        // The agent socket cannot borrow a bar's admin privilege.
        let agent_conn = 73_u64;
        let (agent_stream, _agent_peer) = UnixStream::pair().expect("unix stream pair");
        state.ipc_clients.insert(
            agent_conn,
            IpcClientConn {
                conn_id: agent_conn,
                stream: agent_stream,
                channel: SocketChannel::Agent,
                auth: PeerAuthState::AgentAuthenticated {
                    session_id: SessionId("agent".to_owned()),
                    token: CapabilityToken(
                        "koma_cap_test_token_for_admin_only_assertions".to_owned(),
                    ),
                },
                reg_token: None,
                inbox: FrameInbox::default(),
                outbox: FrameOutbox::default(),
                event_ring: EventRingBuffer::default(),
                subscribed_workspaces: BTreeSet::new(),
                subscribed_topics: HashSet::new(),
                armed_writable: false,
            },
        );
        let denied = dispatch_rpc(
            &mut state,
            agent_conn,
            IpcRequest {
                id: 5,
                method: "win.move_to_workspace".to_owned(),
                params: json!({ "workspace": "1" }),
            },
            Vec::new(),
        )
        .expect_err("win.move_to_workspace is admin-only");
        assert_eq!(denied.symbol, ErrorSymbol::ErrCapabilityDenied);
    }

    #[test]
    fn workspace_lookup_prefers_exact_ids_over_names() {
        let (_event_loop, _display, mut state) = crate::state::tests::make_headless_test_state();
        let ws1 = state.active_ws_id.clone();
        assert_eq!(ws1.as_str(), "ws_01");

        // A workspace deliberately *named* like the real id of another workspace.
        let decoy_id = WorkspaceId::new("ws_09");
        let now_ms = state.elapsed_ms();
        state.workspaces.insert(
            decoy_id.clone(),
            WorkspaceState::new(
                decoy_id.clone(),
                ws1.as_str(),
                SeatId::new("vseat_09"),
                LogicalViewport::new(1920, 1080, 1.0),
                PresentationTarget::Physical(koma_proto::OutputId::new("primary")),
                LayoutMode::Dwindle,
                now_ms,
            ),
        );
        let src = state.ensure_workspace_by_name_or_id("2");
        let win_id = insert_window(&mut state, &src);
        let conn_id = 75_u64;
        insert_admin_conn(&mut state, conn_id);

        assert_eq!(
            state.find_workspace_by_id_or_name("ws_01"),
            Some(ws1.clone()),
            "an exact workspace id must win over a workspace merely named `ws_01`"
        );

        let (_, _) = dispatch_rpc(
            &mut state,
            conn_id,
            IpcRequest {
                id: 1,
                method: "ws.focus".to_owned(),
                params: json!({ "ws_id": "ws_01" }),
            },
            Vec::new(),
        )
        .expect("ws.focus must resolve the exact id");
        assert_eq!(state.active_ws_id, ws1, "ws.focus landed on the decoy name");

        // Return to the window's workspace so the move has a source.
        let (_, _) = dispatch_rpc(
            &mut state,
            conn_id,
            IpcRequest {
                id: 2,
                method: "ws.focus".to_owned(),
                params: json!({ "ws_id": "2" }),
            },
            Vec::new(),
        )
        .expect("ws.focus must switch back by name");
        assert_eq!(state.active_ws_id, src);

        let (moved, _) = dispatch_rpc(
            &mut state,
            conn_id,
            IpcRequest {
                id: 3,
                method: "win.move_to_workspace".to_owned(),
                params: json!({ "workspace": "ws_01" }),
            },
            Vec::new(),
        )
        .expect("win.move_to_workspace must resolve the exact id");
        assert_eq!(moved["to_ws_id"], json!(ws1.as_str()));
        assert_eq!(state.workspaces[&ws1].active_window(), Some(win_id));
        assert_ne!(state.workspaces[&decoy_id].active_window(), Some(win_id));
    }

    #[test]
    fn scene_inventory_marks_exactly_one_workspace_focused() {
        let (_event_loop, _display, mut state) = crate::state::tests::make_headless_test_state();
        let ws1 = state.active_ws_id.clone();
        let ws2 = state.ensure_workspace_by_name_or_id("2");
        state.ensure_workspace_by_name_or_id("scratch");
        let conn_id = 74_u64;
        insert_admin_conn(&mut state, conn_id);

        let focused_ids = |state: &mut KomaState| -> Vec<String> {
            let (inventory, _) = dispatch_rpc(
                state,
                conn_id,
                IpcRequest {
                    id: 9,
                    method: "scene.inventory".to_owned(),
                    params: json!({}),
                },
                Vec::new(),
            )
            .expect("scene.inventory must succeed on koma-admin.sock");
            inventory["workspaces"]
                .as_array()
                .expect("workspaces array")
                .iter()
                .filter(|ws| ws["focused"] == json!(true))
                .map(|ws| ws["ws_id"].as_str().unwrap_or_default().to_owned())
                .collect()
        };

        assert_eq!(focused_ids(&mut state), vec![ws1.as_str().to_owned()]);

        let (_, _) = dispatch_rpc(
            &mut state,
            conn_id,
            IpcRequest {
                id: 10,
                method: "ws.focus".to_owned(),
                params: json!({ "ws_id": ws2.as_str() }),
            },
            Vec::new(),
        )
        .expect("ws.focus must switch");
        assert_eq!(focused_ids(&mut state), vec![ws2.as_str().to_owned()]);
    }

    #[test]
    fn vseat_path_stroke_and_cursor_settle_decouple_human_pointer_and_preserve_capture_isolation() {
        use smithay::utils::Point;
        use std::os::unix::net::UnixStream;

        let (_event_loop, _display, mut state) = crate::state::tests::make_headless_test_state();
        let ws_id = state.active_ws_id.clone();
        let expected_epoch = state
            .workspaces
            .get(&ws_id)
            .expect("active ws")
            .lease
            .epoch()
            .get();

        // Human physical pointer must never be clobbered by vseat.* calls.
        let human_pos = Point::from((777.0, 555.0));
        state.pointer_location = human_pos;

        let (client_stream, _peer) = UnixStream::pair().expect("unix stream pair");
        let conn_id = 42_u64;
        state.ipc_clients.insert(
            conn_id,
            IpcClientConn {
                conn_id,
                stream: client_stream,
                channel: SocketChannel::Admin,
                auth: PeerAuthState::AdminVerified {
                    uid: 1000,
                    gid: 1000,
                    session_id: SessionId("test-admin".to_owned()),
                },
                reg_token: None,
                inbox: FrameInbox::default(),
                outbox: FrameOutbox::default(),
                event_ring: EventRingBuffer::default(),
                subscribed_workspaces: BTreeSet::new(),
                subscribed_topics: HashSet::new(),
                armed_writable: false,
            },
        );

        // 1. Validate error when fewer than 2 waypoints provided
        let bad_req = IpcRequest {
            id: 1,
            method: "vseat.path".to_owned(),
            params: json!({
                "ws_id": ws_id.as_str(),
                "expected_epoch": expected_epoch,
                "waypoints": [[100, 100]]
            }),
        };
        let err = dispatch_rpc(&mut state, conn_id, bad_req, Vec::new())
            .expect_err("1 waypoint must fail validation");
        assert_eq!(err.symbol, ErrorSymbol::ErrInvalidParams);

        // 2. Execute multi-waypoint vseat.path mixing [x,y], {anchor,dx,dy}, and {grid_225}
        let path_req = IpcRequest {
            id: 2,
            method: "vseat.path".to_owned(),
            params: json!({
                "ws_id": ws_id.as_str(),
                "expected_epoch": expected_epoch,
                "spline": "catmull_rom",
                "duration_ms": 180,
                "waypoints": [
                    [100.0, 120.0],
                    { "anchor": "center", "dx": 15.0, "dy": -10.0 },
                    { "grid_225": "M11" }
                ]
            }),
        };
        let (path_res, _) =
            dispatch_rpc(&mut state, conn_id, path_req, Vec::new()).expect("vseat.path succeeds");
        assert_eq!(path_res["ok"], json!(true));
        assert_eq!(path_res["waypoints"], json!(3));
        assert_eq!(state.pointer_location, human_pos);
        assert_eq!(state.pointer_animations.len(), 1);
        assert!(state.step_agent_cursor_for_workspace(&ws_id).is_some());
        let active_epoch = path_res["epoch"].as_u64().expect("active epoch");

        // 3. Execute instant vseat.stroke and confirm button press/release left clean ledger & human pointer intact
        let stroke_req = IpcRequest {
            id: 3,
            method: "vseat.stroke".to_owned(),
            params: json!({
                "ws_id": ws_id.as_str(),
                "expected_epoch": active_epoch,
                "spline": "bezier",
                "kinematics": { "profile": "instant" },
                "waypoints": [
                    { "x": 200.0, "y": 240.0 },
                    { "x": 360.0, "y": 320.0 }
                ]
            }),
        };
        let (stroke_res, _) = dispatch_rpc(&mut state, conn_id, stroke_req, Vec::new())
            .expect("vseat.stroke succeeds");
        assert_eq!(stroke_res["ok"], json!(true));
        assert_eq!(stroke_res["waypoints"], json!(2));
        assert_eq!(state.pointer_location, human_pos);
        let ac = state
            .agent_cursors
            .get(&ws_id)
            .expect("agent cursor tracked");
        assert!((ac.ws_local_pos.x - 360.0).abs() < 1e-3);
        assert!((ac.ws_local_pos.y - 320.0).abs() < 1e-3);

        // 4. capture.frame excludes agent cursor rendering (`AgentCapture` isolation)
        assert!(!koma_render::frame_composer::should_render_agent_cursor(
            RenderTargetKind::AgentCapture
        ));
        assert!(koma_render::frame_composer::should_render_agent_cursor(
            RenderTargetKind::PhysicalOutput
        ));
        assert!(koma_render::frame_composer::should_render_agent_cursor(
            RenderTargetKind::ScreencastPreview
        ));
        let capture_req = IpcRequest {
            id: 4,
            method: "capture.frame".to_owned(),
            params: json!({
                "ws_id": ws_id.as_str(),
                "overlay_kind": "none",
                "max_edge": 1280,
                "format": "raw_rgba8",
                "transport": "inline_base64"
            }),
        };
        let (cap_res, _) = dispatch_rpc(&mut state, conn_id, capture_req, Vec::new())
            .expect("capture.frame succeeds");
        assert!(cap_res["width"].as_u64().is_some());
        assert_eq!(state.pointer_location, human_pos);

        // 5. vseat.cursor_settle immediate hides visual cursor cleanly
        let settle_req = IpcRequest {
            id: 5,
            method: "vseat.cursor_settle".to_owned(),
            params: json!({
                "ws_id": ws_id.as_str(),
                "immediate": true
            }),
        };
        let (settle_res, _) = dispatch_rpc(&mut state, conn_id, settle_req, Vec::new())
            .expect("vseat.cursor_settle succeeds");
        assert_eq!(settle_res["ok"], json!(true));
        assert!(state.step_agent_cursor_for_workspace(&ws_id).is_none());
    }

    #[test]
    fn config_status_reports_the_effective_pointer_accel_instead_of_hiding_it() {
        use koma_lua::schema::{AccelProfileSpec, PointerInputSpec};

        // Unset profile = libinput keeps its adaptive curve; unset speed = no modification.
        let mut input = koma_lua::schema::InputConfigSpec::default();
        let defaults = config_status_input_json(&input);
        assert_eq!(
            defaults["pointer"]["accel_profile"],
            serde_json::Value::Null
        );
        assert_eq!(defaults["pointer"]["accel_speed"], serde_json::Value::Null);

        // The migration bug from komawm/koma#34: `sensitivity = 0` mapped to `flat`, which removes
        // acceleration entirely. The report has to make that visible.
        input.pointer = PointerInputSpec {
            accel_profile: Some(AccelProfileSpec::Flat),
            accel_speed: Some(0.0),
            ..PointerInputSpec::default()
        };
        let flat = config_status_input_json(&input);
        assert_eq!(flat["pointer"]["accel_profile"], "flat");
        assert_eq!(flat["pointer"]["accel_speed"], 0.0);

        input.pointer.accel_profile = Some(AccelProfileSpec::Adaptive);
        let adaptive = config_status_input_json(&input);
        assert_eq!(adaptive["pointer"]["accel_profile"], "adaptive");
    }

    #[test]
    fn config_status_reports_the_xkb_keyboard_options_that_remap_caps() {
        use koma_lua::schema::InputConfigSpec;

        let mut input = InputConfigSpec::default();
        assert_eq!(
            config_status_input_json(&input)["keyboard"]["options"],
            serde_json::Value::Null,
            "an unset keymap option list must stay null, not an empty string"
        );

        // The Hyprland-compatible Caps-to-Escape remap, plus the swap that pairs with it.
        input.keyboard.layout = Some("us".to_owned());
        input.keyboard.options = Some("caps:escape,altwin:swap_alt_win".to_owned());
        let reported = config_status_input_json(&input);
        assert_eq!(reported["keyboard"]["layout"], "us");
        assert_eq!(
            reported["keyboard"]["options"],
            "caps:escape,altwin:swap_alt_win"
        );
    }

    #[test]
    fn issue_106_scene_inventory_orders_workspaces_canonically() {
        let (_el, _disp, mut state) = crate::state::tests::make_headless_test_state();
        let vp = LogicalViewport::new(1600, 1000, 1.0);
        for (ws_id, name) in [
            ("ws_06", "6"),
            ("ws_agent_b", "agent-b"),
            ("ws_02", "2"),
            ("ws_agent_a", "agent-a"),
            ("ws_03", "3"),
        ] {
            state.workspaces.insert(
                WorkspaceId::new(ws_id),
                WorkspaceState::new(
                    WorkspaceId::new(ws_id),
                    name,
                    SeatId::new(format!("vseat_{ws_id}")),
                    vp,
                    PresentationTarget::Offscreen { lazy_fbo_slot: 0 },
                    LayoutMode::Deck,
                    0,
                ),
            );
        }

        insert_admin_conn(&mut state, 1);
        let req = IpcRequest {
            id: 42,
            method: "scene.inventory".to_owned(),
            params: json!({}),
        };
        let (res, _) =
            dispatch_rpc(&mut state, 1, req, Vec::new()).expect("scene.inventory succeeds");
        let names: Vec<&str> = res["workspaces"]
            .as_array()
            .expect("workspaces array")
            .iter()
            .filter_map(|item| item["name"].as_str())
            .collect();
        assert_eq!(names, vec!["1", "2", "3", "6", "agent-a", "agent-b"]);
    }
}
