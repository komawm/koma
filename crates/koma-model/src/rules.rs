use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::geometry::Anchor9;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenMode {
    Tile,
    Float,
    Fullscreen,
    Maximized,
    DeckTop,
    StashBackground,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockOutPolicy {
    None,
    AgentCapture,
    Screencast,
    Screencopy,
    All,
}

impl BlockOutPolicy {
    #[must_use]
    pub const fn blocks_agent_capture(self) -> bool {
        matches!(self, Self::AgentCapture | Self::All)
    }
}

#[derive(Debug, Clone)]
pub struct WindowMatchSpec {
    pub app_id: Option<Regex>,
    pub title: Option<Regex>,
    pub is_floating: Option<bool>,
    pub is_focused: Option<bool>,
    pub is_xwayland: Option<bool>,
    pub is_modal_dialog: Option<bool>,
    pub is_screencast_target: Option<bool>,
    pub is_agent_workspace: Option<bool>,
}

impl WindowMatchSpec {
    #[must_use]
    pub fn matches(&self, ctx: &WindowMatchContext<'_>) -> bool {
        self.app_id
            .as_ref()
            .is_none_or(|re| re.is_match(ctx.app_id))
            && self.title.as_ref().is_none_or(|re| re.is_match(ctx.title))
            && self.is_floating.is_none_or(|v| v == ctx.is_floating)
            && self.is_focused.is_none_or(|v| v == ctx.is_focused)
            && self.is_xwayland.is_none_or(|v| v == ctx.is_xwayland)
            && self
                .is_modal_dialog
                .is_none_or(|v| v == ctx.is_modal_dialog)
            && self
                .is_screencast_target
                .is_none_or(|v| v == ctx.is_screencast_target)
            && self
                .is_agent_workspace
                .is_none_or(|v| v == ctx.is_agent_workspace)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct WindowMatchContext<'a> {
    pub app_id: &'a str,
    pub title: &'a str,
    pub is_floating: bool,
    pub is_focused: bool,
    pub is_xwayland: bool,
    pub is_modal_dialog: bool,
    pub is_screencast_target: bool,
    pub is_agent_workspace: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InitialWindowRules {
    pub open_on_workspace: Option<String>,
    pub open_mode: Option<OpenMode>,
    pub open_focused: Option<bool>,
    pub default_floating_pos: Option<(Anchor9, i32, i32)>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DynamicWindowRules {
    pub opacity: Option<f32>,
    pub corner_radius: Option<u16>,
    pub clip_to_geometry: Option<bool>,
    pub focusable: Option<bool>,
    pub suppress_maximize: Option<bool>,
    pub block_out_from: Option<BlockOutPolicy>,
}

#[derive(Debug, Clone)]
pub struct WindowRuleEntry {
    pub id: u64,
    pub name: Option<String>,
    pub enabled: bool,
    pub matches: Vec<WindowMatchSpec>,
    pub excludes: Vec<WindowMatchSpec>,
    pub initial: InitialWindowRules,
    pub dynamic: DynamicWindowRules,
}

impl WindowRuleEntry {
    #[must_use]
    pub fn applies_to(&self, ctx: &WindowMatchContext<'_>) -> bool {
        if !self.enabled {
            return false;
        }
        let matched = self.matches.is_empty() || self.matches.iter().any(|m| m.matches(ctx));
        if !matched {
            return false;
        }
        !self.excludes.iter().any(|ex| ex.matches(ctx))
    }
}

/// Evaluates two-phase window rules and enforces the Protected Surface Blackout Invariant.
#[derive(Debug, Default)]
pub struct WindowRuleEngine {
    rules: Vec<WindowRuleEntry>,
    protected: ProtectedSurfaceClassifier,
}

impl WindowRuleEngine {
    #[must_use]
    pub fn new() -> Self {
        Self {
            rules: Vec::new(),
            protected: ProtectedSurfaceClassifier::default(),
        }
    }

    /// Atomically replaces user-configured window rules during hot-reload while keeping the
    /// built-in `ProtectedSurfaceClassifier` (`Polkit` / `KeePassXC` / `Bitwarden` / `1Password`) intact.
    pub fn replace_rules(&mut self, rules: Vec<WindowRuleEntry>) {
        self.rules = rules;
    }

    #[must_use]
    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }

    pub fn push_rule(&mut self, rule: WindowRuleEntry) {
        self.rules.push(rule);
    }

    pub fn set_rule_enabled(&mut self, id: u64, enabled: bool) -> bool {
        if let Some(r) = self.rules.iter_mut().find(|r| r.id == id) {
            r.enabled = enabled;
            return true;
        }
        false
    }

    #[must_use]
    pub fn resolve_dynamic(&self, ctx: &WindowMatchContext<'_>) -> DynamicWindowRules {
        let mut out = DynamicWindowRules::default();
        for rule in &self.rules {
            if !rule.applies_to(ctx) {
                continue;
            }
            if let Some(v) = rule.dynamic.opacity {
                out.opacity = Some(v);
            }
            if let Some(v) = rule.dynamic.corner_radius {
                out.corner_radius = Some(v);
            }
            if let Some(v) = rule.dynamic.clip_to_geometry {
                out.clip_to_geometry = Some(v);
            }
            if let Some(v) = rule.dynamic.focusable {
                out.focusable = Some(v);
            }
            if let Some(v) = rule.dynamic.suppress_maximize {
                out.suppress_maximize = Some(v);
            }
            if let Some(v) = rule.dynamic.block_out_from {
                out.block_out_from = Some(v);
            }
        }
        // Hard invariant (`IPC_SPEC.md` §3.2): Polkit, pinentry, and credential vaults
        // always force BlockOutPolicy::All even if user config omits a rule.
        if self.protected.is_protected(ctx.app_id, ctx.title) {
            out.block_out_from = Some(BlockOutPolicy::All);
        }
        out
    }
}

#[derive(Debug, Clone)]
pub struct ProtectedSurfaceClassifier {
    denylist_app_id: Regex,
}

impl Default for ProtectedSurfaceClassifier {
    fn default() -> Self {
        Self {
            denylist_app_id: Regex::new(
                r"(?i)^(polkit-gnome-authentication-agent.*|hyprpolkitagent|pinentry.*|org\.keepassxc\.KeePassXC|.*bitwarden.*|1password)$",
            )
            .expect("valid built-in protected surface regex"),
        }
    }
}

impl ProtectedSurfaceClassifier {
    #[must_use]
    pub fn is_protected(&self, app_id: &str, _title: &str) -> bool {
        self.denylist_app_id.is_match(app_id)
    }
}
