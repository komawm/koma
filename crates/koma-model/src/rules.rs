use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::{geometry::Anchor9, scrolling::PresetColumnWidth};

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

#[derive(Debug, Clone, Default)]
pub struct WindowMatchSpec {
    pub app_id: Option<Regex>,
    pub title: Option<Regex>,
    pub workspace: Option<String>,
    pub is_floating: Option<bool>,
    pub is_focused: Option<bool>,
    pub is_xwayland: Option<bool>,
    pub is_modal_dialog: Option<bool>,
    pub is_screencast_target: Option<bool>,
    pub is_agent_workspace: Option<bool>,
    pub at_startup: Option<bool>,
}

impl WindowMatchSpec {
    #[must_use]
    pub fn matches(&self, ctx: &WindowMatchContext<'_>) -> bool {
        self.app_id
            .as_ref()
            .is_none_or(|re| re.is_match(ctx.app_id))
            && self.title.as_ref().is_none_or(|re| re.is_match(ctx.title))
            && self
                .workspace
                .as_deref()
                .is_none_or(|ws| ctx.workspace.is_some_and(|cur| cur == ws))
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
            && self.at_startup.is_none_or(|v| v == ctx.at_startup)
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct WindowMatchContext<'a> {
    pub app_id: &'a str,
    pub title: &'a str,
    pub workspace: Option<&'a str>,
    pub is_floating: bool,
    pub is_focused: bool,
    pub is_xwayland: bool,
    pub is_modal_dialog: bool,
    pub is_screencast_target: bool,
    pub is_agent_workspace: bool,
    pub at_startup: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InitialWindowRules {
    pub open_on_output: Option<String>,
    pub open_on_workspace: Option<String>,
    pub open_mode: Option<OpenMode>,
    pub open_focused: Option<bool>,
    pub default_width: Option<PresetColumnWidth>,
    pub default_height: Option<PresetColumnWidth>,
    pub max_width: Option<PresetColumnWidth>,
    pub max_height: Option<PresetColumnWidth>,
    pub min_width: Option<PresetColumnWidth>,
    pub min_height: Option<PresetColumnWidth>,
    pub default_floating_pos: Option<(Anchor9, i32, i32)>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DynamicWindowRules {
    pub opacity: Option<f32>,
    pub inactive_opacity: Option<f32>,
    pub animation: Option<String>,
    pub immediate: Option<bool>,
    pub no_blur: Option<bool>,
    pub no_shadow: Option<bool>,
    pub border_color: Option<String>,
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
    pub fn resolve_initial(&self, ctx: &WindowMatchContext<'_>) -> InitialWindowRules {
        let mut out = InitialWindowRules::default();
        for rule in &self.rules {
            if !rule.applies_to(ctx) {
                continue;
            }
            if let Some(ref output) = rule.initial.open_on_output {
                out.open_on_output = Some(output.clone());
            }
            if let Some(ref ws) = rule.initial.open_on_workspace {
                out.open_on_workspace = Some(ws.clone());
            }
            if let Some(mode) = rule.initial.open_mode {
                out.open_mode = Some(mode);
            }
            if let Some(focused) = rule.initial.open_focused {
                out.open_focused = Some(focused);
            }
            if let Some(w) = rule.initial.default_width {
                out.default_width = Some(w);
            }
            if let Some(h) = rule.initial.default_height {
                out.default_height = Some(h);
            }
            if let Some(w) = rule.initial.max_width {
                out.max_width = Some(w);
            }
            if let Some(h) = rule.initial.max_height {
                out.max_height = Some(h);
            }
            if let Some(w) = rule.initial.min_width {
                out.min_width = Some(w);
            }
            if let Some(h) = rule.initial.min_height {
                out.min_height = Some(h);
            }
            if let Some(pos) = rule.initial.default_floating_pos {
                out.default_floating_pos = Some(pos);
            }
        }
        out
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
            if let Some(v) = rule.dynamic.inactive_opacity {
                out.inactive_opacity = Some(v);
            }
            if let Some(v) = rule.dynamic.animation.as_ref() {
                out.animation = Some(v.clone());
            }
            if let Some(v) = rule.dynamic.immediate {
                out.immediate = Some(v);
            }
            if let Some(v) = rule.dynamic.no_blur {
                out.no_blur = Some(v);
            }
            if let Some(v) = rule.dynamic.no_shadow {
                out.no_shadow = Some(v);
            }
            if let Some(v) = rule.dynamic.border_color.as_ref() {
                out.border_color = Some(v.clone());
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

/// Returns `true` when `app_id` belongs to a Waydroid container or Android window
/// (`Waydroid`, `waydroid.<android.package>`).
#[must_use]
pub fn is_waydroid_app_id(app_id: &str) -> bool {
    let normalized = app_id.trim().to_ascii_lowercase();
    normalized == "waydroid" || normalized.starts_with("waydroid.")
}

/// Returns `true` when `app_id` identifies an individual Waydroid multi-window Android app.
#[must_use]
pub fn is_waydroid_multiwindow_app_id(app_id: &str) -> bool {
    app_id.trim().to_ascii_lowercase().starts_with("waydroid.")
}

#[cfg(test)]
mod waydroid_rule_tests {
    use super::{is_waydroid_app_id, is_waydroid_multiwindow_app_id};

    #[test]
    fn identifies_waydroid_full_ui_and_multiwindow_app_ids() {
        assert!(is_waydroid_app_id("Waydroid"));
        assert!(is_waydroid_app_id("waydroid"));
        assert!(is_waydroid_app_id("waydroid.com.android.settings"));
        assert!(!is_waydroid_app_id("firefox"));
        assert!(!is_waydroid_app_id("waydroid-helper"));

        assert!(!is_waydroid_multiwindow_app_id("Waydroid"));
        assert!(is_waydroid_multiwindow_app_id(
            "waydroid.com.android.settings"
        ));
        assert!(is_waydroid_multiwindow_app_id("WAYDROID.org.fdroid.fdroid"));
    }

    #[test]
    fn resolves_workspace_and_at_startup_matches_and_merges_launch_rules() {
        use regex::Regex;

        use super::{
            DynamicWindowRules, InitialWindowRules, OpenMode, PresetColumnWidth,
            WindowMatchContext, WindowMatchSpec, WindowRuleEngine, WindowRuleEntry,
        };

        let mut engine = WindowRuleEngine::new();
        engine.push_rule(WindowRuleEntry {
            id: 1,
            name: Some("startup-fm".to_owned()),
            enabled: true,
            matches: vec![WindowMatchSpec {
                app_id: Some(Regex::new("^thunar$").unwrap()),
                workspace: Some("1".to_owned()),
                at_startup: Some(true),
                ..Default::default()
            }],
            excludes: Vec::new(),
            initial: InitialWindowRules {
                open_on_output: Some("DP-1".to_owned()),
                open_mode: Some(OpenMode::Float),
                default_width: Some(PresetColumnWidth::Fixed(960)),
                default_height: Some(PresetColumnWidth::Fixed(640)),
                max_width: Some(PresetColumnWidth::Proportion(0.8)),
                max_height: Some(PresetColumnWidth::Proportion(0.8)),
                min_width: Some(PresetColumnWidth::Fixed(320)),
                min_height: Some(PresetColumnWidth::Fixed(240)),
                ..Default::default()
            },
            dynamic: DynamicWindowRules {
                immediate: Some(true),
                suppress_maximize: Some(true),
                ..Default::default()
            },
        });

        let matching_ctx = WindowMatchContext {
            app_id: "thunar",
            title: "Home",
            workspace: Some("1"),
            at_startup: true,
            ..Default::default()
        };
        let initial = engine.resolve_initial(&matching_ctx);
        assert_eq!(initial.open_on_output.as_deref(), Some("DP-1"));
        assert_eq!(initial.open_mode, Some(OpenMode::Float));
        assert_eq!(initial.default_width, Some(PresetColumnWidth::Fixed(960)));
        assert_eq!(initial.default_height, Some(PresetColumnWidth::Fixed(640)));
        assert_eq!(initial.max_width, Some(PresetColumnWidth::Proportion(0.8)));
        assert_eq!(initial.max_height, Some(PresetColumnWidth::Proportion(0.8)));
        assert_eq!(initial.min_width, Some(PresetColumnWidth::Fixed(320)));
        assert_eq!(initial.min_height, Some(PresetColumnWidth::Fixed(240)));

        let dyn_rules = engine.resolve_dynamic(&matching_ctx);
        assert_eq!(dyn_rules.immediate, Some(true));
        assert_eq!(dyn_rules.suppress_maximize, Some(true));

        let non_startup_ctx = WindowMatchContext {
            at_startup: false,
            ..matching_ctx
        };
        assert_eq!(engine.resolve_initial(&non_startup_ctx).open_mode, None);

        let wrong_ws_ctx = WindowMatchContext {
            workspace: Some("2"),
            ..matching_ctx
        };
        assert_eq!(engine.resolve_initial(&wrong_ws_ctx).open_mode, None);
    }
}
