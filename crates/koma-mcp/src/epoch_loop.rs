//! Stateful per-workspace `Epoch` observation ledger & deterministic `ERR_STALE_EPOCH` /
//! `ERR_HUMAN_PREEMPTED` closed-loop recovery (`docs/IPC_SPEC.md` §4.2).
//!
//! Enforces Koma's core TOCTOU safety invariant:
//! 1. An agent can NEVER execute a mutating tool on a workspace whose UI state has not been
//!    observed (`koma_capture_frame` or `koma_scene_inventory`) at the current `Epoch`.
//! 2. When `koma-agent.sock` rejects an action with `4090 ERR_STALE_EPOCH`, `koma-mcp` NEVER
//!    blindly retries the stale click/keystroke/macro with the new epoch—instead, it immediately
//!    performs a zero-copy `capture.frame` re-observation, records the new `Epoch`, and bundles
//!    the fresh PNG screenshot + updated `new_observed_epoch` directly in the error response so
//!    the VLM/LLM recomputes its decision in a single turn!

use std::collections::BTreeMap;

use koma_proto::{Epoch, ErrorSymbol, IpcError, WorkspaceId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "validity", rename_all = "snake_case")]
pub enum ObservationStatus {
    /// Agent has observed a snapshot or inventory of this workspace at `epoch`.
    Fresh { epoch: Epoch },
    /// Workspace epoch changed (e.g., lease boundary or previous stale error); must re-observe.
    StaleRequiresReobserve {
        last_known_server_epoch: Option<Epoch>,
    },
    /// Human pressed `Mod + G` (`Takeover`); agent writes are paused until `workspace.resumed`.
    HumanPreempted { takeover_epoch: Option<Epoch> },
}

#[derive(Debug, Default, Clone)]
pub struct EpochObservationTracker {
    records: BTreeMap<WorkspaceId, ObservationStatus>,
}

impl EpochObservationTracker {
    /// Records a verified UI observation (`capture.frame` or `scene.inventory`) for `ws_id`.
    pub fn record_observed(&mut self, ws_id: WorkspaceId, epoch: Epoch, is_human_preempted: bool) {
        let status = if is_human_preempted {
            ObservationStatus::HumanPreempted {
                takeover_epoch: Some(epoch),
            }
        } else {
            ObservationStatus::Fresh { epoch }
        };
        self.records.insert(ws_id, status);
    }

    /// Updates the local ledger when a server mutation succeeds and returns an updated `Epoch`
    /// (for example, `auto_acquire` advancing `Unheld(1) -> AgentExclusive(2)`).
    pub fn record_verified_mutation_epoch(&mut self, ws_id: &WorkspaceId, new_epoch: Epoch) {
        self.records
            .insert(ws_id.clone(), ObservationStatus::Fresh { epoch: new_epoch });
    }

    /// Resolves which `expected_epoch` to send for a mutating tool call, rejecting un-observed
    /// or locally known stale/preempted states before even hitting the wire!
    pub fn resolve_mutation_epoch(
        &self,
        ws_id: &WorkspaceId,
        explicit_expected: Option<Epoch>,
    ) -> Result<Epoch, IpcError> {
        let Some(status) = self.records.get(ws_id) else {
            return Err(IpcError::new(
                ErrorSymbol::ErrStaleEpoch,
                format!(
                    "workspace `{ws_id}` has not been observed yet in this session; call `koma_capture_frame` or `koma_scene_inventory` first"
                ),
            ));
        };

        match *status {
            ObservationStatus::Fresh { epoch: observed } => {
                if let Some(exp) = explicit_expected {
                    if exp != observed {
                        return Err(IpcError::stale_epoch(exp, observed));
                    }
                }
                Ok(observed)
            }
            ObservationStatus::StaleRequiresReobserve {
                last_known_server_epoch,
            } => Err(IpcError {
                code: ErrorSymbol::ErrStaleEpoch.code(),
                symbol: ErrorSymbol::ErrStaleEpoch,
                message: format!(
                    "workspace `{ws_id}` changed epoch and requires fresh observation before acting"
                ),
                current_epoch: last_known_server_epoch.map(Epoch::get),
                retryable: Some(false),
            }),
            ObservationStatus::HumanPreempted { takeover_epoch } => Err(IpcError::human_preempted(
                takeover_epoch.unwrap_or_default(),
            )),
        }
    }

    /// Marks local state upon receiving an `IpcError` from `koma-agent.sock` and indicates whether
    /// `koma-mcp` should immediately attach a fresh zero-copy `capture.frame` re-observation!
    pub fn on_mutation_ipc_error(
        &mut self,
        ws_id: &WorkspaceId,
        err: &IpcError,
    ) -> RecoveryDirective {
        match err.symbol {
            ErrorSymbol::ErrStaleEpoch => {
                let server_ep = err.current_epoch.map(Epoch);
                self.records.insert(
                    ws_id.clone(),
                    ObservationStatus::StaleRequiresReobserve {
                        last_known_server_epoch: server_ep,
                    },
                );
                RecoveryDirective::AutoCaptureFreshSnapshot {
                    previous_stale_epoch: server_ep,
                }
            }
            ErrorSymbol::ErrHumanPreempted => {
                let server_ep = err.current_epoch.map(Epoch);
                self.records.insert(
                    ws_id.clone(),
                    ObservationStatus::HumanPreempted {
                        takeover_epoch: server_ep,
                    },
                );
                RecoveryDirective::HumanTakeoverPaused {
                    current_epoch: server_ep,
                }
            }
            ErrorSymbol::ErrProtectedSurface => RecoveryDirective::ProtectedSurfaceBlocked,
            _ => RecoveryDirective::PassThroughError,
        }
    }

    #[must_use]
    pub fn status(&self, ws_id: &WorkspaceId) -> Option<&ObservationStatus> {
        self.records.get(ws_id)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryDirective {
    /// Immediately run `capture.frame`, update `EpochObservationTracker` to `Fresh(new_epoch)`,
    /// and bundle the fresh screenshot + new epoch in the error result (NEVER auto-retrying the stale mutation).
    AutoCaptureFreshSnapshot { previous_stale_epoch: Option<Epoch> },
    /// Workspace is under human `Mod + G` takeover; report pause cleanly without mutating.
    HumanTakeoverPaused { current_epoch: Option<Epoch> },
    /// Target is a credential vault / Polkit prompt / lockscreen; prompt human verification.
    ProtectedSurfaceBlocked,
    /// Standard validation/parameter error.
    PassThroughError,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enforces_observation_before_write_and_triggers_auto_reobserve_on_stale_epoch() {
        let mut tracker = EpochObservationTracker::default();
        let ws = WorkspaceId::from("ws_01");

        // 1. Rejects write prior to initial observation
        let unobserved_err = tracker.resolve_mutation_epoch(&ws, None).unwrap_err();
        assert_eq!(unobserved_err.symbol, ErrorSymbol::ErrStaleEpoch);

        // 2. Capture frame observes Epoch(4) -> allows mutation at Epoch(4)
        tracker.record_observed(ws.clone(), Epoch(4), false);
        assert_eq!(tracker.resolve_mutation_epoch(&ws, None).unwrap(), Epoch(4));

        // 3. Server rejects in-flight click with 4090 ERR_STALE_EPOCH (current_epoch = 5)
        let server_err = IpcError::stale_epoch(Epoch(4), Epoch(5));
        let directive = tracker.on_mutation_ipc_error(&ws, &server_err);
        assert_eq!(
            directive,
            RecoveryDirective::AutoCaptureFreshSnapshot {
                previous_stale_epoch: Some(Epoch(5))
            }
        );

        // Until re-observation completes, blind retry is blocked locally!
        assert!(tracker.resolve_mutation_epoch(&ws, Some(Epoch(5))).is_err());

        // 4. Auto-reobservation capture completes at Epoch(5) -> Fresh(5) restored!
        tracker.record_observed(ws.clone(), Epoch(5), false);
        assert_eq!(
            tracker.resolve_mutation_epoch(&ws, Some(Epoch(5))).unwrap(),
            Epoch(5)
        );
    }
}
