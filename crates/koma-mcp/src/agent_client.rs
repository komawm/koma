//! Authenticated `koma-agent.sock` client performing `CapabilityToken` handshake, `SCM_RIGHTS`
//! zero-copy frame capture, and `Epoch`-fenced Computer Use / `Phodopus` macro RPCs.

use std::{
    collections::BTreeMap,
    env, io,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STD};
use koma_lua::MacroDefinition;
use koma_proto::{
    ADMIN_SOCKET_NAME, AGENT_SOCKET_NAME, CapabilityToken, Epoch, ErrorSymbol, IpcError,
    IpcRequest, IpcResponse, OverlayKind, PrivilegeLevel, SessionId, WorkspaceId,
    rpc::CaptureFrameResult, scm_unix::sync_roundtrip,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    epoch_loop::{EpochObservationTracker, RecoveryDirective},
    png_codec::read_sealed_memfd_and_encode_png,
};

#[derive(Debug, Clone)]
pub struct CapturedPngObservation {
    pub ws_id: WorkspaceId,
    pub epoch: Epoch,
    pub width: u32,
    pub height: u32,
    pub has_protected_blackout: bool,
    pub grid_cells: Option<u16>,
    pub png_base64: String,
}

#[derive(Debug, Deserialize)]
struct AuthReply {
    session_id: SessionId,
    can_create_headless: bool,
    grants: BTreeMap<WorkspaceId, PrivilegeLevel>,
}

#[derive(Debug, Deserialize)]
struct MacroListReply {
    macros: Vec<MacroDefinition>,
}

pub struct KomaAgentSession {
    stream: UnixStream,
    next_id: u64,
    pub session_id: SessionId,
    pub can_create_headless: bool,
    pub grants: BTreeMap<WorkspaceId, PrivilegeLevel>,
    pub epochs: EpochObservationTracker,
}

impl KomaAgentSession {
    /// Connects to `koma-agent.sock` and immediately completes the mandatory Frame `#1`
    /// `auth.authenticate` handshake using `token` (`docs/IPC_SPEC.md` §2.1 & §5.1).
    pub fn connect_and_authenticate(
        agent_sock_path: &Path,
        token: &CapabilityToken,
    ) -> Result<Self, IpcError> {
        if !token.is_well_formed() {
            return Err(IpcError::new(
                ErrorSymbol::ErrUnauthenticated,
                "malformed KOMA_CAPABILITY_TOKEN (expected `koma_cap_<base64url>`)",
            ));
        }

        let stream = UnixStream::connect(agent_sock_path).map_err(|e| {
            IpcError::new(
                ErrorSymbol::ErrUnauthenticated,
                format!("failed connecting to {}: {e}", agent_sock_path.display()),
            )
        })?;

        let auth_req = IpcRequest {
            id: 1,
            method: "auth.authenticate".to_owned(),
            params: json!({
                "token": token.0,
                "client_name": "koma-mcp-stdio",
                "protocol_version": koma_proto::PROTOCOL_VERSION,
            }),
        };

        let (resp, _): (IpcResponse, _) =
            sync_roundtrip(&stream, &auth_req, Vec::new()).map_err(|e| {
                IpcError::new(
                    ErrorSymbol::ErrUnauthenticated,
                    format!("auth I/O failed: {e}"),
                )
            })?;
        if let Some(err) = resp.error {
            return Err(err);
        }

        let reply: AuthReply = serde_json::from_value(resp.result.unwrap_or_default())
            .map_err(|e| IpcError::new(ErrorSymbol::ErrInvalidParams, e.to_string()))?;

        Ok(Self {
            stream,
            next_id: 2,
            session_id: reply.session_id,
            can_create_headless: reply.can_create_headless,
            grants: reply.grants,
            epochs: EpochObservationTracker::default(),
        })
    }

    /// Resolves socket paths from `$XDG_RUNTIME_DIR/koma/$KOMA_INSTANCE_SIGNATURE/` and authenticates
    /// using `KOMA_CAPABILITY_TOKEN` (or mints an ephemeral scoped token via `koma-admin.sock` when
    /// `auto_mint_dev_token` is explicitly enabled by the human's MCP config).
    pub fn from_env(auto_mint_dev_token: bool) -> Result<Self, IpcError> {
        let base = env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .ok_or_else(|| IpcError::new(ErrorSymbol::ErrInvalidParams, "XDG_RUNTIME_DIR unset"))?;
        let sig = env::var("KOMA_INSTANCE_SIGNATURE").map_err(|_| {
            IpcError::new(
                ErrorSymbol::ErrInvalidParams,
                "KOMA_INSTANCE_SIGNATURE unset",
            )
        })?;
        let dir = base.join("koma").join(sig);

        let token = match env::var("KOMA_CAPABILITY_TOKEN") {
            Ok(val) if !val.trim().is_empty() => CapabilityToken(val.trim().to_owned()),
            _ if auto_mint_dev_token => mint_dev_token_via_admin(&dir.join(ADMIN_SOCKET_NAME))?,
            _ => {
                return Err(IpcError::new(
                    ErrorSymbol::ErrUnauthenticated,
                    "KOMA_CAPABILITY_TOKEN env var is required (or pass `--auto-mint-admin` in trusted local dev)",
                ));
            }
        };

        Self::connect_and_authenticate(&dir.join(AGENT_SOCKET_NAME), &token)
    }

    fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    pub fn raw_rpc(&mut self, method: &str, params: Value) -> Result<Value, IpcError> {
        let req = IpcRequest {
            id: self.alloc_id(),
            method: method.to_owned(),
            params,
        };
        let (resp, _): (IpcResponse, _) =
            sync_roundtrip(&self.stream, &req, Vec::new()).map_err(|e| {
                IpcError::new(
                    ErrorSymbol::ErrInvalidParams,
                    format!("IPC socket error: {e}"),
                )
            })?;
        if let Some(err) = resp.error {
            return Err(err);
        }
        Ok(resp.result.unwrap_or(Value::Null))
    }

    /// Queries `scene.inventory` and updates `EpochObservationTracker` for all returned workspaces.
    pub fn fetch_inventory_and_record_epochs(&mut self) -> Result<Value, IpcError> {
        let val = self.raw_rpc("scene.inventory", json!({}))?;
        if let Some(arr) = val.get("workspaces").and_then(Value::as_array) {
            for item in arr {
                if let (Some(ws_s), Some(ep_n)) = (
                    item.get("ws_id").and_then(Value::as_str),
                    item.get("epoch").and_then(Value::as_u64),
                ) {
                    let preempted = item
                        .get("is_human_preempted")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    self.epochs
                        .record_observed(WorkspaceId::new(ws_s), Epoch(ep_n), preempted);
                }
            }
        }
        Ok(val)
    }

    /// Queries registered `Phodopus` macros (`macro.list`) so `tools/list` exposes dynamic `macro__*` tools.
    pub fn fetch_registered_macros(&mut self) -> Vec<MacroDefinition> {
        self.raw_rpc("macro.list", json!({}))
            .ok()
            .and_then(|v| serde_json::from_value::<MacroListReply>(v).ok())
            .map(|r| r.macros)
            .unwrap_or_default()
    }

    /// Captures a workspace/window via zero-copy `SCM_RIGHTS` sealed `memfd` (`mmap` `PROT_READ`),
    /// encodes an `image/png` Base64 payload, and records the observed `Epoch` as `Fresh`!
    pub fn capture_png_observation(
        &mut self,
        ws_id: &WorkspaceId,
        overlay_kind: OverlayKind,
        max_edge: u32,
    ) -> Result<CapturedPngObservation, IpcError> {
        let req = IpcRequest {
            id: self.alloc_id(),
            method: "capture.frame".to_owned(),
            params: json!({
                "ws_id": ws_id,
                "win_id": null,
                "overlay_kind": overlay_kind,
                "max_edge": max_edge,
                "format": "raw_rgba8",
                "transport": "shm_fd",
            }),
        };

        let (resp, mut fds): (IpcResponse, _) = sync_roundtrip(&self.stream, &req, Vec::new())
            .map_err(|e| {
                IpcError::new(
                    ErrorSymbol::ErrInvalidParams,
                    format!("capture IPC failed: {e}"),
                )
            })?;
        if let Some(err) = resp.error {
            return Err(err);
        }

        let meta: CaptureFrameResult = serde_json::from_value(resp.result.unwrap_or_default())
            .map_err(|e| IpcError::new(ErrorSymbol::ErrInvalidParams, e.to_string()))?;
        let memfd = fds.pop().ok_or_else(|| {
            IpcError::new(
                ErrorSymbol::ErrInvalidParams,
                "compositor reply omitted SCM_RIGHTS sealed memfd",
            )
        })?;

        let (out_w, out_h, png_bytes) = read_sealed_memfd_and_encode_png(
            memfd,
            meta.width,
            meta.height,
            meta.byte_len,
            max_edge,
        )
        .map_err(|e| {
            IpcError::new(
                ErrorSymbol::ErrInvalidParams,
                format!("PNG encode failed: {e}"),
            )
        })?;

        self.epochs
            .record_observed(ws_id.clone(), meta.epoch, false);

        Ok(CapturedPngObservation {
            ws_id: ws_id.clone(),
            epoch: meta.epoch,
            width: out_w,
            height: out_h,
            has_protected_blackout: meta.has_protected_blackout,
            grid_cells: meta.grid_cells,
            png_base64: BASE64_STD.encode(&png_bytes),
        })
    }

    /// Executes an `Epoch`-fenced mutating RPC (`vseat.*`, `win.activate`, `lua.exec_tx`) and
    /// implements the **Deterministic `ERR_STALE_EPOCH` Re-Observation Closed Loop**:
    /// - Never blindly retries a rejected action with a new epoch.
    /// - On `4090 ERR_STALE_EPOCH`, automatically captures a fresh `SCM_RIGHTS` screenshot (`grid_225`),
    ///   updates `EpochObservationTracker` to `Fresh(new_epoch)`, and returns `MutateOutcome::StaleReobserved`
    ///   with the fresh image attached so the LLM recomputes in 1 turn!
    pub fn execute_fenced_mutation(
        &mut self,
        ws_id: &WorkspaceId,
        explicit_epoch: Option<Epoch>,
        method: &str,
        mut params: Value,
    ) -> MutateOutcome {
        let target_epoch = match self.epochs.resolve_mutation_epoch(ws_id, explicit_epoch) {
            Ok(ep) => ep,
            Err(pre_err) => {
                // Local ledger knows workspace is unobserved or stale -> auto-observe right now!
                if pre_err.symbol == ErrorSymbol::ErrStaleEpoch
                    && let Ok(fresh) =
                        self.capture_png_observation(ws_id, OverlayKind::Grid225, 1920)
                {
                    return MutateOutcome::StaleReobserved {
                        rejected_error: pre_err,
                        fresh_observation: fresh,
                    };
                }
                return MutateOutcome::Error(pre_err);
            }
        };

        if let Some(obj) = params.as_object_mut() {
            obj.insert("ws_id".to_owned(), json!(ws_id));
            obj.insert("expected_epoch".to_owned(), json!(target_epoch));
        }

        match self.raw_rpc(method, params) {
            Ok(result_val) => {
                if let Some(new_ep) = result_val.get("epoch").and_then(Value::as_u64) {
                    self.epochs
                        .record_verified_mutation_epoch(ws_id, Epoch(new_ep));
                }
                MutateOutcome::Applied(result_val)
            }
            Err(ipc_err) => match self.epochs.on_mutation_ipc_error(ws_id, &ipc_err) {
                RecoveryDirective::AutoCaptureFreshSnapshot { .. } => {
                    match self.capture_png_observation(ws_id, OverlayKind::Grid225, 1920) {
                        Ok(fresh) => MutateOutcome::StaleReobserved {
                            rejected_error: ipc_err,
                            fresh_observation: fresh,
                        },
                        Err(_) => MutateOutcome::Error(ipc_err),
                    }
                }
                RecoveryDirective::HumanTakeoverPaused { .. }
                | RecoveryDirective::ProtectedSurfaceBlocked
                | RecoveryDirective::PassThroughError => MutateOutcome::Error(ipc_err),
            },
        }
    }
}

#[derive(Debug)]
pub enum MutateOutcome {
    Applied(Value),
    StaleReobserved {
        rejected_error: IpcError,
        fresh_observation: CapturedPngObservation,
    },
    Error(IpcError),
}

fn mint_dev_token_via_admin(admin_sock: &Path) -> Result<CapabilityToken, IpcError> {
    let stream = UnixStream::connect(admin_sock).map_err(|e| {
        IpcError::new(
            ErrorSymbol::ErrUnauthenticated,
            format!(
                "--auto-mint-admin failed connecting to {}: {e}",
                admin_sock.display()
            ),
        )
    })?;
    let req = IpcRequest {
        id: 1,
        method: "auth.mint_token".to_owned(),
        params: json!({
            "label": "koma-mcp-local-dev",
            "ttl_secs": 28_800,
            "can_create_headless": true,
            "max_owned_workspaces": 4,
            "workspace_grants": { "ws_01": "observe_only" },
        }),
    };
    let (resp, _): (IpcResponse, _) = sync_roundtrip(&stream, &req, Vec::new()).map_err(|e| {
        IpcError::new(
            ErrorSymbol::ErrInvalidParams,
            io::Error::other(e).to_string(),
        )
    })?;
    if let Some(err) = resp.error {
        return Err(err);
    }
    let tok = resp
        .result
        .and_then(|v| v.get("token").and_then(Value::as_str).map(str::to_owned))
        .ok_or_else(|| IpcError::new(ErrorSymbol::ErrInvalidParams, "missing token field"))?;
    Ok(CapabilityToken(tok))
}
