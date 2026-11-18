#![deny(unsafe_code)]

//! Wire protocol, `SCM_RIGHTS` ancillary FD transport, capability tokens, and event rings for `Koma`.

pub mod capability;
pub mod error;
pub mod event;
pub mod frame;
pub mod ids;
pub mod overview;
pub mod rpc;

#[cfg(unix)]
pub mod scm_unix;

pub use crate::{
    capability::{CapabilityToken, PrivilegeLevel, TokenAuthorizer, TokenClaims},
    error::{
        DeliveryDisposition, DeliverySemantics, ErrorSymbol, IpcError, RetryDisposition,
        default_disposition_for,
    },
    event::{EventRingBuffer, EventTopic, IpcEventEnvelope, WorkspaceEvent},
    frame::{
        ADMIN_SOCKET_NAME, AGENT_SOCKET_NAME, AncillaryFdRef, FrameDecodeError, IpcRequest,
        IpcResponse, MAX_FRAME_BYTES, MAX_SCM_FDS, PROTOCOL_VERSION, decode_frame_header,
        encode_frame,
    },
    ids::{Epoch, MacroId, OutputId, SeatId, SessionId, WindowId, WorkspaceId},
    rpc::{
        CursorLabelParams, CursorSettleParams, HitTestParams, HitTestResult, KinematicProfileKind,
        OverlayKind, PathSplineKind, PointerPathParams, RelativeMoveParams, TextInjectStrategy,
        TransportKind, WheelScrollParams,
    },
};
