use std::fmt;

use serde::{Deserialize, Serialize};

/// Monotonically increasing 64-bit workspace concurrency fence (`IPC_SPEC.md` §4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Epoch(pub u64);

impl Epoch {
    /// Every newly created workspace starts at `Epoch(1)`.
    pub const INITIAL: Self = Self(1);

    /// Atomically advances the epoch fence on lease or human-takeover transitions.
    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl Default for Epoch {
    fn default() -> Self {
        Self::INITIAL
    }
}

impl fmt::Display for Epoch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

macro_rules! string_id {
    ($name:ident, $doc:expr) => {
        #[doc = $doc]
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            #[must_use]
            pub fn new(id: impl Into<String>) -> Self {
                Self(id.into())
            }

            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }
    };
}

string_id!(
    WorkspaceId,
    "Stable logical workspace identifier (e.g., `ws_01`)."
);
string_id!(
    OutputId,
    "Physical DRM connector or virtual output identifier (e.g., `eDP-1`)."
);
string_id!(
    SessionId,
    "Authenticated IPC client session identifier (e.g., `sess_01jd8x9`)."
);
string_id!(
    SeatId,
    "Physical (`seat0`) or per-workspace virtual seat (`vseat_01`) identifier."
);
string_id!(
    MacroId,
    "Registered Phodopus macro identifier (e.g., `wps.insert_styled_table`)."
);

/// Generation-tagged window identifier preventing ABA mis-targeting when windows close and spawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct WindowId {
    pub slot: u64,
    pub generation: u32,
}

impl WindowId {
    #[must_use]
    pub const fn new(slot: u64, generation: u32) -> Self {
        Self { slot, generation }
    }
}

impl fmt::Display for WindowId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "win_{:x}_g{}", self.slot, self.generation)
    }
}
