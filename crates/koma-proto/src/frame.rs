use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::error::IpcError;

pub const PROTOCOL_VERSION: &str = "0.1";
pub const ADMIN_SOCKET_NAME: &str = "koma-admin.sock";
pub const AGENT_SOCKET_NAME: &str = "koma-agent.sock";
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_SCM_FDS: usize = 4;

/// Zero-based index into ancillary `SCM_RIGHTS` file descriptors accompanying a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AncillaryFdRef(pub u8);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcRequest {
    pub id: u64,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcResponse {
    pub id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<IpcError>,
}

impl IpcResponse {
    #[must_use]
    pub fn ok(id: u64, result: serde_json::Value) -> Self {
        Self {
            id,
            result: Some(result),
            error: None,
        }
    }

    #[must_use]
    pub fn err(id: u64, error: IpcError) -> Self {
        Self {
            id,
            result: None,
            error: Some(error),
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum FrameDecodeError {
    #[error("empty zero-length IPC frame is invalid")]
    ZeroLength,
    #[error("frame length {0} exceeds 16 MiB ceiling ({MAX_FRAME_BYTES})")]
    FrameTooLarge(usize),
    #[error("JSON serialization/deserialization failed: {0}")]
    Json(String),
}

/// Encodes a serializable envelope with a 4-byte little-endian byte length header.
pub fn encode_frame<T: Serialize>(msg: &T) -> Result<Vec<u8>, FrameDecodeError> {
    let payload = serde_json::to_vec(msg).map_err(|e| FrameDecodeError::Json(e.to_string()))?;
    if payload.is_empty() {
        return Err(FrameDecodeError::ZeroLength);
    }
    if payload.len() > MAX_FRAME_BYTES {
        return Err(FrameDecodeError::FrameTooLarge(payload.len()));
    }
    let len_le = (payload.len() as u32).to_le_bytes();
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&len_le);
    out.extend_from_slice(&payload);
    Ok(out)
}

/// Validates a 4-byte little-endian frame header and returns the expected payload size.
pub const fn decode_frame_header(header: [u8; 4]) -> Result<usize, FrameDecodeError> {
    let len = u32::from_le_bytes(header) as usize;
    if len == 0 {
        return Err(FrameDecodeError::ZeroLength);
    }
    if len > MAX_FRAME_BYTES {
        return Err(FrameDecodeError::FrameTooLarge(len));
    }
    Ok(len)
}
