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

/// 5-state delivery disposition tracking whether an IPC request reached its target surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryDisposition {
    #[default]
    Unknown,
    NotDelivered,
    DeliveryUncertain,
    DeliveredUnverified,
    DeliveredVerified,
}

/// Retry safety guidance paired with [`DeliveryDisposition`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetryDisposition {
    Safe,
    Unsafe,
    #[default]
    Unknown,
}

/// Combined delivery and retry safety semantics attached to IPC errors and input responses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DeliverySemantics {
    pub delivery: DeliveryDisposition,
    pub retry: RetryDisposition,
}

impl DeliverySemantics {
    #[must_use]
    pub const fn not_delivered_safe() -> Self {
        Self {
            delivery: DeliveryDisposition::NotDelivered,
            retry: RetryDisposition::Safe,
        }
    }

    #[must_use]
    pub const fn delivery_uncertain_unsafe() -> Self {
        Self {
            delivery: DeliveryDisposition::DeliveryUncertain,
            retry: RetryDisposition::Unsafe,
        }
    }

    #[must_use]
    pub const fn delivered_unverified_unsafe() -> Self {
        Self {
            delivery: DeliveryDisposition::DeliveredUnverified,
            retry: RetryDisposition::Unsafe,
        }
    }

    #[must_use]
    pub const fn delivered_verified() -> Self {
        Self {
            delivery: DeliveryDisposition::DeliveredVerified,
            retry: RetryDisposition::Unsafe,
        }
    }
}

/// Returns the default [`DeliverySemantics`] associated with a canonical [`ErrorSymbol`].
#[must_use]
pub const fn default_disposition_for(symbol: ErrorSymbol) -> DeliverySemantics {
    match symbol {
        ErrorSymbol::ErrStaleEpoch
        | ErrorSymbol::ErrInvalidParams
        | ErrorSymbol::ErrNotFound
        | ErrorSymbol::ErrUnauthenticated
        | ErrorSymbol::ErrCapabilityDenied
        | ErrorSymbol::ErrQuotaExceeded
        | ErrorSymbol::ErrHumanPreempted
        | ErrorSymbol::ErrProtectedSurface
        | ErrorSymbol::ErrFrameTooLarge => DeliverySemantics::not_delivered_safe(),
        ErrorSymbol::ErrLeaseHeldByOther | ErrorSymbol::ErrSettleTimeout => DeliverySemantics {
            delivery: DeliveryDisposition::Unknown,
            retry: RetryDisposition::Unknown,
        },
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disposition: Option<DeliverySemantics>,
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
            disposition: Some(default_disposition_for(symbol)),
        }
    }

    #[must_use]
    pub const fn with_disposition(mut self, disposition: DeliverySemantics) -> Self {
        self.disposition = Some(disposition);
        self
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
            disposition: Some(default_disposition_for(ErrorSymbol::ErrStaleEpoch)),
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
            disposition: Some(default_disposition_for(ErrorSymbol::ErrHumanPreempted)),
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
            disposition: Some(default_disposition_for(ErrorSymbol::ErrLeaseHeldByOther)),
        }
    }

    #[must_use]
    pub fn protected_surface(detail: impl Into<String>) -> Self {
        Self::new(ErrorSymbol::ErrProtectedSurface, detail)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipc_error_populates_delivery_and_retry_disposition_defaults() {
        for sym in [
            ErrorSymbol::ErrStaleEpoch,
            ErrorSymbol::ErrInvalidParams,
            ErrorSymbol::ErrNotFound,
            ErrorSymbol::ErrUnauthenticated,
            ErrorSymbol::ErrCapabilityDenied,
            ErrorSymbol::ErrQuotaExceeded,
            ErrorSymbol::ErrHumanPreempted,
            ErrorSymbol::ErrProtectedSurface,
            ErrorSymbol::ErrFrameTooLarge,
        ] {
            let err = IpcError::new(sym, "rejected before delivery");
            assert_eq!(
                err.disposition,
                Some(DeliverySemantics::not_delivered_safe())
            );
        }

        let stale = IpcError::stale_epoch(Epoch(2), Epoch(3));
        assert_eq!(
            stale.disposition,
            Some(DeliverySemantics::not_delivered_safe())
        );

        let preempted = IpcError::human_preempted(Epoch(4));
        assert_eq!(
            preempted.disposition,
            Some(DeliverySemantics::not_delivered_safe())
        );

        let lease_err = IpcError::lease_held_by_other(Epoch(4));
        assert_eq!(
            lease_err.disposition,
            Some(DeliverySemantics {
                delivery: DeliveryDisposition::Unknown,
                retry: RetryDisposition::Unknown,
            })
        );

        let timeout = IpcError::new(ErrorSymbol::ErrSettleTimeout, "timed out")
            .with_disposition(DeliverySemantics::delivery_uncertain_unsafe());
        assert_eq!(
            timeout.disposition,
            Some(DeliverySemantics::delivery_uncertain_unsafe())
        );

        let json = serde_json::to_value(&timeout).unwrap();
        assert_eq!(json["disposition"]["delivery"], "delivery_uncertain");
        assert_eq!(json["disposition"]["retry"], "unsafe");
    }
}
