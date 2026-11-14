//! `koma-admin.sock` client fetching spatial inventory, toggling Tier 3 `PreviewThumbnail`
//! subscriptions, and reading zero-copy `SCM_RIGHTS` sealed `memfd` frames.

use std::{
    env,
    fs::File,
    io::{self, Read as _, Write as _},
    os::unix::net::UnixStream,
    path::PathBuf,
};

use koma_proto::{
    ADMIN_SOCKET_NAME, Epoch, IpcRequest, IpcResponse, WorkspaceId,
    rpc::{CaptureFrameResult, WorkspaceModeSpec},
    scm_unix::sync_roundtrip,
};
use serde::Deserialize;
use serde_json::json;

use crate::compositor_card::RawRgbaFrame;

pub const OVERVIEW_CONTROL_SOCK_NAME: &str = "koma-overview.ctl.sock";

#[derive(Debug, Clone, Deserialize)]
pub struct InventoryWorkspaceItem {
    pub ws_id: WorkspaceId,
    pub name: String,
    pub epoch: Epoch,
    pub is_human_preempted: bool,
    pub frame_tier: String,
}

#[derive(Debug, Clone, Deserialize)]
struct InventoryReply {
    workspaces: Vec<InventoryWorkspaceItem>,
}

pub fn runtime_instance_dir() -> io::Result<PathBuf> {
    let base = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "XDG_RUNTIME_DIR not set"))?;
    let sig = env::var("KOMA_INSTANCE_SIGNATURE")
        .map_err(|_| io::Error::new(io::ErrorKind::NotFound, "KOMA_INSTANCE_SIGNATURE not set"))?;
    Ok(base.join("koma").join(sig))
}

pub fn control_socket_path() -> io::Result<PathBuf> {
    Ok(runtime_instance_dir()?.join(OVERVIEW_CONTROL_SOCK_NAME))
}

pub struct AdminIpcClient {
    stream: UnixStream,
    next_id: u64,
}

impl AdminIpcClient {
    pub fn connect() -> io::Result<Self> {
        let sock_path = runtime_instance_dir()?.join(ADMIN_SOCKET_NAME);
        let stream = UnixStream::connect(sock_path)?;
        Ok(Self { stream, next_id: 1 })
    }

    fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    pub fn fetch_inventory(&mut self) -> io::Result<Vec<InventoryWorkspaceItem>> {
        let req = IpcRequest {
            id: self.alloc_id(),
            method: "scene.inventory".to_owned(),
            params: json!({}),
        };
        let (resp, _): (IpcResponse, _) = sync_roundtrip(&self.stream, &req, Vec::new())?;
        if let Some(err) = resp.error {
            return Err(io::Error::other(err.to_string()));
        }
        let parsed: InventoryReply = serde_json::from_value(resp.result.unwrap_or_default())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        Ok(parsed.workspaces)
    }

    pub fn set_preview_subscribe(
        &mut self,
        ws_id: &WorkspaceId,
        subscribe: bool,
    ) -> io::Result<()> {
        let req = IpcRequest {
            id: self.alloc_id(),
            method: "ws.set_preview_subscribe".to_owned(),
            params: json!({
                "ws_id": ws_id,
                "subscribe": subscribe,
            }),
        };
        let (resp, _): (IpcResponse, _) = sync_roundtrip(&self.stream, &req, Vec::new())?;
        if let Some(err) = resp.error {
            return Err(io::Error::other(err.to_string()));
        }
        Ok(())
    }

    pub fn set_workspace_mode(
        &mut self,
        ws_id: &WorkspaceId,
        mode: WorkspaceModeSpec,
    ) -> io::Result<()> {
        let req = IpcRequest {
            id: self.alloc_id(),
            method: "ws.set_mode".to_owned(),
            params: json!({
                "ws_id": ws_id,
                "mode": mode,
                "output": null,
            }),
        };
        let (resp, _): (IpcResponse, _) = sync_roundtrip(&self.stream, &req, Vec::new())?;
        if let Some(err) = resp.error {
            return Err(io::Error::other(err.to_string()));
        }
        Ok(())
    }

    /// Requests a Render-on-Read snapshot via `capture.frame` (`transport = "shm_fd"`) and reads
    /// the write-sealed `memfd` attached at `SCM_RIGHTS` `fd_index = 0`.
    pub fn capture_zero_copy_frame(
        &mut self,
        ws_id: &WorkspaceId,
        max_edge: u32,
    ) -> io::Result<RawRgbaFrame> {
        let req = IpcRequest {
            id: self.alloc_id(),
            method: "capture.frame".to_owned(),
            params: json!({
                "ws_id": ws_id,
                "win_id": null,
                "overlay_kind": "none",
                "max_edge": max_edge,
                "format": "raw_rgba8",
                "transport": "shm_fd",
            }),
        };
        let (resp, mut fds): (IpcResponse, _) = sync_roundtrip(&self.stream, &req, Vec::new())?;
        if let Some(err) = resp.error {
            return Err(io::Error::other(err.to_string()));
        }
        let meta: CaptureFrameResult = serde_json::from_value(resp.result.unwrap_or_default())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        let fd = fds.pop().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "missing SCM_RIGHTS memfd in capture.frame",
            )
        })?;

        let mut shm_file = File::from(fd);
        let mut pixels = Vec::with_capacity(meta.byte_len);
        shm_file.read_to_end(&mut pixels)?;
        Ok(RawRgbaFrame {
            width: meta.width,
            height: meta.height,
            pixels,
        })
    }
}

/// Sends a one-line control command (`"hover ws_01 180 34\n"`, `"leave\n"`, `"toggle\n"`)
/// from status-bar scripts (`Waybar` / `Ironbar`) or `komactl` to the running `koma-overview --daemon`.
pub fn send_daemon_control_line(line: &str) -> io::Result<()> {
    let path = control_socket_path()?;
    let mut stream = UnixStream::connect(path)?;
    stream.write_all(line.as_bytes())?;
    stream.write_all(b"\n")?;
    Ok(())
}
