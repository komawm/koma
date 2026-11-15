use std::collections::BTreeMap;

use koma_model::AnchorPoint;
use koma_proto::{Epoch, IpcError, MacroId, TextInjectStrategy, WorkspaceId};

pub const CONFIG_FUEL_BUDGET: u64 = 200_000;
pub const LAYOUT_HOOK_FUEL_BUDGET: u64 = 50_000;
pub const MACRO_SLICE_FUEL_BUDGET: u64 = 100_000;
pub const DEFAULT_HEAP_LIMIT_BYTES: usize = 4 * 1024 * 1024;

/// Host-side operations that suspend a stackless `Phodopus` macro coroutine (`ARCHITECTURE.md` §2.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingHostOp {
    SleepTimer {
        deadline_ms: u64,
    },
    WaitDamageSettle {
        ws_id: WorkspaceId,
        quiet_ms: u64,
        timeout_at_ms: u64,
    },
    PointerTrajectory {
        ws_id: WorkspaceId,
        started_at_ms: u64,
        finish_at_ms: u64,
        settle_driven: bool,
    },
}

/// Host capability interface implemented by Ring 0 (`koma` + `koma-seat`) for `MacroContext`.
pub trait HostBridge {
    fn current_epoch(&self, ws_id: &WorkspaceId) -> Option<Epoch>;
    /// Returns `Some(true)` once `ws_id`'s active pointer trajectory (e.g. damped Hooke spring) has converged.
    fn pointer_trajectory_settled(&self, _ws_id: &WorkspaceId) -> Option<bool> {
        None
    }
    fn focused_app_id(&self, ws_id: &WorkspaceId) -> Option<String>;
    fn tap_key_chord(&mut self, ws_id: &WorkspaceId, chord: &str) -> Result<(), IpcError>;
    fn move_pointer(&mut self, ws_id: &WorkspaceId, target: &AnchorPoint) -> Result<u64, IpcError>;
    fn inject_text(
        &mut self,
        ws_id: &WorkspaceId,
        utf8: &str,
        strategy: TextInjectStrategy,
    ) -> Result<(), IpcError>;
    fn paste_mime(
        &mut self,
        ws_id: &WorkspaceId,
        mime_type: &str,
        payload: &[u8],
    ) -> Result<(), IpcError>;
}

#[derive(Debug, Clone)]
pub struct ParkedMacroTask {
    pub raw_host_op_handle: u64,
    pub macro_id: MacroId,
    pub ws_id: WorkspaceId,
    pub expected_epoch: Epoch,
    pub op: PendingHostOp,
}

/// Tracks parked `Phodopus` `HostOpHandle` coroutines on `calloop` and enforces `Epoch` fencing.
#[derive(Debug, Default)]
pub struct TrampolineScheduler {
    parked: BTreeMap<u64, ParkedMacroTask>,
}

impl TrampolineScheduler {
    /// Returns the earliest monotonic wakeup deadline (in ms) across all parked coroutines,
    /// allowing `calloop` to arm a single exact one-shot `Timer` with zero idle polling.
    #[must_use]
    pub fn earliest_deadline_ms(&self) -> Option<u64> {
        self.parked
            .values()
            .map(|task| match &task.op {
                PendingHostOp::SleepTimer { deadline_ms } => *deadline_ms,
                PendingHostOp::PointerTrajectory { finish_at_ms, .. } => *finish_at_ms,
                PendingHostOp::WaitDamageSettle { timeout_at_ms, .. } => *timeout_at_ms,
            })
            .min()
    }

    /// Returns the tightest `quiet_ms` requested by any coroutine waiting for `ws_id` to settle.
    #[must_use]
    pub fn pending_settle_quiet_ms(&self, ws_id: &WorkspaceId) -> Option<u64> {
        self.parked
            .values()
            .filter_map(|task| match &task.op {
                PendingHostOp::WaitDamageSettle {
                    ws_id: target_ws,
                    quiet_ms,
                    ..
                } if target_ws == ws_id => Some(*quiet_ms),
                _ => None,
            })
            .min()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.parked.is_empty()
    }

    pub fn register_suspended(&mut self, task: ParkedMacroTask) {
        self.parked.insert(task.raw_host_op_handle, task);
    }

    /// Cancels all parked macro coroutines targeting `ws_id` (invoked on `Mod + G` preemption).
    /// Returns the raw `HostOpHandle` IDs to pass into `Executor::cancel_host_op`.
    pub fn cancel_workspace_tasks(&mut self, ws_id: &WorkspaceId) -> Vec<u64> {
        let handles: Vec<u64> = self
            .parked
            .iter()
            .filter(|(_, t)| &t.ws_id == ws_id)
            .map(|(&h, _)| h)
            .collect();
        for h in &handles {
            self.parked.remove(h);
        }
        handles
    }

    /// Collects ready `HostOpHandle` IDs whose timer or damage-settle condition is met,
    /// splitting them into `(valid_resumes, stale_epoch_cancels)`.
    pub fn poll_ready(
        &mut self,
        now_ms: u64,
        settled_workspaces: &[WorkspaceId],
        bridge: &dyn HostBridge,
    ) -> (Vec<ParkedMacroTask>, Vec<ParkedMacroTask>) {
        let mut ready_handles = Vec::new();
        for (&handle, task) in &self.parked {
            let is_ready = match &task.op {
                PendingHostOp::SleepTimer { deadline_ms } => now_ms >= *deadline_ms,
                PendingHostOp::PointerTrajectory {
                    ws_id,
                    started_at_ms,
                    finish_at_ms,
                    settle_driven,
                } => {
                    now_ms >= *finish_at_ms
                        || (*settle_driven
                            && now_ms > *started_at_ms
                            && bridge.pointer_trajectory_settled(ws_id) == Some(true))
                }
                PendingHostOp::WaitDamageSettle {
                    ws_id,
                    timeout_at_ms,
                    ..
                } => now_ms >= *timeout_at_ms || settled_workspaces.contains(ws_id),
            };
            if is_ready {
                ready_handles.push(handle);
            }
        }

        let mut valid = Vec::new();
        let mut stale = Vec::new();
        for h in ready_handles {
            if let Some(task) = self.parked.remove(&h) {
                if bridge.current_epoch(&task.ws_id) == Some(task.expected_epoch) {
                    valid.push(task);
                } else {
                    stale.push(task);
                }
            }
        }
        (valid, stale)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DummyBridge(Epoch);
    impl HostBridge for DummyBridge {
        fn current_epoch(&self, _ws_id: &WorkspaceId) -> Option<Epoch> {
            Some(self.0)
        }
        fn focused_app_id(&self, _ws_id: &WorkspaceId) -> Option<String> {
            Some("wps".into())
        }
        fn tap_key_chord(&mut self, _ws_id: &WorkspaceId, _chord: &str) -> Result<(), IpcError> {
            Ok(())
        }
        fn move_pointer(
            &mut self,
            _ws_id: &WorkspaceId,
            _target: &AnchorPoint,
        ) -> Result<u64, IpcError> {
            Ok(0)
        }
        fn inject_text(
            &mut self,
            _ws_id: &WorkspaceId,
            _utf8: &str,
            _strategy: TextInjectStrategy,
        ) -> Result<(), IpcError> {
            Ok(())
        }
        fn paste_mime(
            &mut self,
            _ws_id: &WorkspaceId,
            _mime_type: &str,
            _payload: &[u8],
        ) -> Result<(), IpcError> {
            Ok(())
        }
    }

    #[test]
    fn aborts_parked_coroutine_when_epoch_changes_during_sleep() {
        let mut sched = TrampolineScheduler::default();
        sched.register_suspended(ParkedMacroTask {
            raw_host_op_handle: 42,
            macro_id: MacroId::from("wps.insert_styled_table"),
            ws_id: WorkspaceId::from("ws_01"),
            expected_epoch: Epoch(4),
            op: PendingHostOp::SleepTimer { deadline_ms: 500 },
        });

        // Epoch advanced to 5 while coroutine was sleeping
        let bridge = DummyBridge(Epoch(5));
        let (valid, stale) = sched.poll_ready(550, &[], &bridge);
        assert!(valid.is_empty());
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].raw_host_op_handle, 42);
    }
}
