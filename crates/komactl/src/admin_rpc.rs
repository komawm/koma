//! Synchronous `koma-admin.sock` RPC helper for `komactl`, supporting automatic runtime
//! instance discovery when invoked from external SSH / host terminals.

use std::{
    collections::BTreeMap,
    env, fs, io,
    os::{fd::OwnedFd, unix::net::UnixStream},
    path::{Path, PathBuf},
};

use koma_proto::{
    ADMIN_SOCKET_NAME, AGENT_SOCKET_NAME, IpcRequest, IpcResponse, PrivilegeLevel, WorkspaceId,
    scm_unix::sync_roundtrip,
};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug, Clone)]
pub struct DiscoveredInstance {
    pub runtime_dir: PathBuf,
    pub admin_socket: PathBuf,
    pub agent_socket: PathBuf,
}

#[derive(Debug, Deserialize)]
pub struct CreatedWorkspaceReply {
    pub ws_id: WorkspaceId,
    pub epoch: u64,
    pub vseat_id: String,
}

#[derive(Debug, Deserialize)]
pub struct CreatedWaylandJailReply {
    pub ws_id: WorkspaceId,
    pub label: String,
    pub restricted: bool,
    pub socket_path: PathBuf,
}

#[derive(Debug, Deserialize)]
pub struct MintedTokenReply {
    pub token: String,
}

/// Resolves `$XDG_RUNTIME_DIR/koma/<signature>/koma-admin.sock` either from
/// `KOMA_INSTANCE_SIGNATURE` or by scanning `$XDG_RUNTIME_DIR/koma/` for an active socket.
pub fn discover_koma_instance() -> io::Result<DiscoveredInstance> {
    let runtime_base = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(format!("/run/user/{}", rustix::process::getuid().as_raw()))
        });
    let koma_root = runtime_base.join("koma");

    if let Ok(sig) = env::var("KOMA_INSTANCE_SIGNATURE") {
        let dir = koma_root.join(sig);
        let admin_socket = dir.join(ADMIN_SOCKET_NAME);
        let agent_socket = dir.join(AGENT_SOCKET_NAME);
        return Ok(DiscoveredInstance {
            runtime_dir: dir,
            admin_socket,
            agent_socket,
        });
    }

    // Fallback when `komactl` is invoked from an external TTY / SSH / systemd service:
    // probe `$XDG_RUNTIME_DIR/koma/*/` and select the newest live `koma-admin.sock`.
    let entries = fs::read_dir(&koma_root).map_err(|_| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "KOMA_INSTANCE_SIGNATURE is unset and {} does not exist",
                koma_root.display()
            ),
        )
    })?;

    let mut candidates: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.join(ADMIN_SOCKET_NAME).exists())
        .collect();
    candidates.sort();

    for dir in candidates.into_iter().rev() {
        let admin_socket = dir.join(ADMIN_SOCKET_NAME);
        if UnixStream::connect(&admin_socket).is_ok() {
            let agent_socket = dir.join(AGENT_SOCKET_NAME);
            return Ok(DiscoveredInstance {
                runtime_dir: dir,
                admin_socket,
                agent_socket,
            });
        }
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "no active koma-admin.sock found under $XDG_RUNTIME_DIR/koma/",
    ))
}

pub struct AdminClient {
    instance: DiscoveredInstance,
    stream: UnixStream,
    next_id: u64,
}

impl AdminClient {
    pub fn connect() -> io::Result<Self> {
        let instance = discover_koma_instance()?;
        let stream = UnixStream::connect(&instance.admin_socket)?;
        Ok(Self {
            instance,
            stream,
            next_id: 1,
        })
    }

    pub fn connect_at(admin_socket: &Path) -> io::Result<Self> {
        let runtime_dir = admin_socket
            .parent()
            .unwrap_or_else(|| Path::new("/tmp"))
            .to_path_buf();
        let agent_socket = runtime_dir.join(AGENT_SOCKET_NAME);
        let stream = UnixStream::connect(admin_socket)?;
        Ok(Self {
            instance: DiscoveredInstance {
                runtime_dir,
                admin_socket: admin_socket.to_path_buf(),
                agent_socket,
            },
            stream,
            next_id: 1,
        })
    }

    #[must_use]
    pub fn instance(&self) -> &DiscoveredInstance {
        &self.instance
    }

    pub fn call_raw(&mut self, method: &str, params: Value) -> io::Result<(Value, Vec<OwnedFd>)> {
        let id = self.next_id;
        self.next_id += 1;
        let req = IpcRequest {
            id,
            method: method.to_owned(),
            params,
        };
        let (resp, fds): (IpcResponse, Vec<OwnedFd>) =
            sync_roundtrip(&self.stream, &req, Vec::new())?;
        if let Some(ipc_err) = resp.error {
            return Err(io::Error::other(format!(
                "RPC `{method}` failed ({}): {}",
                ipc_err.code, ipc_err.message
            )));
        }
        Ok((resp.result.unwrap_or(Value::Null), fds))
    }

    pub fn workspace_exists(&mut self, ws_id: &str) -> io::Result<bool> {
        let (val, _) = self.call_raw("scene.inventory", json!({}))?;
        let exists = val
            .get("workspaces")
            .and_then(Value::as_array)
            .is_some_and(|arr| {
                arr.iter()
                    .any(|w| w.get("ws_id").and_then(Value::as_str) == Some(ws_id))
            });
        Ok(exists)
    }

    pub fn create_headless_workspace(
        &mut self,
        name: &str,
        width: u32,
        height: u32,
        scale: f64,
    ) -> io::Result<CreatedWorkspaceReply> {
        let (val, _) = self.call_raw(
            "ws.create",
            json!({
                "name": name,
                "viewport": {
                    "width": width,
                    "height": height,
                    "scale": scale,
                },
                "acquire_lease": false,
            }),
        )?;
        serde_json::from_value(val)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
    }

    pub fn create_wayland_jail(
        &mut self,
        ws_id: &str,
        label: &str,
    ) -> io::Result<CreatedWaylandJailReply> {
        let (val, _) = self.call_raw(
            "sandbox.create_wayland_jail",
            json!({
                "ws_id": ws_id,
                "label": label,
            }),
        )?;
        serde_json::from_value(val)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
    }

    pub fn revoke_wayland_jail(&mut self, ws_id: &str) -> io::Result<bool> {
        let (val, _) = self.call_raw(
            "sandbox.revoke_wayland_jail",
            json!({
                "ws_id": ws_id,
            }),
        )?;
        Ok(val.get("revoked").and_then(Value::as_bool).unwrap_or(false))
    }

    /// Mints a least-privilege `CapabilityToken` scoped strictly to `ws_id` (`can_create_headless = false`),
    /// suitable for passing into a containerized AI coding/GUI agent alongside `koma-agent.sock`.
    pub fn mint_workspace_scoped_agent_token(
        &mut self,
        label: &str,
        ws_id: &WorkspaceId,
        ttl_secs: u64,
    ) -> io::Result<String> {
        let mut grants = BTreeMap::new();
        grants.insert(ws_id.as_str().to_owned(), PrivilegeLevel::Owner);
        let (val, _) = self.call_raw(
            "auth.mint_token",
            json!({
                "label": label,
                "ttl_secs": ttl_secs,
                "can_create_headless": false,
                "max_owned_workspaces": 1,
                "workspace_grants": grants,
            }),
        )?;
        let parsed: MintedTokenReply = serde_json::from_value(val)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        Ok(parsed.token)
    }
}
