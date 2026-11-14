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
    LayoutMode, LogicalViewport, PresentationTarget, WindowMatchContext, WorkspaceState,
};
use koma_proto::{
    ADMIN_SOCKET_NAME, AGENT_SOCKET_NAME, AncillaryFdRef, CapabilityToken, Epoch, ErrorSymbol,
    EventRingBuffer, EventTopic, IpcError, IpcRequest, IpcResponse, OverlayKind, PrivilegeLevel,
    SeatId, SessionId, TokenAuthorizer as _, TokenClaims, TransportKind, WorkspaceEvent,
    WorkspaceId,
    rpc::{
        CaptureFrameParams, CaptureFrameResult, LuaExecTxParams, WorkspaceModeSpec,
        WorkspaceSetModeParams,
    },
    scm_unix::{
        FrameInbox, FrameOutbox, InboxPoll, create_sealed_readonly_memfd, verify_same_uid_peer,
    },
};
use koma_render::{
    RenderTargetKind,
    frame_composer::{WindowSceneItem, assemble_workspace_elements},
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
    // Resolved IPC runtime paths retained on startup handle for admin/diagnostics.
    #[allow(dead_code)]
    pub runtime_dir: PathBuf,
    #[allow(dead_code)]
    pub admin_socket: PathBuf,
    #[allow(dead_code)]
    pub agent_socket: PathBuf,
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

    // Export signature for child processes launched inside Koma (`IPC_SPEC.md` §2.1)
    // SAFETY: Invoked on the single-threaded compositor main thread during startup before spawning workers.
    unsafe {
        env::set_var("KOMA_INSTANCE_SIGNATURE", &instance_signature);
    }

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
                    state.ipc_clients.remove(&conn_id);
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

/// Authorizes and executes a parsed `IpcRequest` against `KomaState`.
fn dispatch_rpc(
    state: &mut KomaState,
    conn_id: u64,
    req: IpcRequest,
    _inbound_fds: Vec<OwnedFd>,
) -> Result<(Value, Vec<OwnedFd>), IpcError> {
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
            let ws = state
                .workspaces
                .get_mut(&ws_id)
                .ok_or_else(|| IpcError::new(ErrorSymbol::ErrNotFound, "unknown workspace"))?;
            let new_epoch = ws.lease.release(&session_id, expected_epoch)?;
            if let Some(vseat) = state.vseats.get_mut(&ws.vseat_id) {
                let _ = vseat.flush_all_held();
            }
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

        "scene.inventory" => {
            let (caller_sess, agent_tok) = resolve_caller_identity(&auth_snapshot)?;
            let mut list = Vec::new();
            for (ws_id, ws) in &state.workspaces {
                if let Some(tok) = &agent_tok {
                    let allowed = state.token_store.get(tok).is_some_and(|c| {
                        c.authorize_workspace(ws_id, PrivilegeLevel::ObserveOnly, now_ms, false)
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
            Ok((json!({ "workspaces": list }), Vec::new()))
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

        "vseat.click" => {
            let ws_id = parse_ws_id(&req.params)?;
            let expected_epoch = parse_expected_epoch(&req.params)?;
            let auto_ttl = req
                .params
                .get("auto_acquire")
                .and_then(Value::as_bool)
                .unwrap_or(true)
                .then_some(60_000);
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
                    auto_ttl,
                    now_ms,
                )?
            };

            let target = if let Some(cell) = req.params.get("grid_225").and_then(Value::as_str) {
                koma_model::AnchorPoint::Grid225(cell.to_owned())
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

            crate::macro_runtime::dispatch_single_host_action(
                state,
                koma_lua::phodopus_bridge::QueuedHostAction::ClickPointer {
                    ws_id,
                    expected_epoch: active_epoch,
                    target,
                    opts: koma_lua::schema::LuaClickOptsInput {
                        button,
                        clicks,
                        hold_ms: 25,
                    },
                },
            );
            Ok((json!({ "ok": true, "epoch": active_epoch }), Vec::new()))
        }

        "vseat.drag" => {
            let ws_id = parse_ws_id(&req.params)?;
            let expected_epoch = parse_expected_epoch(&req.params)?;
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

            let parse_pt = |sub: Option<&Value>| -> koma_model::AnchorPoint {
                if let Some(cell) = sub.and_then(|v| v.get("grid_225")).and_then(Value::as_str) {
                    koma_model::AnchorPoint::Grid225(cell.to_owned())
                } else {
                    let x = sub
                        .and_then(|v| v.get("x"))
                        .and_then(Value::as_f64)
                        .unwrap_or(0.0);
                    let y = sub
                        .and_then(|v| v.get("y"))
                        .and_then(Value::as_f64)
                        .unwrap_or(0.0);
                    koma_model::AnchorPoint::AnchorOffset {
                        anchor: koma_model::Anchor9::TopLeft,
                        dx: x,
                        dy: y,
                    }
                }
            };
            let from_pt = parse_pt(req.params.get("from"));
            let to_pt = parse_pt(req.params.get("to"));
            let button = req
                .params
                .get("button")
                .and_then(Value::as_str)
                .unwrap_or("left")
                .to_owned();
            let duration_ms = req
                .params
                .get("duration_ms")
                .and_then(Value::as_u64)
                .unwrap_or(110);

            crate::macro_runtime::dispatch_single_host_action(
                state,
                koma_lua::phodopus_bridge::QueuedHostAction::MovePointer {
                    ws_id: ws_id.clone(),
                    expected_epoch: active_epoch,
                    target: from_pt,
                    kinematics: koma_lua::schema::LuaKinematicsInput {
                        profile: koma_proto::KinematicProfileKind::Instant,
                        duration_ms: 0,
                        arc_curvature: 0.0,
                    },
                },
            );
            crate::macro_runtime::dispatch_single_host_action(
                state,
                koma_lua::phodopus_bridge::QueuedHostAction::PointerButton {
                    ws_id: ws_id.clone(),
                    expected_epoch: active_epoch,
                    button: button.clone(),
                    pressed: true,
                },
            );
            crate::macro_runtime::dispatch_single_host_action(
                state,
                koma_lua::phodopus_bridge::QueuedHostAction::MovePointer {
                    ws_id: ws_id.clone(),
                    expected_epoch: active_epoch,
                    target: to_pt,
                    kinematics: koma_lua::schema::LuaKinematicsInput {
                        profile: koma_proto::KinematicProfileKind::MinimumJerk,
                        duration_ms,
                        arc_curvature: 0.08,
                    },
                },
            );
            crate::macro_runtime::dispatch_single_host_action(
                state,
                koma_lua::phodopus_bridge::QueuedHostAction::PointerButton {
                    ws_id,
                    expected_epoch: active_epoch,
                    button,
                    pressed: false,
                },
            );
            Ok((json!({ "ok": true, "epoch": active_epoch }), Vec::new()))
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
            Ok((json!({ "ok": true, "epoch": active_epoch }), Vec::new()))
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
            Ok((json!({ "ok": true, "epoch": active_epoch }), Vec::new()))
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
            Ok((
                json!({
                    "generation": state.config_generation,
                    "reload_failures": state.config_reload_failures,
                    "active_path": active_path,
                    "last_error": state.config_error_banner,
                    "deferred_vm_pending": state.deferred_lua_sandbox.is_some(),
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
                vec![WindowSceneItem {
                    window: &mapped.window,
                    rect: mapped.rect,
                    is_floating: mapped.is_floating,
                    is_focused: true,
                    is_protected_surface: is_protected,
                    opacity: 1.0,
                    corner_radius: 0.0,
                    wants_blur: false,
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
            let elements = assemble_workspace_elements(
                renderer,
                viewport,
                Scale::from(viewport.scale),
                &lease_state,
                RenderTargetKind::AgentCapture,
                params.overlay_kind,
                None,
                None,
                &scene_items,
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
