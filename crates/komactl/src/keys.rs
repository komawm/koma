//! `komactl keys` — the effective keymap, mouse map, and gesture list.
//!
//! The listing merges the built-in catalogue (`koma_lua::KEYBIND_CATALOG`, drift-tested against
//! `KeybindTable::with_builtin_defaults()`) with the user configuration, applying the same rules
//! as the compositor's `KeybindTable::compile_with_options`: `inherit_default_binds`, `koma.unbind`,
//! in-place chord overrides, and the always-available `Mod + MouseLeft` / `Mod + MouseRight` drag
//! fallbacks. It works offline; when a live `koma-admin.sock` is reachable the running
//! configuration path, generation, and lock state are merged into the report.

use std::path::{Path, PathBuf};

use koma_lua::schema::TriggerEdge;
use koma_lua::{
    ActionDescriptor, BindHandleId, KEYBIND_CATALOG, KeybindCatalogEntry, KomaConfigRegistry,
};
use regex::RegexBuilder;
use serde_json::{Value, json};

use crate::config_probe;

/// Pointer trigger family of a chord, used to split keyboard, mouse-button, and scroll bindings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChordKind {
    Key,
    PointerButton,
    Wheel,
    Touchpad,
}

impl ChordKind {
    #[must_use]
    pub(crate) fn is_pointer(self) -> bool {
        !matches!(self, Self::Key)
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Key => "key",
            Self::PointerButton => "mouse_button",
            Self::Wheel => "wheel",
            Self::Touchpad => "touchpad",
        }
    }
}

/// One effective binding row.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct BindRow {
    pub chord: String,
    pub raw_chord: String,
    pub mode: Option<String>,
    pub edge: TriggerEdge,
    pub enabled: bool,
    /// Short action descriptor shown in the `ACTION` column.
    pub action: String,
    /// Full Lua expression (`koma.act.*`) or descriptor for callback binds.
    pub expression: String,
    pub description: String,
    /// `builtin` or `config:<path>`.
    pub source: String,
    pub origin_builtin: bool,
    pub overridden_builtin: bool,
    pub synthetic: bool,
    pub kind: ChordKind,
    /// `true` when holding the chord repeats the action (`BindRepeat::WhileHeld`).
    pub repeat: bool,
}

/// One configured touchpad gesture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GestureRow {
    pub chord: String,
    pub action: String,
    pub expression: String,
    pub description: String,
    pub source: String,
    pub enabled: bool,
    pub cooldown_ms: Option<u64>,
}

/// Effective configuration listing.
#[derive(Debug, Clone)]
pub(crate) struct KeyListing {
    pub rows: Vec<BindRow>,
    pub gestures: Vec<GestureRow>,
    pub config_path: PathBuf,
    pub inherits_builtins: bool,
    pub config_error: Option<String>,
    pub live: Option<Value>,
    pub live_error: Option<String>,
}

impl KeyListing {
    #[must_use]
    pub fn keyboard(&self) -> Vec<&BindRow> {
        self.rows
            .iter()
            .filter(|r| r.kind == ChordKind::Key)
            .collect()
    }

    #[must_use]
    pub fn pointer(&self) -> Vec<&BindRow> {
        self.rows.iter().filter(|r| r.kind.is_pointer()).collect()
    }

    fn overridden_count(&self) -> usize {
        self.rows.iter().filter(|r| r.overridden_builtin).count()
    }
}

// =============================================================================
// Chord formatting
// =============================================================================

const MODIFIER_ORDER: &[&str] = &["Mod", "Ctrl", "Shift", "Alt", "Super", "Mod5"];

fn canonical_modifier(token: &str) -> Option<&'static str> {
    match token.trim().to_ascii_lowercase().as_str() {
        "mod" => Some("Mod"),
        "ctrl" | "control" => Some("Ctrl"),
        "shift" => Some("Shift"),
        "alt" | "mod1" => Some("Alt"),
        "super" | "win" | "logo" | "meta" | "mod4" => Some("Super"),
        "mod5" | "altgr" | "iso_level3_shift" => Some("Mod5"),
        _ => None,
    }
}

fn canonical_pointer_trigger(token: &str) -> Option<(&'static str, ChordKind)> {
    match token.trim().to_ascii_lowercase().as_str() {
        "mouseleft" | "mouse_left" | "mouse1" | "lmb" | "btn_left" | "mouse:272" => {
            Some(("MouseLeft", ChordKind::PointerButton))
        }
        "mouseright" | "mouse_right" | "mouse2" | "rmb" | "btn_right" | "mouse:273" => {
            Some(("MouseRight", ChordKind::PointerButton))
        }
        "mousemiddle" | "mouse_middle" | "mouse3" | "mmb" | "btn_middle" | "mouse:274" => {
            Some(("MouseMiddle", ChordKind::PointerButton))
        }
        "mouseback" | "mouse_back" | "mouse4" | "btn_side" | "mouse:275" => {
            Some(("MouseBack", ChordKind::PointerButton))
        }
        "mouseforward" | "mouse_forward" | "mouse5" | "btn_extra" | "mouse:276" => {
            Some(("MouseForward", ChordKind::PointerButton))
        }
        "wheelscrollup" | "wheel_scroll_up" | "mousewheelup" => {
            Some(("WheelScrollUp", ChordKind::Wheel))
        }
        "wheelscrolldown" | "wheel_scroll_down" | "mousewheeldown" => {
            Some(("WheelScrollDown", ChordKind::Wheel))
        }
        "wheelscrollleft" | "wheel_scroll_left" | "mousewheelleft" => {
            Some(("WheelScrollLeft", ChordKind::Wheel))
        }
        "wheelscrollright" | "wheel_scroll_right" | "mousewheelright" => {
            Some(("WheelScrollRight", ChordKind::Wheel))
        }
        "touchpadscrollup" | "touchpad_scroll_up" => {
            Some(("TouchpadScrollUp", ChordKind::Touchpad))
        }
        "touchpadscrolldown" | "touchpad_scroll_down" => {
            Some(("TouchpadScrollDown", ChordKind::Touchpad))
        }
        "touchpadscrollleft" | "touchpad_scroll_left" => {
            Some(("TouchpadScrollLeft", ChordKind::Touchpad))
        }
        "touchpadscrollright" | "touchpad_scroll_right" => {
            Some(("TouchpadScrollRight", ChordKind::Touchpad))
        }
        _ => None,
    }
}

fn canonical_key_trigger(token: &str) -> String {
    let lowered = token.trim().to_ascii_lowercase();
    let canonical = match lowered.as_str() {
        "enter" | "return" => "Return",
        "esc" | "escape" => "Escape",
        "space" => "Space",
        "tab" => "Tab",
        "backspace" | "bs" => "BackSpace",
        "del" | "delete" => "Delete",
        "[" => "BracketLeft",
        "]" => "BracketRight",
        "/" => "Slash",
        "\\" => "Backslash",
        "-" => "Minus",
        "=" => "Equal",
        "+" => "Plus",
        "," => "Comma",
        "." => "Period",
        ";" => "Semicolon",
        "'" => "Apostrophe",
        "`" => "Grave",
        "pageup" | "page_up" | "pgup" => "Page_Up",
        "pagedown" | "page_down" | "pgdn" => "Page_Down",
        "print" | "printscreen" | "print_screen" | "sysrq" => "Print",
        "left" => "Left",
        "right" => "Right",
        "up" => "Up",
        "down" => "Down",
        "home" => "Home",
        "end" => "End",
        "xf86audioprevious" | "audioprev" | "audioprevious" => "XF86AudioPrev",
        "audionext" => "XF86AudioNext",
        "audioplay" => "XF86AudioPlay",
        "audiopause" => "XF86AudioPause",
        "audiostop" => "XF86AudioStop",
        "audiomute" => "XF86AudioMute",
        "audiomicmute" => "XF86AudioMicMute",
        "audioraisevolume" => "XF86AudioRaiseVolume",
        "audiolowervolume" => "XF86AudioLowerVolume",
        "monbrightnessup" | "brightnessup" => "XF86MonBrightnessUp",
        "monbrightnessdown" | "brightnessdown" => "XF86MonBrightnessDown",
        "kbdbrightnessup" => "XF86KbdBrightnessUp",
        "kbdbrightnessdown" => "XF86KbdBrightnessDown",
        _ => {
            if lowered.len() > 1
                && lowered.starts_with('f')
                && lowered[1..].chars().all(|c| c.is_ascii_digit())
            {
                return lowered.to_ascii_uppercase();
            }
            if lowered.chars().count() == 1 {
                return lowered.to_ascii_uppercase();
            }
            return token.trim().to_owned();
        }
    };
    canonical.to_owned()
}

fn canonical_parts(raw: &str) -> (Vec<String>, String, ChordKind) {
    let tokens: Vec<&str> = raw
        .split('+')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect();
    let Some((trigger, modifiers)) = tokens.split_last() else {
        return (Vec::new(), raw.trim().to_owned(), ChordKind::Key);
    };
    let mut mods: Vec<String> = Vec::new();
    for token in modifiers {
        let name = canonical_modifier(token).unwrap_or(token.trim()).to_owned();
        if !mods.contains(&name) {
            mods.push(name);
        }
    }
    mods.sort_by_key(|name| {
        MODIFIER_ORDER
            .iter()
            .position(|known| known == name)
            .unwrap_or(MODIFIER_ORDER.len())
    });
    if let Some((name, kind)) = canonical_pointer_trigger(trigger) {
        (mods, name.to_owned(), kind)
    } else {
        (mods, canonical_key_trigger(trigger), ChordKind::Key)
    }
}

/// Renders a chord as `Mod + Shift + 2`, canonicalizing modifier order and alias spellings.
#[must_use]
pub(crate) fn format_chord(raw: &str) -> String {
    let (mods, trigger, _) = canonical_parts(raw);
    let mut parts = mods;
    parts.push(trigger);
    parts.join(" + ")
}

#[must_use]
pub(crate) fn chord_kind(raw: &str) -> ChordKind {
    canonical_parts(raw).2
}

/// Identity used for override/unbind bookkeeping, mirroring
/// `chords_match_under_mod_override`: when `input.mod_key` is set, `<Mod>` and that physical
/// modifier denote the same chord.
#[must_use]
pub(crate) fn chord_identity(raw: &str, mod_key: Option<&str>) -> String {
    let (mods, trigger, _) = canonical_parts(raw);
    let mapped: Vec<String> = mods
        .into_iter()
        .map(|name| {
            if name != "Mod" {
                return name;
            }
            match mod_key.map(str::trim).filter(|m| !m.is_empty()) {
                Some(explicit) if canonical_modifier(explicit) != Some("Mod") => {
                    canonical_modifier(explicit).map_or_else(|| "Mod".to_owned(), str::to_owned)
                }
                _ => "Mod".to_owned(),
            }
        })
        .collect();
    let mut deduped: Vec<String> = Vec::new();
    for name in mapped {
        if !deduped.contains(&name) {
            deduped.push(name);
        }
    }
    deduped.sort_by_key(|name| {
        MODIFIER_ORDER
            .iter()
            .position(|known| known == name)
            .unwrap_or(MODIFIER_ORDER.len())
    });
    deduped.push(trigger);
    deduped.join(" + ")
}

// =============================================================================
// Action description
// =============================================================================

pub(crate) fn lua_quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// Renders an [`ActionDescriptor`] as the Lua expression a user would write.
#[must_use]
pub(crate) fn describe_action(action: &ActionDescriptor) -> String {
    match action {
        ActionDescriptor::Spawn { argv } => {
            let args: Vec<String> = argv.iter().map(|arg| lua_quote(arg)).collect();
            format!("koma.act.spawn({{ {} }})", args.join(", "))
        }
        ActionDescriptor::WindowClose => "koma.act.window.close()".to_owned(),
        ActionDescriptor::WindowFullscreenToggle => {
            "koma.act.window.fullscreen_toggle()".to_owned()
        }
        ActionDescriptor::WindowMaximizeToggle => "koma.act.window.maximize_toggle()".to_owned(),
        ActionDescriptor::WindowFloatToggle => "koma.act.window.float_toggle()".to_owned(),
        ActionDescriptor::WindowPseudotileToggle => {
            "koma.act.window.pseudotile_toggle()".to_owned()
        }
        ActionDescriptor::WindowDragMove => "koma.act.window.drag_move()".to_owned(),
        ActionDescriptor::WindowDragResize => "koma.act.window.drag_resize()".to_owned(),
        ActionDescriptor::WindowResizeDelta { width, height } => {
            format!("koma.act.window.resize({{ width = {width}, height = {height} }})")
        }
        ActionDescriptor::WindowMoveToWorkspace { workspace, follow } => {
            if *follow {
                format!(
                    "koma.act.window.move_to_workspace({})",
                    lua_quote(workspace)
                )
            } else {
                format!(
                    "koma.act.window.move_to_workspace({}, {{ follow = false }})",
                    lua_quote(workspace)
                )
            }
        }
        ActionDescriptor::WindowSwapDirection(dir) => {
            format!("koma.act.window.swap({})", lua_quote(dir))
        }
        ActionDescriptor::WindowMoveDirection(dir) => {
            format!("koma.act.window.move_direction({})", lua_quote(dir))
        }
        ActionDescriptor::FocusDirection(dir) => format!("koma.act.focus.{dir}()"),
        ActionDescriptor::DwindleToggleSplit => "koma.act.dwindle.toggle_split()".to_owned(),
        ActionDescriptor::DwindleSwapSplit => "koma.act.dwindle.swap_split()".to_owned(),
        ActionDescriptor::DwindleSplitRatio(delta) => {
            format!("koma.act.dwindle.split_ratio({delta})")
        }
        ActionDescriptor::ScrollingSwitchPresetWidth { forward } => {
            if *forward {
                "koma.act.scrolling.switch_preset_column_width()".to_owned()
            } else {
                "koma.act.scrolling.switch_preset_column_width_back()".to_owned()
            }
        }
        ActionDescriptor::ScrollingSwitchPresetHeight { forward } => {
            if *forward {
                "koma.act.scrolling.switch_preset_window_height()".to_owned()
            } else {
                "koma.act.scrolling.switch_preset_window_height_back()".to_owned()
            }
        }
        ActionDescriptor::ScrollingResetWindowHeight => {
            "koma.act.scrolling.reset_window_height()".to_owned()
        }
        ActionDescriptor::ScrollingToggleColumnTabbed => {
            "koma.act.scrolling.toggle_column_tabbed_display()".to_owned()
        }
        ActionDescriptor::ScrollingMaximizeColumn => {
            "koma.act.scrolling.maximize_column()".to_owned()
        }
        ActionDescriptor::ScrollingCenterColumn => "koma.act.scrolling.center_column()".to_owned(),
        ActionDescriptor::ScrollingMoveColumnLeft => {
            "koma.act.scrolling.move_column_left()".to_owned()
        }
        ActionDescriptor::ScrollingMoveColumnRight => {
            "koma.act.scrolling.move_column_right()".to_owned()
        }
        ActionDescriptor::ScrollingConsumeIntoColumn => {
            "koma.act.scrolling.consume_window_into_column()".to_owned()
        }
        ActionDescriptor::ScrollingExpelFromColumn => {
            "koma.act.scrolling.expel_window_from_column()".to_owned()
        }
        ActionDescriptor::ScrollingConsumeOrExpelLeft => {
            "koma.act.scrolling.consume_or_expel_window_left()".to_owned()
        }
        ActionDescriptor::ScrollingConsumeOrExpelRight => {
            "koma.act.scrolling.consume_or_expel_window_right()".to_owned()
        }
        ActionDescriptor::ScratchpadToggle(name) => match name {
            Some(name) => format!("koma.act.scratchpad.toggle({})", lua_quote(name)),
            None => "koma.act.scratchpad.toggle()".to_owned(),
        },
        ActionDescriptor::ScratchpadMoveWindow { name, silent } => {
            let name = name.as_deref().map_or_else(|| "nil".to_owned(), lua_quote);
            format!("koma.act.scratchpad.move_window({name}, {silent})")
        }
        ActionDescriptor::DeckCycleNext => "koma.act.deck.cycle_next()".to_owned(),
        ActionDescriptor::DeckCyclePrev => "koma.act.deck.cycle_prev()".to_owned(),
        ActionDescriptor::OverviewToggle => "koma.act.overview.toggle()".to_owned(),
        ActionDescriptor::OverviewOpen => "koma.act.overview.open()".to_owned(),
        ActionDescriptor::ScrollStripSwipe => "koma.act.scroll_strip.swipe()".to_owned(),
        ActionDescriptor::WorkspaceFocus(ws) => {
            format!("koma.act.workspace.focus({})", lua_quote(ws))
        }
        ActionDescriptor::WorkspaceFocusNext => "koma.act.workspace.focus_next()".to_owned(),
        ActionDescriptor::WorkspaceFocusPrev => "koma.act.workspace.focus_prev()".to_owned(),
        ActionDescriptor::WorkspacePeek(ws) => {
            format!("koma.act.workspace.peek({})", lua_quote(ws))
        }
        ActionDescriptor::WorkspaceSetLayout { workspace, layout } => {
            let layout = format!("{layout:?}").to_ascii_lowercase();
            match workspace {
                Some(ws) => format!(
                    "koma.act.workspace.set_layout({}, {})",
                    lua_quote(&layout),
                    lua_quote(ws)
                ),
                None => format!("koma.act.workspace.set_layout({})", lua_quote(&layout)),
            }
        }
        ActionDescriptor::WindowFocus(win) => format!("koma.act.window.focus({win})"),
        ActionDescriptor::MonitorFocus(name) => {
            format!("koma.act.monitor.focus({})", lua_quote(name))
        }
        ActionDescriptor::MonitorFocusDirection(dir) => {
            format!("koma.act.monitor.focus_direction({})", lua_quote(dir))
        }
        ActionDescriptor::WindowMoveToMonitor(name) => {
            format!("koma.act.window.move_to_monitor({})", lua_quote(name))
        }
        ActionDescriptor::WindowMoveToMonitorDirection(dir) => {
            format!(
                "koma.act.window.move_to_monitor_direction({})",
                lua_quote(dir)
            )
        }
        ActionDescriptor::ModeEnter(name) => format!("koma.act.mode.enter({})", lua_quote(name)),
        ActionDescriptor::ModeExit => "koma.act.mode.exit()".to_owned(),
        ActionDescriptor::ReloadConfig => "koma.act.reload_config()".to_owned(),
        ActionDescriptor::CompositorQuit => "koma.act.quit()".to_owned(),
        ActionDescriptor::RunMacro { macro_id, args } => {
            if args.is_null() || args.as_object().is_some_and(serde_json::Map::is_empty) {
                format!("koma.act.macro.run({})", lua_quote(macro_id.as_str()))
            } else {
                format!(
                    "koma.act.macro.run({}, {})",
                    lua_quote(macro_id.as_str()),
                    args
                )
            }
        }
        ActionDescriptor::LuaCallback(BindHandleId(id)) => {
            format!("function(ctx) ... end (bind #{id})")
        }
    }
}

/// Short descriptor used by the `ACTION` column: the expression without the `koma.act.` prefix.
#[must_use]
pub(crate) fn short_action(expression: &str) -> String {
    expression
        .strip_prefix("koma.act.")
        .unwrap_or(expression)
        .to_owned()
}

// =============================================================================
// Effective listing
// =============================================================================

/// Shortens the user's home directory to `~` for the human table; JSON keeps the absolute path.
fn display_source(source: &str) -> String {
    match std::env::var_os("HOME") {
        Some(home) if !home.is_empty() => {
            let home = home.to_string_lossy();
            let Some(rest) = source.strip_prefix(home.as_ref()) else {
                return source.to_owned();
            };
            if !rest.starts_with('/') {
                return source.to_owned();
            }
            format!("~{rest}")
        }
        _ => source.to_owned(),
    }
}

fn source_for(config_path: &Path) -> String {
    format!("config:{}", config_path.display())
}

fn edge_label(edge: TriggerEdge) -> &'static str {
    match edge {
        TriggerEdge::Press => "press",
        TriggerEdge::Release => "release",
        TriggerEdge::Both => "both",
    }
}

fn builtin_row(entry: &KeybindCatalogEntry, mod_key: Option<&str>) -> BindRow {
    let expression = entry.lua_action.to_owned();
    BindRow {
        chord: format_chord(entry.chord),
        raw_chord: entry.chord.to_owned(),
        mode: None,
        edge: TriggerEdge::Press,
        enabled: true,
        action: short_action(&expression),
        expression,
        description: entry.description.to_owned(),
        source: "builtin".to_owned(),
        origin_builtin: true,
        overridden_builtin: false,
        synthetic: false,
        kind: chord_kind(&chord_identity(entry.chord, mod_key)),
        repeat: entry.repeat.repeats(),
    }
}

fn config_row(spec: &koma_lua::BindSpec, config_path: &Path, mod_key: Option<&str>) -> BindRow {
    let expression = describe_action(&spec.action);
    BindRow {
        chord: format_chord(&spec.chord),
        raw_chord: spec.chord.clone(),
        mode: spec.mode.clone(),
        edge: spec.options.edge,
        enabled: spec.enabled,
        action: short_action(&expression),
        expression,
        description: spec.options.desc.clone().unwrap_or_default(),
        source: source_for(config_path),
        origin_builtin: false,
        overridden_builtin: false,
        synthetic: false,
        kind: chord_kind(&chord_identity(&spec.chord, mod_key)),
        repeat: spec.options.repeats(&spec.action),
    }
}

fn apply_config_row(
    row: &mut BindRow,
    spec: &koma_lua::BindSpec,
    config_path: &Path,
    mod_key: Option<&str>,
) {
    let mut replacement = config_row(spec, config_path, mod_key);
    replacement.overridden_builtin = row.origin_builtin;
    if replacement.description.is_empty() {
        replacement.description = row.description.clone();
    }
    // Keep the builtin description so the catalogue stays visible for overrides.
    if replacement.overridden_builtin && row.origin_builtin {
        replacement.description =
            format!("{} (builtin: {})", replacement.description, row.description);
    }
    *row = replacement;
}

/// Merges the built-in catalogue with the evaluated user configuration.
#[must_use]
pub(crate) fn merge_binds(
    registry: &KomaConfigRegistry,
    config_path: &Path,
    catalogue: &[KeybindCatalogEntry],
) -> Vec<BindRow> {
    let mod_key = registry.input.mod_key.as_deref();
    let unbinds: Vec<String> = registry
        .unbinds
        .iter()
        .map(|raw| chord_identity(raw, mod_key))
        .collect();

    let mut rows: Vec<BindRow> = Vec::new();
    if registry.input.inherit_default_binds {
        for entry in catalogue {
            if unbinds.contains(&chord_identity(entry.chord, mod_key)) {
                continue;
            }
            rows.push(builtin_row(entry, mod_key));
        }
    }

    for spec in &registry.binds {
        let identity = chord_identity(&spec.chord, mod_key);
        if let Some(existing) = rows.iter_mut().find(|row| {
            row.mode == spec.mode
                && row.edge == spec.options.edge
                && chord_identity(&row.raw_chord, mod_key) == identity
        }) {
            apply_config_row(existing, spec, config_path, mod_key);
        } else {
            rows.push(config_row(spec, config_path, mod_key));
        }
    }

    // `KeybindTable::compile_with_options` keeps the pointer drag fallbacks available unless a
    // bind or an unbind claims the chord, even with `inherit_default_binds = false`.
    for (chord, action, description) in [
        (
            "Mod + MouseLeft",
            ActionDescriptor::WindowDragMove,
            "Drag move a floating or tiled window",
        ),
        (
            "Mod + MouseRight",
            ActionDescriptor::WindowDragResize,
            "Drag resize a corner or Dwindle split ratio",
        ),
    ] {
        let identity = chord_identity(chord, mod_key);
        let claimed = unbinds.contains(&identity)
            || rows.iter().any(|row| {
                row.mode.is_none() && chord_identity(&row.raw_chord, mod_key) == identity
            });
        if claimed {
            continue;
        }
        let expression = describe_action(&action);
        rows.push(BindRow {
            chord: format_chord(chord),
            raw_chord: chord.to_owned(),
            mode: None,
            edge: TriggerEdge::Press,
            enabled: true,
            action: short_action(&expression),
            expression,
            description: description.to_owned(),
            source: "builtin".to_owned(),
            origin_builtin: true,
            overridden_builtin: false,
            synthetic: true,
            kind: ChordKind::PointerButton,
            repeat: false,
        });
    }

    rows
}

#[must_use]
pub(crate) fn gesture_rows(registry: &KomaConfigRegistry, config_path: &Path) -> Vec<GestureRow> {
    registry
        .gestures
        .iter()
        .map(|gesture| {
            let expression = describe_action(&gesture.action);
            GestureRow {
                chord: format!("{}-finger {}", gesture.fingers, gesture.direction),
                action: short_action(&expression),
                expression,
                description: gesture.desc.clone().unwrap_or_default(),
                source: source_for(config_path),
                enabled: gesture.enabled,
                cooldown_ms: gesture.cooldown_ms,
            }
        })
        .collect()
}

// =============================================================================
// Rendering
// =============================================================================

fn truncate(text: &str, width: usize) -> String {
    let count = text.chars().count();
    if count <= width {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(width.saturating_sub(2)).collect();
    out.push_str("..");
    out
}

fn render_table(header: &[&str], rows: &[Vec<String>]) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let caps = [30usize, 40, 60, 40, 0];
    let mut widths: Vec<usize> = header.iter().map(|cell| cell.chars().count()).collect();
    for row in rows {
        for (idx, cell) in row.iter().enumerate() {
            let cap = caps.get(idx).copied().unwrap_or(0);
            let len = cell.chars().count();
            let len = if cap == 0 { len } else { len.min(cap) };
            if idx < widths.len() && len > widths[idx] {
                widths[idx] = len;
            }
        }
    }
    let mut out = String::new();
    let render_row = |cells: &[String], out: &mut String| {
        let mut line = String::from("  ");
        for (idx, cell) in cells.iter().enumerate() {
            let cap = caps.get(idx).copied().unwrap_or(0);
            let width = widths[idx];
            let text = if cap == 0 {
                cell.clone()
            } else {
                truncate(cell, cap)
            };
            if idx + 1 == cells.len() {
                line.push_str(&text);
            } else {
                line.push_str(&format!("{text:<width$}  "));
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    };
    let header_cells: Vec<String> = header.iter().map(|cell| (*cell).to_owned()).collect();
    render_row(&header_cells, &mut out);
    for row in rows {
        render_row(row, &mut out);
    }
    out
}

fn bind_rows_cells(rows: &[&BindRow]) -> Vec<Vec<String>> {
    rows.iter()
        .map(|row| {
            let mode = row
                .mode
                .as_ref()
                .map_or_else(String::new, |mode| format!(" [{mode}]"));
            let edge = if row.edge == TriggerEdge::Press {
                String::new()
            } else {
                format!(" ({})", edge_label(row.edge))
            };
            let disabled = if row.enabled { "" } else { " [disabled]" };
            let repeat = if row.repeat { " [repeat]" } else { "" };
            let description = if row.description.is_empty() {
                String::from("-")
            } else {
                row.description.clone()
            };
            vec![
                format!("{}{mode}", row.chord),
                format!("{}{edge}{disabled}{repeat}", row.action),
                row.expression.clone(),
                display_source(&row.source),
                description,
            ]
        })
        .collect()
}

/// Renders the human-readable listing.
#[must_use]
pub(crate) fn render_human(listing: &KeyListing) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "komactl keys — {} ({} keyboard, {} pointer, {} gesture; {})\n",
        listing.config_path.display(),
        listing.keyboard().len(),
        listing.pointer().len(),
        listing.gestures.len(),
        if listing.inherits_builtins {
            "builtins inherited"
        } else {
            "builtins disabled by inherit_default_binds"
        }
    ));
    if let Some(live) = &listing.live {
        let generation = live
            .get("generation")
            .and_then(Value::as_u64)
            .map_or_else(|| "?".to_owned(), |g| g.to_string());
        let active = live
            .get("active_path")
            .and_then(Value::as_str)
            .unwrap_or("?");
        out.push_str(&format!(
            "live: generation {generation}, active config {active}\n"
        ));
    }
    if let Some(err) = &listing.live_error {
        out.push_str(&format!("live: {err}\n"));
    }
    if let Some(err) = &listing.config_error {
        out.push_str(&format!(
            "warning: the configuration could not be evaluated, showing built-ins only\n  {err}\n"
        ));
    }
    out.push('\n');

    let header = ["CHORD", "ACTION", "EXPRESSION", "SOURCE", "DESCRIPTION"];
    let keyboard = listing.keyboard();
    out.push_str(&format!("KEYBOARD BINDS ({})\n", keyboard.len()));
    if keyboard.is_empty() {
        out.push_str("  (none)\n");
    } else {
        out.push_str(&render_table(&header, &bind_rows_cells(&keyboard)));
    }
    out.push('\n');

    let pointer = listing.pointer();
    out.push_str(&format!("MOUSE AND SCROLL BINDS ({})\n", pointer.len()));
    if pointer.is_empty() {
        out.push_str("  (none)\n");
    } else {
        out.push_str(&render_table(&header, &bind_rows_cells(&pointer)));
    }
    out.push('\n');

    out.push_str(&format!("GESTURES ({})\n", listing.gestures.len()));
    if listing.gestures.is_empty() {
        out.push_str("  (none)\n");
    } else {
        let rows: Vec<Vec<String>> = listing
            .gestures
            .iter()
            .map(|gesture| {
                let cooldown = gesture
                    .cooldown_ms
                    .map_or_else(String::new, |ms| format!(" ({ms} ms cooldown)"));
                let disabled = if gesture.enabled { "" } else { " [disabled]" };
                vec![
                    gesture.chord.clone(),
                    format!("{}{cooldown}{disabled}", gesture.action),
                    gesture.expression.clone(),
                    display_source(&gesture.source),
                    if gesture.description.is_empty() {
                        String::from("-")
                    } else {
                        gesture.description.clone()
                    },
                ]
            })
            .collect();
        out.push_str(&render_table(
            &["GESTURE", "ACTION", "EXPRESSION", "SOURCE", "DESCRIPTION"],
            &rows,
        ));
    }

    let overrides = listing.overridden_count();
    out.push_str(&format!(
        "\n{overrides} built-in {} overridden by the configuration\n",
        if overrides == 1 {
            "binding"
        } else {
            "bindings"
        }
    ));
    out
}

/// Builds the `--json` document.
#[must_use]
pub(crate) fn render_json(listing: &KeyListing) -> Value {
    let bind_json = |row: &BindRow| {
        json!({
            "chord": row.chord,
            "raw_chord": row.raw_chord,
            "kind": row.kind.as_str(),
            "action": row.action,
            "expression": row.expression,
            "source": row.source,
            "origin": if row.origin_builtin { "builtin" } else { "config" },
            "overridden_builtin": row.overridden_builtin,
            "synthetic": row.synthetic,
            "description": row.description,
            "mode": row.mode,
            "edge": edge_label(row.edge),
            "enabled": row.enabled,
            "repeat": row.repeat,
        })
    };
    let binds: Vec<Value> = listing
        .keyboard()
        .iter()
        .map(|row| bind_json(row))
        .collect();
    let pointer: Vec<Value> = listing.pointer().iter().map(|row| bind_json(row)).collect();
    let gestures: Vec<Value> = listing
        .gestures
        .iter()
        .map(|gesture| {
            json!({
                "gesture": gesture.chord,
                "action": gesture.action,
                "expression": gesture.expression,
                "source": gesture.source,
                "description": gesture.description,
                "enabled": gesture.enabled,
                "cooldown_ms": gesture.cooldown_ms,
            })
        })
        .collect();
    json!({
        "config": listing.config_path.display().to_string(),
        "inherits_builtins": listing.inherits_builtins,
        "config_error": listing.config_error,
        "live": listing.live,
        "live_error": listing.live_error,
        "counts": {
            "keyboard": binds.len(),
            "pointer": pointer.len(),
            "gestures": gestures.len(),
            "total": binds.len() + pointer.len(),
            "overridden_builtins": listing.overridden_count(),
            "catalogue": KEYBIND_CATALOG.len(),
        },
        "binds": binds,
        "pointer_binds": pointer,
        "gestures": gestures,
    })
}

// =============================================================================
// Command
// =============================================================================

const KEYS_USAGE_EXIT: i32 = 2;

fn keys_help() -> &'static str {
    "komactl keys — list every effective keybind, pointer bind, and gesture

Usage:
  komactl keys [path/to/init.lua] [--json] [--grep <pattern>] [-h | --help]

Lists the effective keymap: the built-in catalogue merged with the user
configuration (input.inherit_default_binds, koma.unbind, and chord overrides),
plus the pointer drag/scroll binds and touchpad gestures. Every row reports the
chord (`Mod + Shift + 2`), the action, the Lua expression or action descriptor,
the source (`builtin` or `config:<file>`), and the catalogue description.

Overrides of a built-in chord keep the built-in description in parentheses so
the original binding stays visible. Rows whose action keeps firing while the
chord is held are marked `[repeat]` (movement, focus, resize, split-ratio,
workspace and preset cycling); `repeat = true|false` overrides the default.
Pointer and gesture binds are listed in their own sections. Nothing is hidden: binds the compositor cannot translate
back into a catalogue entry are printed with their raw action descriptor.

Works offline by default: the configuration is evaluated in the same jailed
shadow VM as `komactl validate`, so no compositor is required. When a live
`koma-admin.sock` is reachable, the running generation, active configuration
path, and lock state are merged into the header and the JSON `live` object.

Options:
  --json            Emit one JSON document (binds, pointer_binds, gestures, counts)
  --grep <pattern>  Only list rows matching the regular expression (case-insensitive)
  -h, --help        Print this help

Exit codes:
  0  listing produced
  1  the configuration could not be evaluated (built-ins are still listed)
  2  usage error or internal failure
"
}

#[derive(Debug, Clone, Default)]
struct KeysArgs {
    path: Option<PathBuf>,
    json: bool,
    grep: Option<String>,
    help: bool,
}

fn parse_args(args: &[String]) -> Result<KeysArgs, String> {
    let mut parsed = KeysArgs::default();
    let mut cursor = 0;
    while cursor < args.len() {
        let arg = &args[cursor];
        match arg.as_str() {
            "--json" => parsed.json = true,
            "-h" | "--help" => parsed.help = true,
            "--grep" => {
                let Some(pattern) = args.get(cursor + 1) else {
                    return Err("`--grep` requires a pattern".to_owned());
                };
                parsed.grep = Some(pattern.clone());
                cursor += 1;
            }
            other if other.starts_with("--grep=") => {
                parsed.grep = Some(other["--grep=".len()..].to_owned());
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown option `{other}`"));
            }
            other => {
                if parsed.path.is_some() {
                    return Err(format!("unexpected argument `{other}`"));
                }
                parsed.path = Some(PathBuf::from(other));
            }
        }
        cursor += 1;
    }
    Ok(parsed)
}

fn build_matcher(pattern: &str) -> Result<regex::Regex, String> {
    RegexBuilder::new(pattern)
        .case_insensitive(true)
        .build()
        .map_err(|e| format!("invalid --grep pattern `{pattern}`: {e}"))
}

fn row_matches(row: &BindRow, matcher: &regex::Regex) -> bool {
    matcher.is_match(&row.chord)
        || matcher.is_match(&row.action)
        || matcher.is_match(&row.expression)
        || matcher.is_match(&row.description)
        || matcher.is_match(&row.source)
}

fn gesture_matches(gesture: &GestureRow, matcher: &regex::Regex) -> bool {
    matcher.is_match(&gesture.chord)
        || matcher.is_match(&gesture.action)
        || matcher.is_match(&gesture.expression)
        || matcher.is_match(&gesture.description)
        || matcher.is_match(&gesture.source)
}

fn listing_from_path(
    path: &Path,
    live: Option<Value>,
    live_error: Option<String>,
) -> (KeyListing, bool) {
    match config_probe::evaluate(path) {
        Ok(evaluation) => {
            let registry = evaluation.registry.unwrap_or_default();
            let failed = evaluation.eval_error.is_some();
            let rows = merge_binds(&registry, path, KEYBIND_CATALOG);
            let gestures = gesture_rows(&registry, path);
            (
                KeyListing {
                    rows,
                    gestures,
                    config_path: path.to_path_buf(),
                    inherits_builtins: registry.input.inherit_default_binds,
                    config_error: evaluation.eval_error,
                    live,
                    live_error,
                },
                failed,
            )
        }
        Err(err) => (
            KeyListing {
                rows: merge_binds(&KomaConfigRegistry::default(), path, KEYBIND_CATALOG),
                gestures: Vec::new(),
                config_path: path.to_path_buf(),
                inherits_builtins: true,
                config_error: Some(err),
                live,
                live_error,
            },
            true,
        ),
    }
}

/// Entry point for `komactl keys`.
pub(crate) fn run(args: &[String]) -> i32 {
    let parsed = match parse_args(args) {
        Ok(parsed) => parsed,
        Err(err) => {
            eprintln!("komactl keys: {err}\n\n{}", keys_help());
            return KEYS_USAGE_EXIT;
        }
    };
    if parsed.help {
        crate::emit(keys_help());
        return 0;
    }
    let matcher = match parsed.grep.as_deref().map(build_matcher).transpose() {
        Ok(matcher) => matcher,
        Err(err) => {
            eprintln!("komactl keys: {err}");
            return KEYS_USAGE_EXIT;
        }
    };

    let (live, live_error) = match config_probe::probe_live() {
        config_probe::LiveProbe::Reachable { status, .. } => (Some(status), None),
        config_probe::LiveProbe::Offline { .. } => (None, None),
        config_probe::LiveProbe::Unreachable {
            admin_socket,
            detail,
        } => (
            None,
            Some(format!(
                "found {} but could not read config.status: {detail}",
                admin_socket.display()
            )),
        ),
    };

    // Merge the live state: the compositor reports the configuration it actually loaded, so an
    // implicit path is replaced by the live one before parsing.
    let mut target = parsed.path.clone();
    if target.is_none()
        && let Some(active) = live
            .as_ref()
            .and_then(|status| status.get("active_path"))
            .and_then(Value::as_str)
            .filter(|path| !path.is_empty())
    {
        target = Some(PathBuf::from(active));
    }
    let path = target.unwrap_or_else(config_probe::default_init_lua_path);

    let (mut listing, config_failed) = listing_from_path(&path, live, live_error);
    if let Some(matcher) = &matcher {
        listing.rows.retain(|row| row_matches(row, matcher));
        listing
            .gestures
            .retain(|gesture| gesture_matches(gesture, matcher));
    }

    if parsed.json {
        let mut text = serde_json::to_string_pretty(&render_json(&listing)).unwrap();
        text.push('\n');
        crate::emit(&text);
    } else if listing.rows.is_empty() && listing.gestures.is_empty() {
        let mut text = render_human(&listing);
        if let Some(pattern) = &parsed.grep {
            text.push_str(&format!("no bind matches `{pattern}`\n"));
        }
        crate::emit(&text);
    } else {
        crate::emit(&render_human(&listing));
    }

    i32::from(config_failed)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::atomic::{AtomicU32, Ordering},
    };

    use koma_lua::scaffold_config_tree;

    use super::*;

    static NEXT_ID: AtomicU32 = AtomicU32::new(0);

    fn temp_root(label: &str) -> PathBuf {
        let base = Path::new("/tmp/koma");
        fs::create_dir_all(base).expect("create /tmp/koma");
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let dir = base.join(format!("keys-{label}-{}-{id}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn listing_for(source: &str, extra: &[(&str, &str)]) -> (PathBuf, KomaConfigRegistry) {
        let root = temp_root("listing");
        fs::create_dir_all(root.join("lua")).expect("create lua dir");
        fs::write(root.join("init.lua"), source).expect("write init.lua");
        for (name, body) in extra {
            fs::write(root.join(name), body).expect("write module");
        }
        let evaluation = config_probe::evaluate(&root.join("init.lua")).expect("evaluate");
        assert!(
            evaluation.eval_error.is_none(),
            "{:?}",
            evaluation.eval_error
        );
        (root, evaluation.registry.expect("registry"))
    }

    #[test]
    fn scaffolded_config_lists_the_whole_catalogue() {
        let root = temp_root("scaffold");
        scaffold_config_tree(&root, false).expect("scaffold");
        let evaluation = config_probe::evaluate(&root.join("init.lua")).expect("evaluate");
        assert!(
            evaluation.eval_error.is_none(),
            "{:?}",
            evaluation.eval_error
        );
        let registry = evaluation.registry.expect("registry");
        let rows = merge_binds(&registry, &root.join("init.lua"), KEYBIND_CATALOG);
        assert_eq!(
            rows.len(),
            KEYBIND_CATALOG.len(),
            "the scaffold spells out every built-in bind, so the merge must not add rows"
        );
        assert!(
            rows.iter().any(|row| row.kind == ChordKind::PointerButton),
            "pointer binds must be classified out of the keyboard section"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn config_only_bind_extends_the_catalogue_count() {
        let (root, registry) = listing_for(
            "koma.bind(\"Mod + D\", koma.act.spawn({ \"fuzzel\" }), { desc = \"Launcher\" })\n",
            &[],
        );
        let rows = merge_binds(&registry, &root.join("init.lua"), KEYBIND_CATALOG);
        assert_eq!(rows.len(), KEYBIND_CATALOG.len() + 1);
        let custom = rows
            .iter()
            .find(|row| row.chord == "Mod + D")
            .expect("config-only bind listed");
        assert_eq!(
            custom.source,
            format!("config:{}", root.join("init.lua").display())
        );
        assert!(!custom.origin_builtin);
        assert_eq!(custom.description, "Launcher");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn overriding_a_builtin_chord_keeps_the_catalogue_visible() {
        let (root, registry) = listing_for(
            "koma.bind(\"Mod + Q\", koma.act.spawn({ \"noop\" }))\n",
            &[],
        );
        let rows = merge_binds(&registry, &root.join("init.lua"), KEYBIND_CATALOG);
        assert_eq!(
            rows.len(),
            KEYBIND_CATALOG.len(),
            "an override replaces the built-in row instead of adding one"
        );
        let overridden = rows
            .iter()
            .find(|row| row.chord == "Mod + Q")
            .expect("override listed");
        assert!(overridden.overridden_builtin);
        assert!(
            overridden
                .description
                .contains("builtin: Close active window")
        );
        assert!(overridden.expression.contains("spawn"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn unbind_removes_the_builtin_and_the_pointer_fallback_is_restored() {
        let (root, registry) = listing_for(
            "koma.unbind(\"Mod + Q\")\nkoma.input({ inherit_default_binds = false })\n",
            &[],
        );
        let rows = merge_binds(&registry, &root.join("init.lua"), KEYBIND_CATALOG);
        assert!(rows.iter().all(|row| row.chord != "Mod + Q"));
        assert!(
            rows.iter().filter(|row| row.synthetic).count() == 2,
            "the pointer drag fallbacks stay available when builtins are disabled"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn gestures_and_modes_are_listed() {
        let (root, registry) = listing_for(
            "koma.gesture({ fingers = 3, direction = \"horizontal\", action = koma.act.scrolling.swipe(), desc = \"Swipe\" })\n\
             koma.mode(\"resize\", {})\n\
             koma.bind(\"Mod + H\", koma.act.focus.left(), { desc = \"Resize left\" })\n",
            &[],
        );
        let gestures = gesture_rows(&registry, &root.join("init.lua"));
        assert_eq!(gestures.len(), 1);
        assert_eq!(gestures[0].chord, "3-finger horizontal");
        assert_eq!(gestures[0].description, "Swipe");
        let rows = merge_binds(&registry, &root.join("init.lua"), KEYBIND_CATALOG);
        let overridden = rows
            .iter()
            .find(|row| row.chord == "Mod + H")
            .expect("override listed");
        assert!(overridden.overridden_builtin);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn chords_are_canonicalized_without_raw_masks() {
        assert_eq!(format_chord("shift+mod+2"), "Mod + Shift + 2");
        assert_eq!(format_chord("mod + mouse:272"), "Mod + MouseLeft");
        assert_eq!(format_chord("ctrl+alt+t"), "Ctrl + Alt + T");
        assert_eq!(chord_kind("Mod + WheelScrollDown"), ChordKind::Wheel);
        assert_eq!(chord_kind("Mod + TouchpadScrollLeft"), ChordKind::Touchpad);
        assert_eq!(chord_kind("Mod + MouseRight"), ChordKind::PointerButton);
        assert_eq!(chord_kind("Mod + Shift + 2"), ChordKind::Key);
    }

    #[test]
    fn mod_key_makes_mod_and_the_physical_modifier_the_same_chord() {
        assert_eq!(chord_identity("Mod + Q", None), "Mod + Q");
        assert_eq!(chord_identity("Mod + Q", Some("Super")), "Super + Q");
        assert_eq!(chord_identity("Super + Q", Some("super")), "Super + Q");
        assert_ne!(chord_identity("Super + Q", None), "Mod + Q");
    }

    #[test]
    fn action_descriptions_render_lua_expressions() {
        assert_eq!(
            describe_action(&ActionDescriptor::WindowClose),
            "koma.act.window.close()"
        );
        assert_eq!(
            describe_action(&ActionDescriptor::Spawn {
                argv: vec!["bitty".to_owned()]
            }),
            "koma.act.spawn({ \"bitty\" })"
        );
        assert_eq!(
            describe_action(&ActionDescriptor::WindowMoveToWorkspace {
                workspace: "2".to_owned(),
                follow: false
            }),
            "koma.act.window.move_to_workspace(\"2\", { follow = false })"
        );
        assert_eq!(
            describe_action(&ActionDescriptor::LuaCallback(BindHandleId(9))),
            "function(ctx) ... end (bind #9)"
        );
    }

    #[test]
    fn human_and_json_rendering_report_counts_and_overrides() {
        let root = temp_root("render");
        scaffold_config_tree(&root, false).expect("scaffold");
        let evaluation = config_probe::evaluate(&root.join("init.lua")).expect("evaluate");
        let registry = evaluation.registry.expect("registry");
        let listing = KeyListing {
            rows: merge_binds(&registry, &root.join("init.lua"), KEYBIND_CATALOG),
            gestures: gesture_rows(&registry, &root.join("init.lua")),
            config_path: root.join("init.lua"),
            inherits_builtins: registry.input.inherit_default_binds,
            config_error: None,
            live: None,
            live_error: None,
        };
        let human = render_human(&listing);
        assert!(human.contains("KEYBOARD BINDS ("));
        assert!(human.contains("MOUSE AND SCROLL BINDS ("));
        assert!(human.contains("GESTURES ("));
        assert!(human.contains("builtins disabled by inherit_default_binds"));
        assert!(human.contains("Mod + Return"));

        let json = render_json(&listing);
        assert_eq!(json["counts"]["catalogue"], KEYBIND_CATALOG.len());
        assert_eq!(json["counts"]["total"], KEYBIND_CATALOG.len());
        assert_eq!(json["counts"]["gestures"], listing.gestures.len());
        assert!(
            json["binds"]
                .as_array()
                .is_some_and(|rows| { rows.iter().all(|row| row["chord"].as_str().is_some()) })
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn repeat_rows_are_marked_in_the_human_and_json_listings() {
        let (root, registry) = listing_for(
            "koma.bind(\"Mod + D\", koma.act.focus.left())\n\
             koma.bind(\"Mod + U\", koma.act.spawn({ \"bitty\" }))\n\
             koma.bind(\"Mod + Y\", koma.act.focus.up(), { repeat_while_held = false })\n\
             koma.bind(\"Mod + I\", koma.act.window.close(), { repeat_while_held = true })\n",
            &[],
        );
        let rows = merge_binds(&registry, &root.join("init.lua"), KEYBIND_CATALOG);
        let repeat_of = |chord: &str| {
            rows.iter()
                .find(|row| row.chord == chord)
                .unwrap_or_else(|| panic!("`{chord}` is missing from the listing"))
                .repeat
        };

        // Built-ins: movement/focus repeat, spawn/close/quit/reload do not.
        assert!(repeat_of("Mod + Shift + H"), "window movement repeats");
        assert!(repeat_of("Mod + H"), "directional focus repeats");
        assert!(!repeat_of("Mod + Return"), "spawning must stay one-shot");
        assert!(!repeat_of("Mod + Q"), "closing must stay one-shot");
        assert!(!repeat_of("Mod + Shift + E"), "quitting must stay one-shot");
        assert!(
            !repeat_of("Mod + Shift + R"),
            "reloading must stay one-shot"
        );

        // Config-only binds derive from the action, and the explicit option wins.
        assert!(repeat_of("Mod + D"), "derived from the focus action");
        assert!(!repeat_of("Mod + U"), "derived from the spawn action");
        assert!(
            !repeat_of("Mod + Y"),
            "explicit `repeat_while_held = false` wins"
        );
        assert!(
            repeat_of("Mod + I"),
            "explicit `repeat_while_held = true` wins"
        );

        let listing = KeyListing {
            rows,
            gestures: Vec::new(),
            config_path: root.join("init.lua"),
            inherits_builtins: true,
            config_error: None,
            live: None,
            live_error: None,
        };
        let human = render_human(&listing);
        let movement_line = human
            .lines()
            .find(|line| line.starts_with("  Mod + Shift + H"))
            .expect("movement row rendered");
        assert!(
            movement_line.contains("[repeat]"),
            "repeat rows must be marked in the human table: {movement_line}"
        );
        let return_line = human
            .lines()
            .find(|line| line.starts_with("  Mod + Return"))
            .expect("terminal row rendered");
        assert!(
            !return_line.contains("[repeat]"),
            "one-shot rows must not be marked: {return_line}"
        );

        let json = render_json(&listing);
        let binds = json["binds"].as_array().expect("binds array");
        let json_repeat = |chord: &str| {
            binds
                .iter()
                .find(|row| row["chord"] == chord)
                .unwrap_or_else(|| panic!("`{chord}` missing from JSON"))
                .get("repeat")
                .and_then(Value::as_bool)
        };
        assert_eq!(json_repeat("Mod + Shift + H"), Some(true));
        assert_eq!(json_repeat("Mod + Return"), Some(false));
        assert_eq!(json_repeat("Mod + Y"), Some(false));
        assert_eq!(json_repeat("Mod + I"), Some(true));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn grep_filters_rows_and_gestures() {
        let (root, registry) = listing_for(
            "koma.gesture({ fingers = 4, direction = \"up\", action = koma.act.overview.toggle(), desc = \"Overview swipe\" })\n",
            &[],
        );
        let matcher = build_matcher("overview").expect("valid regex");
        let rows = merge_binds(&registry, &root.join("init.lua"), KEYBIND_CATALOG);
        let matched: Vec<&BindRow> = rows
            .iter()
            .filter(|row| row_matches(row, &matcher))
            .collect();
        assert!(
            matched
                .iter()
                .all(|row| row.description.to_lowercase().contains("overview")),
            "grep must not match unrelated rows"
        );
        assert!(!matched.is_empty());
        let gestures = gesture_rows(&registry, &root.join("init.lua"));
        assert!(gestures.iter().all(|g| gesture_matches(g, &matcher)));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn broken_config_still_lists_builtins() {
        let root = temp_root("broken");
        fs::create_dir_all(&root).expect("fixture dir");
        let init = root.join("init.lua");
        fs::write(&init, "koma.bind(\"Mod + X\",\n").expect("write init.lua");
        let (listing, failed) = listing_from_path(&init, None, None);
        assert!(failed);
        assert!(listing.config_error.is_some());
        assert_eq!(listing.rows.len(), KEYBIND_CATALOG.len());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn help_documents_the_offline_contract() {
        let help = keys_help();
        for needle in [
            "--json",
            "--grep",
            "offline",
            "Exit codes:",
            "config:<file>",
        ] {
            assert!(help.contains(needle), "help is missing `{needle}`");
        }
    }

    #[test]
    fn argument_parsing_accepts_flags_in_any_order() {
        let args: Vec<String> = ["--json", "--grep", "mod", "/tmp/koma/x.lua"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        let parsed = parse_args(&args).expect("parse");
        assert!(parsed.json);
        assert_eq!(parsed.grep.as_deref(), Some("mod"));
        assert_eq!(parsed.path, Some(PathBuf::from("/tmp/koma/x.lua")));
        let err = parse_args(&["--grep".to_owned()]).expect_err("missing value");
        assert!(err.contains("--grep"));
    }
}
