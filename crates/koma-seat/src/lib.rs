#![forbid(unsafe_code)]

//! Seat multiplexing (`seat0` + per-workspace `vSeat`) and 3-tier Zero-IME text injection (`Tier 2`).

use std::collections::BTreeSet;

use koma_proto::{IpcError, SeatId, TextInjectStrategy, WindowId};

/// Tracks virtual modifiers and mouse buttons held on an isolated `vSeat` so `Mod + G` preemption
/// or coroutine cancellation can synthesize symmetric release events in `< 1 ms`.
#[derive(Debug, Clone)]
pub struct VirtualSeatLedger {
    pub id: SeatId,
    held_keysyms: BTreeSet<String>,
    held_buttons: BTreeSet<u32>,
}

impl VirtualSeatLedger {
    #[must_use]
    pub fn new(id: SeatId) -> Self {
        Self {
            id,
            held_keysyms: BTreeSet::new(),
            held_buttons: BTreeSet::new(),
        }
    }

    pub fn record_key_down(&mut self, key: impl Into<String>) {
        self.held_keysyms.insert(key.into());
    }

    pub fn record_key_up(&mut self, key: &str) {
        self.held_keysyms.remove(key);
    }

    pub fn record_button_down(&mut self, button_code: u32) {
        self.held_buttons.insert(button_code);
    }

    pub fn record_button_up(&mut self, button_code: u32) {
        self.held_buttons.remove(&button_code);
    }

    /// Atomically drains all held virtual keys and buttons for immediate release synthesis.
    pub fn flush_all_held(&mut self) -> (Vec<String>, Vec<u32>) {
        let keys = self.held_keysyms.iter().cloned().collect();
        let btns = self.held_buttons.iter().copied().collect();
        self.held_keysyms.clear();
        self.held_buttons.clear();
        (keys, btns)
    }
}

/// 3-Tier Zero-IME Text Injection contract (`ARCHITECTURE.md` §5.3).
pub trait ZeroImeTextInjector {
    fn supports_text_input_v3(&self, win_id: WindowId) -> bool;
    fn commit_text_input_v3(&mut self, win_id: WindowId, utf8: &str) -> Result<(), IpcError>;
    fn paste_isolated_vseat_clipboard(
        &mut self,
        seat_id: &SeatId,
        utf8: &str,
    ) -> Result<(), IpcError>;
    fn type_ephemeral_xkb_keymap(&mut self, seat_id: &SeatId, utf8: &str) -> Result<(), IpcError>;

    fn inject_cascade(
        &mut self,
        seat_id: &SeatId,
        win_id: WindowId,
        utf8: &str,
        strategy: TextInjectStrategy,
    ) -> Result<(), IpcError> {
        match strategy {
            TextInjectStrategy::TextInputV3 => self.commit_text_input_v3(win_id, utf8),
            TextInjectStrategy::Clipboard => self.paste_isolated_vseat_clipboard(seat_id, utf8),
            TextInjectStrategy::XkbKeymap => self.type_ephemeral_xkb_keymap(seat_id, utf8),
            TextInjectStrategy::Auto => {
                if self.supports_text_input_v3(win_id) {
                    self.commit_text_input_v3(win_id, utf8)
                } else {
                    self.paste_isolated_vseat_clipboard(seat_id, utf8)
                        .or_else(|_| self.type_ephemeral_xkb_keymap(seat_id, utf8))
                }
            }
        }
    }
}
