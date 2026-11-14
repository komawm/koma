use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::{
    error::{ErrorSymbol, IpcError},
    ids::WorkspaceId,
};

/// Monotonically ordered workspace privilege level (`IPC_SPEC.md` §3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrivilegeLevel {
    /// Permits `scene.*`, `capture.*`, and read-only event subscriptions.
    ObserveOnly,
    /// Adds `lease.*`, `win.*`, `vseat.*`, and `lua.exec_tx` subject to Epoch fencing.
    Control,
    /// Adds `ws.set_frame_policy`, `ws.spawn_bound`, and `ws.destroy`.
    Owner,
}

/// Ephemeral 256-bit secret string (`koma_cap_<base64url>`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CapabilityToken(pub String);

impl CapabilityToken {
    pub const PREFIX: &'static str = "koma_cap_";

    #[must_use]
    pub fn is_well_formed(&self) -> bool {
        self.0.starts_with(Self::PREFIX) && self.0.len() >= Self::PREFIX.len() + 32
    }
}

/// Compositor-resident capability claims bound to an authenticated `koma-agent.sock` session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenClaims {
    pub label: String,
    pub expires_at_ms: u64,
    pub can_create_headless: bool,
    pub max_owned_workspaces: usize,
    pub workspace_grants: BTreeMap<WorkspaceId, PrivilegeLevel>,
    pub allow_clipboard_bridge_from: BTreeSet<WorkspaceId>,
}

/// Policy interface evaluated on every incoming `koma-agent.sock` request.
pub trait TokenAuthorizer {
    fn authorize_workspace(
        &self,
        ws_id: &WorkspaceId,
        required: PrivilegeLevel,
        now_ms: u64,
        is_headed_human_preempted: bool,
    ) -> Result<(), IpcError>;
}

impl TokenAuthorizer for TokenClaims {
    fn authorize_workspace(
        &self,
        ws_id: &WorkspaceId,
        required: PrivilegeLevel,
        now_ms: u64,
        is_headed_human_preempted: bool,
    ) -> Result<(), IpcError> {
        if now_ms >= self.expires_at_ms {
            return Err(IpcError::new(
                ErrorSymbol::ErrUnauthenticated,
                "capability token has expired",
            ));
        }

        // Hard Isolation Rule (IPC_SPEC.md §3.1): agent tokens never hold Control/Owner
        // over a physical monitor workspace in HumanPreempted mode.
        if is_headed_human_preempted && required >= PrivilegeLevel::Control {
            return Err(IpcError::new(
                ErrorSymbol::ErrCapabilityDenied,
                "agent tokens cannot control a physical workspace in HumanPreempted mode",
            ));
        }

        let Some(&granted) = self.workspace_grants.get(ws_id) else {
            return Err(IpcError::new(
                ErrorSymbol::ErrCapabilityDenied,
                format!("no workspace grant for {ws_id}"),
            ));
        };

        if granted < required {
            return Err(IpcError::new(
                ErrorSymbol::ErrCapabilityDenied,
                format!("workspace {ws_id} requires {required:?}, token holds {granted:?}"),
            ));
        }

        Ok(())
    }
}
