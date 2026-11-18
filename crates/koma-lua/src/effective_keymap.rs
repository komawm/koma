//! Shared effective keymap: the built-in catalogue merged with the evaluated user configuration.
//!
//! `komactl keys` and the compositor's keybind overview both call [`merge_binds`], so the CLI
//! listing and the on-screen overlay can never disagree about the active chords, overrides, or
//! catalogue descriptions. The merge mirrors `KeybindTable::compile_with_options`:
//! `input.inherit_default_binds`, `koma.unbind`, in-place chord overrides, and the
//! always-available `Mod + MouseLeft` / `Mod + MouseRight` drag fallbacks.
//!
//! CLI table rendering deliberately lives in `komactl`; this module only produces the rows.

use std::path::Path;

use crate::{
    ActionDescriptor, BindHandleId, KomaConfigRegistry,
    keybind_catalog::{
        GROUP_FOCUS, GROUP_LAYOUT, GROUP_POINTER, GROUP_SCRATCHPAD, GROUP_SESSION, GROUP_WINDOWS,
        GROUP_WORKSPACES, KeybindCatalogEntry,
    },
    schema::TriggerEdge,
};

/// Pointer trigger family of a chord, used to split keyboard, mouse-button, and scroll bindings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChordKind {
    Key,
    PointerButton,
    Wheel,
    Touchpad,
}

impl ChordKind {
    #[must_use]
    pub const fn is_pointer(self) -> bool {
        !matches!(self, Self::Key)
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Key => "key",
            Self::PointerButton => "mouse_button",
            Self::Wheel => "wheel",
            Self::Touchpad => "touchpad",
        }
    }
}

/// One effective binding row shared by `komactl keys` and the compositor keybind overlay.
#[derive(Debug, Clone, PartialEq)]
pub struct EffectiveBind {
    /// Canonical display chord (`Mod + Shift + Slash`).
    pub chord: String,
    /// Chord as authored in the catalogue or configuration.
    pub raw_chord: String,
    pub mode: Option<String>,
    pub edge: TriggerEdge,
    pub enabled: bool,
    /// Short action descriptor (`window.close()`), shown in the CLI `ACTION` column.
    pub action: String,
    /// Full Lua expression (`koma.act.*`) or descriptor for callback binds.
    pub expression: String,
    pub description: String,
    /// `builtin` or `config:<path>`.
    pub source: String,
    pub origin_builtin: bool,
    pub overridden_builtin: bool,
    /// Synthetic `Mod + MouseLeft` / `Mod + MouseRight` drag fallbacks.
    pub synthetic: bool,
    pub kind: ChordKind,
    /// Presentation group (`Windows`, `Focus and movement`, ...).
    pub group: &'static str,
}

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
pub fn format_chord(raw: &str) -> String {
    let (mods, trigger, _) = canonical_parts(raw);
    let mut parts = mods;
    parts.push(trigger);
    parts.join(" + ")
}

#[must_use]
pub fn chord_kind(raw: &str) -> ChordKind {
    canonical_parts(raw).2
}

/// Identity used for override/unbind bookkeeping, mirroring
/// `chords_match_under_mod_override`: when `input.mod_key` is set, `<Mod>` and that physical
/// modifier denote the same chord.
#[must_use]
pub fn chord_identity(raw: &str, mod_key: Option<&str>) -> String {
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

/// Quotes a string as a Lua literal.
#[must_use]
pub fn lua_quote(text: &str) -> String {
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
pub fn describe_action(action: &ActionDescriptor) -> String {
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
        ActionDescriptor::OverlayKeybindsToggle => "koma.act.overlay.keybinds_toggle()".to_owned(),
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

/// Short descriptor used by the CLI `ACTION` column: the expression without the `koma.act.` prefix.
#[must_use]
pub fn short_action(expression: &str) -> String {
    expression
        .strip_prefix("koma.act.")
        .unwrap_or(expression)
        .to_owned()
}

/// Presentation group for a configuration-only bind whose action has no catalogue entry.
#[must_use]
pub fn group_for_action(action: &ActionDescriptor) -> &'static str {
    match action {
        ActionDescriptor::WindowClose
        | ActionDescriptor::WindowFullscreenToggle
        | ActionDescriptor::WindowMaximizeToggle
        | ActionDescriptor::WindowFloatToggle
        | ActionDescriptor::WindowPseudotileToggle
        | ActionDescriptor::WindowDragMove
        | ActionDescriptor::WindowDragResize
        | ActionDescriptor::WindowResizeDelta { .. } => GROUP_WINDOWS,
        ActionDescriptor::DwindleToggleSplit
        | ActionDescriptor::DwindleSwapSplit
        | ActionDescriptor::DwindleSplitRatio(_)
        | ActionDescriptor::ScrollingSwitchPresetWidth { .. }
        | ActionDescriptor::ScrollingSwitchPresetHeight { .. }
        | ActionDescriptor::ScrollingResetWindowHeight
        | ActionDescriptor::ScrollingToggleColumnTabbed
        | ActionDescriptor::ScrollingMaximizeColumn
        | ActionDescriptor::ScrollingCenterColumn
        | ActionDescriptor::ScrollingMoveColumnLeft
        | ActionDescriptor::ScrollingMoveColumnRight
        | ActionDescriptor::ScrollingConsumeIntoColumn
        | ActionDescriptor::ScrollingExpelFromColumn
        | ActionDescriptor::ScrollingConsumeOrExpelLeft
        | ActionDescriptor::ScrollingConsumeOrExpelRight
        | ActionDescriptor::DeckCycleNext
        | ActionDescriptor::DeckCyclePrev
        | ActionDescriptor::ScrollStripSwipe => GROUP_LAYOUT,
        ActionDescriptor::FocusDirection(_)
        | ActionDescriptor::WindowSwapDirection(_)
        | ActionDescriptor::WindowMoveDirection(_)
        | ActionDescriptor::WindowFocus(_) => GROUP_FOCUS,
        ActionDescriptor::WindowMoveToWorkspace { .. }
        | ActionDescriptor::WorkspaceFocus(_)
        | ActionDescriptor::WorkspaceFocusNext
        | ActionDescriptor::WorkspaceFocusPrev
        | ActionDescriptor::WorkspacePeek(_)
        | ActionDescriptor::WorkspaceSetLayout { .. }
        | ActionDescriptor::WindowMoveToMonitor(_)
        | ActionDescriptor::WindowMoveToMonitorDirection(_) => GROUP_WORKSPACES,
        ActionDescriptor::ScratchpadToggle(_) | ActionDescriptor::ScratchpadMoveWindow { .. } => {
            GROUP_SCRATCHPAD
        }
        ActionDescriptor::Spawn { .. }
        | ActionDescriptor::OverviewToggle
        | ActionDescriptor::OverviewOpen
        | ActionDescriptor::OverlayKeybindsToggle
        | ActionDescriptor::MonitorFocus(_)
        | ActionDescriptor::MonitorFocusDirection(_)
        | ActionDescriptor::ModeEnter(_)
        | ActionDescriptor::ModeExit
        | ActionDescriptor::ReloadConfig
        | ActionDescriptor::CompositorQuit
        | ActionDescriptor::RunMacro { .. }
        | ActionDescriptor::LuaCallback(_) => GROUP_SESSION,
    }
}

fn source_for(config_path: &Path) -> String {
    format!("config:{}", config_path.display())
}

fn builtin_row(entry: &KeybindCatalogEntry, mod_key: Option<&str>) -> EffectiveBind {
    let expression = entry.lua_action.to_owned();
    EffectiveBind {
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
        group: entry.group,
    }
}

fn config_row(spec: &crate::BindSpec, config_path: &Path, mod_key: Option<&str>) -> EffectiveBind {
    let expression = describe_action(&spec.action);
    let kind = chord_kind(&chord_identity(&spec.chord, mod_key));
    EffectiveBind {
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
        kind,
        group: if kind.is_pointer() {
            GROUP_POINTER
        } else {
            group_for_action(&spec.action)
        },
    }
}

fn apply_config_row(
    row: &mut EffectiveBind,
    spec: &crate::BindSpec,
    config_path: &Path,
    mod_key: Option<&str>,
) {
    let mut replacement = config_row(spec, config_path, mod_key);
    replacement.overridden_builtin = row.origin_builtin;
    // An override stays in the catalogue group it replaces, so the overlay does not reshuffle a
    // built-in chord just because the action changed.
    if row.origin_builtin {
        replacement.group = row.group;
    }
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
pub fn merge_binds(
    registry: &KomaConfigRegistry,
    config_path: &Path,
    catalogue: &[KeybindCatalogEntry],
) -> Vec<EffectiveBind> {
    let mod_key = registry.input.mod_key.as_deref();
    let unbinds: Vec<String> = registry
        .unbinds
        .iter()
        .map(|raw| chord_identity(raw, mod_key))
        .collect();

    let mut rows: Vec<EffectiveBind> = Vec::new();
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
        rows.push(EffectiveBind {
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
            group: GROUP_POINTER,
        });
    }

    rows
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::{KEYBIND_CATALOG, KomaConfigRegistry};

    fn registry(inherit: bool) -> KomaConfigRegistry {
        let mut registry = KomaConfigRegistry::default();
        registry.input.inherit_default_binds = inherit;
        registry
    }

    #[test]
    fn default_registry_merges_the_whole_catalogue_and_drag_fallbacks() {
        let rows = merge_binds(
            &registry(true),
            &PathBuf::from("/tmp/koma/init.lua"),
            KEYBIND_CATALOG,
        );
        assert_eq!(rows.len(), KEYBIND_CATALOG.len());
        assert!(rows.iter().all(|row| row.source == "builtin"));
        assert!(
            rows.iter().any(|row| row.chord == "Mod + Shift + Slash"
                && row.description == "Show the keybind overview"
                && row.group == "Session"),
            "the overlay toggle must be part of the effective keymap"
        );
    }

    #[test]
    fn config_override_keeps_the_catalogue_description_and_group() {
        let mut registry = registry(true);
        let mut spec = crate::BindSpec {
            id: BindHandleId(900),
            mode: None,
            enabled: true,
            chord: "Mod + Q".to_owned(),
            action: ActionDescriptor::Spawn {
                argv: vec!["noop".to_owned()],
            },
            release_action: None,
            options: crate::BindOptions::default(),
        };
        spec.options.desc = Some("Custom close".to_owned());
        registry.binds.push(spec);

        let rows = merge_binds(
            &registry,
            &PathBuf::from("/home/user/.config/koma/init.lua"),
            KEYBIND_CATALOG,
        );
        assert_eq!(rows.len(), KEYBIND_CATALOG.len());
        let overridden = rows
            .iter()
            .find(|row| row.chord == "Mod + Q")
            .expect("override row");
        assert!(overridden.overridden_builtin);
        assert!(!overridden.origin_builtin);
        assert_eq!(overridden.group, "Windows");
        assert_eq!(
            overridden.description,
            "Custom close (builtin: Close active window)"
        );
        assert_eq!(overridden.source, "config:/home/user/.config/koma/init.lua");
    }

    #[test]
    fn config_only_bind_extends_the_catalogue_and_uses_its_action_group() {
        let mut registry = registry(true);
        registry.binds.push(crate::BindSpec {
            id: BindHandleId(901),
            mode: None,
            enabled: true,
            chord: "Mod + D".to_owned(),
            action: ActionDescriptor::Spawn {
                argv: vec!["fuzzel".to_owned()],
            },
            release_action: None,
            options: crate::BindOptions::default(),
        });
        let rows = merge_binds(
            &registry,
            &PathBuf::from("/tmp/koma/init.lua"),
            KEYBIND_CATALOG,
        );
        assert_eq!(rows.len(), KEYBIND_CATALOG.len() + 1);
        let custom = rows.iter().find(|row| row.chord == "Mod + D").unwrap();
        assert_eq!(custom.group, "Session");
        assert!(!custom.overridden_builtin);
    }

    #[test]
    fn pointer_config_binds_group_under_pointer_drag_and_wheel() {
        let mut registry = registry(false);
        registry.binds.push(crate::BindSpec {
            id: BindHandleId(902),
            mode: None,
            enabled: true,
            chord: "Mod + WheelScrollDown".to_owned(),
            action: ActionDescriptor::WorkspaceFocusNext,
            release_action: None,
            options: crate::BindOptions::default(),
        });
        let rows = merge_binds(
            &registry,
            &PathBuf::from("/tmp/koma/init.lua"),
            KEYBIND_CATALOG,
        );
        let scroll = rows
            .iter()
            .find(|row| row.chord == "Mod + WheelScrollDown")
            .unwrap();
        assert_eq!(scroll.group, "Pointer drag and wheel");
        assert!(scroll.kind.is_pointer());
        // The two synthetic drag fallbacks still exist with `inherit_default_binds = false`.
        assert_eq!(rows.iter().filter(|row| row.synthetic).count(), 2);
    }

    #[test]
    fn unbind_removes_the_builtin_before_the_drag_fallback_check() {
        let mut registry = registry(false);
        registry.unbinds.push("Mod + Q".to_owned());
        registry.unbinds.push("Mod + MouseLeft".to_owned());
        let rows = merge_binds(
            &registry,
            &PathBuf::from("/tmp/koma/init.lua"),
            KEYBIND_CATALOG,
        );
        assert!(rows.iter().all(|row| row.chord != "Mod + Q"));
        let synthetic: Vec<&EffectiveBind> = rows.iter().filter(|row| row.synthetic).collect();
        assert_eq!(synthetic.len(), 1);
        assert_eq!(synthetic[0].chord, "Mod + MouseRight");
    }

    #[test]
    fn override_identity_honours_the_configured_mod_key() {
        let mut registry = registry(true);
        registry.input.mod_key = Some("Super".to_owned());
        registry.binds.push(crate::BindSpec {
            id: BindHandleId(903),
            mode: None,
            enabled: true,
            chord: "Super + Q".to_owned(),
            action: ActionDescriptor::WindowClose,
            release_action: None,
            options: crate::BindOptions::default(),
        });
        let rows = merge_binds(
            &registry,
            &PathBuf::from("/tmp/koma/init.lua"),
            KEYBIND_CATALOG,
        );
        assert_eq!(rows.len(), KEYBIND_CATALOG.len());
        assert_eq!(
            rows.iter()
                .filter(|row| row.overridden_builtin && row.chord == "Super + Q")
                .count(),
            1
        );
    }
}
