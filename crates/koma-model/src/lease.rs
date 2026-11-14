use koma_proto::{Epoch, ErrorSymbol, IpcError, PrivilegeLevel, SessionId};
use serde::{Deserialize, Serialize};

/// Three-state preemptive concurrency lease (`ARCHITECTURE.md` §4.2 & `IPC_SPEC.md` §4.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkspaceLeaseState {
    Unheld,
    AgentExclusive {
        holder: SessionId,
        expires_at_ms: u64,
    },
    HumanPreempted {
        interrupted_holder: Option<SessionId>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreemptEffects {
    pub new_epoch: Epoch,
    pub entered_human_takeover: bool,
    pub flush_virtual_seat: bool,
    pub cancel_lua_coroutines: bool,
}

#[derive(Debug, Clone)]
pub struct WorkspaceLease {
    epoch: Epoch,
    state: WorkspaceLeaseState,
}

impl Default for WorkspaceLease {
    fn default() -> Self {
        Self {
            epoch: Epoch::INITIAL,
            state: WorkspaceLeaseState::Unheld,
        }
    }
}

impl WorkspaceLease {
    #[must_use]
    pub const fn epoch(&self) -> Epoch {
        self.epoch
    }

    #[must_use]
    pub const fn state(&self) -> &WorkspaceLeaseState {
        &self.state
    }

    #[must_use]
    pub const fn is_human_preempted(&self) -> bool {
        matches!(self.state, WorkspaceLeaseState::HumanPreempted { .. })
    }

    /// Acquires `AgentExclusive` write ownership if currently `Unheld` (or expired), advancing `Epoch`.
    pub fn acquire(
        &mut self,
        caller: SessionId,
        now_ms: u64,
        ttl_ms: u64,
    ) -> Result<Epoch, IpcError> {
        self.expire_if_needed(now_ms);
        match &self.state {
            WorkspaceLeaseState::Unheld => {
                self.epoch = self.epoch.next();
                self.state = WorkspaceLeaseState::AgentExclusive {
                    holder: caller,
                    expires_at_ms: now_ms.saturating_add(ttl_ms),
                };
                Ok(self.epoch)
            }
            WorkspaceLeaseState::AgentExclusive { holder, .. } if *holder == caller => {
                self.state = WorkspaceLeaseState::AgentExclusive {
                    holder: caller,
                    expires_at_ms: now_ms.saturating_add(ttl_ms),
                };
                Ok(self.epoch)
            }
            WorkspaceLeaseState::AgentExclusive { .. } => {
                Err(IpcError::lease_held_by_other(self.epoch))
            }
            WorkspaceLeaseState::HumanPreempted { .. } => {
                Err(IpcError::human_preempted(self.epoch))
            }
        }
    }

    /// Releases an active `AgentExclusive` lease, advancing `Epoch`.
    pub fn release(
        &mut self,
        caller: &SessionId,
        expected_epoch: Epoch,
    ) -> Result<Epoch, IpcError> {
        if expected_epoch != self.epoch {
            return Err(IpcError::stale_epoch(expected_epoch, self.epoch));
        }
        match &self.state {
            WorkspaceLeaseState::AgentExclusive { holder, .. } if holder == caller => {
                self.epoch = self.epoch.next();
                self.state = WorkspaceLeaseState::Unheld;
                Ok(self.epoch)
            }
            WorkspaceLeaseState::HumanPreempted { .. } => {
                Err(IpcError::human_preempted(self.epoch))
            }
            _ => Err(IpcError::new(
                ErrorSymbol::ErrCapabilityDenied,
                "caller does not hold active lease on workspace",
            )),
        }
    }

    /// Triggered by Ring 0 when the human presses `Mod + G` (`Takeover` toggle).
    pub fn human_toggle_takeover(&mut self, resume_ttl_ms: u64, now_ms: u64) -> PreemptEffects {
        self.epoch = self.epoch.next();
        match &self.state {
            WorkspaceLeaseState::AgentExclusive { holder, .. } => {
                let prev = Some(holder.clone());
                self.state = WorkspaceLeaseState::HumanPreempted {
                    interrupted_holder: prev,
                };
                PreemptEffects {
                    new_epoch: self.epoch,
                    entered_human_takeover: true,
                    flush_virtual_seat: true,
                    cancel_lua_coroutines: true,
                }
            }
            WorkspaceLeaseState::Unheld => {
                self.state = WorkspaceLeaseState::HumanPreempted {
                    interrupted_holder: None,
                };
                PreemptEffects {
                    new_epoch: self.epoch,
                    entered_human_takeover: true,
                    flush_virtual_seat: true,
                    cancel_lua_coroutines: true,
                }
            }
            WorkspaceLeaseState::HumanPreempted { interrupted_holder } => {
                self.state = match interrupted_holder.clone() {
                    Some(holder) => WorkspaceLeaseState::AgentExclusive {
                        holder,
                        expires_at_ms: now_ms.saturating_add(resume_ttl_ms),
                    },
                    None => WorkspaceLeaseState::Unheld,
                };
                PreemptEffects {
                    new_epoch: self.epoch,
                    entered_human_takeover: false,
                    flush_virtual_seat: false,
                    cancel_lua_coroutines: false,
                }
            }
        }
    }

    /// Deterministic Four-Step Mutating RPC Gate (`IPC_SPEC.md` §4.2).
    pub fn validate_mutation_gate(
        &mut self,
        caller: &SessionId,
        granted: PrivilegeLevel,
        expected_epoch: Epoch,
        auto_acquire_ttl_ms: Option<u64>,
        now_ms: u64,
    ) -> Result<Epoch, IpcError> {
        self.expire_if_needed(now_ms);

        // Step 1: Capability Check
        if granted < PrivilegeLevel::Control {
            return Err(IpcError::new(
                ErrorSymbol::ErrCapabilityDenied,
                "mutating workspace RPC requires Control or Owner grant",
            ));
        }

        // Step 2: Human Preemption Check
        if self.is_human_preempted() {
            return Err(IpcError::human_preempted(self.epoch));
        }

        // Step 3: Writer Mutex Check
        match &self.state {
            WorkspaceLeaseState::AgentExclusive { holder, .. } if holder == caller => {}
            WorkspaceLeaseState::AgentExclusive { .. } => {
                return Err(IpcError::lease_held_by_other(self.epoch));
            }
            WorkspaceLeaseState::Unheld => {
                if let Some(ttl_ms) = auto_acquire_ttl_ms {
                    if expected_epoch != self.epoch {
                        return Err(IpcError::stale_epoch(expected_epoch, self.epoch));
                    }
                    return self.acquire(caller.clone(), now_ms, ttl_ms);
                }
                return Err(IpcError::new(
                    ErrorSymbol::ErrCapabilityDenied,
                    "workspace lease is Unheld; call lease.acquire first or pass auto_acquire",
                ));
            }
            WorkspaceLeaseState::HumanPreempted { .. } => unreachable!(),
        }

        // Step 4: Epoch Fence Check
        if expected_epoch != self.epoch {
            return Err(IpcError::stale_epoch(expected_epoch, self.epoch));
        }

        Ok(self.epoch)
    }

    fn expire_if_needed(&mut self, now_ms: u64) {
        if let WorkspaceLeaseState::AgentExclusive { expires_at_ms, .. } = self.state {
            if now_ms >= expires_at_ms {
                self.epoch = self.epoch.next();
                self.state = WorkspaceLeaseState::Unheld;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mod_g_preempt_fences_stale_agent_clicks() {
        let mut lease = WorkspaceLease::default();
        let agent = SessionId::from("sess_agent_1");
        let ep = lease.acquire(agent.clone(), 1000, 60_000).unwrap();
        assert_eq!(ep, Epoch(2));

        // Human presses Mod+G to take over
        let fx = lease.human_toggle_takeover(60_000, 1500);
        assert!(fx.entered_human_takeover);
        assert!(fx.flush_virtual_seat);
        assert_eq!(fx.new_epoch, Epoch(3));

        // In-flight agent click carrying Epoch(2) is deterministically rejected
        let err = lease
            .validate_mutation_gate(&agent, PrivilegeLevel::Control, Epoch(2), None, 1600)
            .unwrap_err();
        assert_eq!(err.symbol, ErrorSymbol::ErrHumanPreempted);

        // Human releases Mod+G; Epoch advances to 4, still rejecting stale Epoch(2)
        let fx2 = lease.human_toggle_takeover(60_000, 2000);
        assert!(!fx2.entered_human_takeover);
        assert_eq!(fx2.new_epoch, Epoch(4));

        let err2 = lease
            .validate_mutation_gate(&agent, PrivilegeLevel::Control, Epoch(2), None, 2100)
            .unwrap_err();
        assert_eq!(err2.symbol, ErrorSymbol::ErrStaleEpoch);
        assert_eq!(err2.current_epoch, Some(4));
    }
}
