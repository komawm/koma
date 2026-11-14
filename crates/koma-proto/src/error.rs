use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::ids::Epoch;

/// Canonical IPC error symbols (`IPC_SPEC.md` §4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorSymbol {
    ErrInvalidParams,
    ErrFrameTooLarge,
    ErrUnauthenticated,
    ErrCapabilityDenied,
    ErrProtectedSurface,
    ErrNotFound,
    ErrStaleEpoch,
    ErrHumanPreempted,
    ErrLeaseHeldByOther,
    ErrQuotaExceeded,
    ErrSettleTimeout,
}

impl ErrorSymbol {
    #[must_use]
    pub const fn code(self) -> u16 {
        match self {
            Self::ErrInvalidParams => 4000,
            Self::ErrFrameTooLarge => 4001,
            Self::ErrUnauthenticated => 4010,
            Self::ErrCapabilityDenied => 4030,
            Self::ErrProtectedSurface => 4032,
            Self::ErrNotFound => 4040,
            Self::ErrStaleEpoch => 4090,
            Self::ErrHumanPreempted => 4091,
            Self::ErrLeaseHeldByOther => 4092,
            Self::ErrQuotaExceeded => 4290,
            Self::ErrSettleTimeout => 5040,
        }
    }

    #[must_use]
    pub const fn default_retryable(self) -> bool {
        matches!(
            self,
            Self::ErrHumanPreempted | Self::ErrLeaseHeldByOther | Self::ErrSettleTimeout
        )
    }
}

/// Wire-serializable error payload returned inside `IpcResponse::error`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Error)]
#[error("[{code} {symbol:?}] {message}")]
pub struct IpcError {
    pub code: u16,
    pub symbol: ErrorSymbol,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_epoch: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retryable: Option<bool>,
}

impl IpcError {
    #[must_use]
    pub fn new(symbol: ErrorSymbol, message: impl Into<String>) -> Self {
        Self {
            code: symbol.code(),
            symbol,
            message: message.into(),
            current_epoch: None,
            retryable: Some(symbol.default_retryable()),
        }
    }

    #[must_use]
    pub fn stale_epoch(expected: Epoch, current: Epoch) -> Self {
        Self {
            code: ErrorSymbol::ErrStaleEpoch.code(),
            symbol: ErrorSymbol::ErrStaleEpoch,
            message: format!(
                "stale workspace epoch: expected {}, current {}; re-observe before acting",
                expected, current
            ),
            current_epoch: Some(current.get()),
            retryable: Some(false),
        }
    }

    #[must_use]
    pub fn human_preempted(current: Epoch) -> Self {
        Self {
            code: ErrorSymbol::ErrHumanPreempted.code(),
            symbol: ErrorSymbol::ErrHumanPreempted,
            message: "workspace is currently under human Mod+G takeover; await workspace.resumed"
                .to_owned(),
            current_epoch: Some(current.get()),
            retryable: Some(true),
        }
    }

    #[must_use]
    pub fn lease_held_by_other(current: Epoch) -> Self {
        Self {
            code: ErrorSymbol::ErrLeaseHeldByOther.code(),
            symbol: ErrorSymbol::ErrLeaseHeldByOther,
            message: "another agent session holds exclusive write lease on this workspace"
                .to_owned(),
            current_epoch: Some(current.get()),
            retryable: Some(true),
        }
    }

    #[must_use]
    pub fn protected_surface(detail: impl Into<String>) -> Self {
        Self::new(ErrorSymbol::ErrProtectedSurface, detail)
    }
}
