//! Single Rust source of truth for Koma's built-in keybind catalogue.
//!
//! Each entry pairs the physical chord with the Lua expression that constructs the
//! same [`ActionDescriptor`](crate::ActionDescriptor) and a human description. The
//! `komactl init` scaffold renders `lua/keybinds.lua` from this table, and Koma's
//! `keybind_dispatcher` tests cross-check it against
//! `KeybindTable::with_builtin_defaults()` so the generated full keymap cannot
//! drift from the runtime fallback.
//!
//! Built-in bindings are active when `koma.input({ inherit_default_binds = true })`
//! (the default) or when `~/.config/koma/init.lua` defines zero binds.
//! Documentation: <https://github.com/komawm/docs/blob/main/docs/LUA_API_SPEC.md>.

/// One built-in keybind exposed to both the runtime defaults and the generated
/// configuration scaffold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeybindCatalogEntry {
    /// Stable `BindHandleId` used by `KeybindTable::with_builtin_defaults()`.
    pub id: u64,
    /// Chord string understood by the chord parser (`Mod + Shift + H`, `Mod + MouseLeft`, ...).
    pub chord: &'static str,
    /// Lua expression evaluated by `koma.bind(chord, <expression>)`.
    pub lua_action: &'static str,
    /// English description copied into the generated `desc = "..."` option.
    pub description: &'static str,
    /// Optional debounce interval for high-frequency pointer triggers.
    pub cooldown_ms: Option<u64>,
    /// Presentation group used by the generated file.
    pub group: &'static str,
}

const WINDOW: &str = "Windows";
const LAYOUT: &str = "Dwindle and Scrolling layout";
const SCRATCHPAD: &str = "Scratchpad";
const FOCUS: &str = "Focus and movement";
const WORKSPACES: &str = "Workspaces";
const POINTER: &str = "Pointer drag and wheel";
const SESSION: &str = "Session";

/// The complete built-in keymap, in a stable presentation order.
///
/// Ids mirror `KeybindTable::with_builtin_defaults()`; `u64::MAX - 1` and
/// `u64::MAX` are the synthetic ids for the always-available
/// `Mod + MouseLeft` / `Mod + MouseRight` drag binds.
pub const KEYBIND_CATALOG: &[KeybindCatalogEntry] = &[
    KeybindCatalogEntry {
        id: 1,
        chord: "Mod + Return",
        lua_action: r#"koma.act.spawn({ "bitty" })"#,
        description: "Open terminal",
        cooldown_ms: None,
        group: SESSION,
    },
    KeybindCatalogEntry {
        id: 2,
        chord: "Mod + Q",
        lua_action: "koma.act.window.close()",
        description: "Close active window",
        cooldown_ms: None,
        group: WINDOW,
    },
    KeybindCatalogEntry {
        id: 3,
        chord: "Mod + V",
        lua_action: "koma.act.window.float_toggle()",
        description: "Toggle floating layer",
        cooldown_ms: None,
        group: WINDOW,
    },
    KeybindCatalogEntry {
        id: 4,
        chord: "Mod + F",
        lua_action: "koma.act.window.fullscreen_toggle()",
        description: "Toggle fullscreen",
        cooldown_ms: None,
        group: WINDOW,
    },
    KeybindCatalogEntry {
        id: 5,
        chord: "Mod + P",
        lua_action: "koma.act.window.pseudotile_toggle()",
        description: "Toggle Dwindle pseudotile",
        cooldown_ms: None,
        group: WINDOW,
    },
    KeybindCatalogEntry {
        id: 6,
        chord: "Mod + E",
        lua_action: "koma.act.dwindle.toggle_split()",
        description: "Toggle Dwindle split axis",
        cooldown_ms: None,
        group: LAYOUT,
    },
    KeybindCatalogEntry {
        id: 28,
        chord: "Mod + R",
        lua_action: "koma.act.scrolling.switch_preset_column_width()",
        description: "Cycle Scrolling column width forward",
        cooldown_ms: None,
        group: LAYOUT,
    },
    KeybindCatalogEntry {
        id: 29,
        chord: "Mod + Ctrl + R",
        lua_action: "koma.act.scrolling.switch_preset_column_width_back()",
        description: "Cycle Scrolling column width backward",
        cooldown_ms: None,
        group: LAYOUT,
    },
    KeybindCatalogEntry {
        id: 30,
        chord: "Mod + C",
        lua_action: "koma.act.scrolling.center_column()",
        description: "Center active Scrolling column",
        cooldown_ms: None,
        group: LAYOUT,
    },
    KeybindCatalogEntry {
        id: 31,
        chord: "Mod + M",
        lua_action: "koma.act.window.maximize_toggle()",
        description: "Toggle maximize (work area, bar stays visible)",
        cooldown_ms: None,
        group: LAYOUT,
    },
    KeybindCatalogEntry {
        id: 36,
        chord: "Mod + Shift + M",
        lua_action: "koma.act.scrolling.maximize_column()",
        description: "Maximize active Scrolling column",
        cooldown_ms: None,
        group: LAYOUT,
    },
    KeybindCatalogEntry {
        id: 32,
        chord: "Mod + Comma",
        lua_action: "koma.act.scrolling.consume_window_into_column()",
        description: "Consume right window into column stack",
        cooldown_ms: None,
        group: LAYOUT,
    },
    KeybindCatalogEntry {
        id: 33,
        chord: "Mod + Period",
        lua_action: "koma.act.scrolling.expel_window_from_column()",
        description: "Expel focused window out of column stack",
        cooldown_ms: None,
        group: LAYOUT,
    },
    KeybindCatalogEntry {
        id: 34,
        chord: "Mod + Ctrl + Shift + R",
        lua_action: "koma.act.scrolling.switch_preset_window_height()",
        description: "Cycle Scrolling tile height preset",
        cooldown_ms: None,
        group: LAYOUT,
    },
    KeybindCatalogEntry {
        id: 35,
        chord: "Mod + W",
        lua_action: "koma.act.scrolling.toggle_column_tabbed_display()",
        description: "Toggle Scrolling column Tabbed / Split mode",
        cooldown_ms: None,
        group: LAYOUT,
    },
    KeybindCatalogEntry {
        id: 21,
        chord: "Mod + BracketLeft",
        lua_action: "koma.act.scrolling.consume_or_expel_window_left()",
        description: "Consume or expel window left",
        cooldown_ms: None,
        group: LAYOUT,
    },
    KeybindCatalogEntry {
        id: 22,
        chord: "Mod + BracketRight",
        lua_action: "koma.act.scrolling.consume_or_expel_window_right()",
        description: "Consume or expel window right",
        cooldown_ms: None,
        group: LAYOUT,
    },
    KeybindCatalogEntry {
        id: 7,
        chord: "Mod + S",
        lua_action: "koma.act.scratchpad.toggle()",
        description: "Toggle Scratchpad overlay",
        cooldown_ms: None,
        group: SCRATCHPAD,
    },
    KeybindCatalogEntry {
        id: 12,
        chord: "Mod + Shift + S",
        lua_action: "koma.act.scratchpad.move_window()",
        description: "Send or pop the active window from the Scratchpad",
        cooldown_ms: None,
        group: SCRATCHPAD,
    },
    KeybindCatalogEntry {
        id: 13,
        chord: "Mod + H",
        lua_action: "koma.act.focus.left()",
        description: "Focus left tile",
        cooldown_ms: None,
        group: FOCUS,
    },
    KeybindCatalogEntry {
        id: 14,
        chord: "Mod + J",
        lua_action: "koma.act.focus.down()",
        description: "Focus down tile",
        cooldown_ms: None,
        group: FOCUS,
    },
    KeybindCatalogEntry {
        id: 15,
        chord: "Mod + K",
        lua_action: "koma.act.focus.up()",
        description: "Focus up tile",
        cooldown_ms: None,
        group: FOCUS,
    },
    KeybindCatalogEntry {
        id: 16,
        chord: "Mod + L",
        lua_action: "koma.act.focus.right()",
        description: "Focus right tile",
        cooldown_ms: None,
        group: FOCUS,
    },
    KeybindCatalogEntry {
        id: 17,
        chord: "Mod + Shift + H",
        lua_action: r#"koma.act.window.move_direction("left")"#,
        description: "Move window left",
        cooldown_ms: None,
        group: FOCUS,
    },
    KeybindCatalogEntry {
        id: 18,
        chord: "Mod + Shift + J",
        lua_action: r#"koma.act.window.move_direction("down")"#,
        description: "Move window down",
        cooldown_ms: None,
        group: FOCUS,
    },
    KeybindCatalogEntry {
        id: 19,
        chord: "Mod + Shift + K",
        lua_action: r#"koma.act.window.move_direction("up")"#,
        description: "Move window up",
        cooldown_ms: None,
        group: FOCUS,
    },
    KeybindCatalogEntry {
        id: 20,
        chord: "Mod + Shift + L",
        lua_action: r#"koma.act.window.move_direction("right")"#,
        description: "Move window right",
        cooldown_ms: None,
        group: FOCUS,
    },
    KeybindCatalogEntry {
        id: 23,
        chord: "Mod + Tab",
        lua_action: "koma.act.overview.toggle()",
        description: "Toggle workspace overview",
        cooldown_ms: None,
        group: WORKSPACES,
    },
    KeybindCatalogEntry {
        id: 8,
        chord: "Mod + WheelScrollDown",
        lua_action: "koma.act.workspace.focus_next()",
        description: "Next workspace",
        cooldown_ms: Some(150),
        group: WORKSPACES,
    },
    KeybindCatalogEntry {
        id: 9,
        chord: "Mod + WheelScrollUp",
        lua_action: "koma.act.workspace.focus_prev()",
        description: "Previous workspace",
        cooldown_ms: Some(150),
        group: WORKSPACES,
    },
    KeybindCatalogEntry {
        id: 10,
        chord: "Mod + WheelScrollRight",
        lua_action: "koma.act.deck.cycle_next()",
        description: "Next Deck window",
        cooldown_ms: Some(150),
        group: WORKSPACES,
    },
    KeybindCatalogEntry {
        id: 11,
        chord: "Mod + WheelScrollLeft",
        lua_action: "koma.act.deck.cycle_prev()",
        description: "Previous Deck window",
        cooldown_ms: Some(150),
        group: WORKSPACES,
    },
    KeybindCatalogEntry {
        id: 100 + 1,
        chord: "Mod + 1",
        lua_action: r#"koma.act.workspace.focus("1")"#,
        description: "Focus workspace 1",
        cooldown_ms: None,
        group: WORKSPACES,
    },
    KeybindCatalogEntry {
        id: 100 + 2,
        chord: "Mod + 2",
        lua_action: r#"koma.act.workspace.focus("2")"#,
        description: "Focus workspace 2",
        cooldown_ms: None,
        group: WORKSPACES,
    },
    KeybindCatalogEntry {
        id: 100 + 3,
        chord: "Mod + 3",
        lua_action: r#"koma.act.workspace.focus("3")"#,
        description: "Focus workspace 3",
        cooldown_ms: None,
        group: WORKSPACES,
    },
    KeybindCatalogEntry {
        id: 100 + 4,
        chord: "Mod + 4",
        lua_action: r#"koma.act.workspace.focus("4")"#,
        description: "Focus workspace 4",
        cooldown_ms: None,
        group: WORKSPACES,
    },
    KeybindCatalogEntry {
        id: 100 + 5,
        chord: "Mod + 5",
        lua_action: r#"koma.act.workspace.focus("5")"#,
        description: "Focus workspace 5",
        cooldown_ms: None,
        group: WORKSPACES,
    },
    KeybindCatalogEntry {
        id: 100 + 6,
        chord: "Mod + 6",
        lua_action: r#"koma.act.workspace.focus("6")"#,
        description: "Focus workspace 6",
        cooldown_ms: None,
        group: WORKSPACES,
    },
    KeybindCatalogEntry {
        id: 100 + 7,
        chord: "Mod + 7",
        lua_action: r#"koma.act.workspace.focus("7")"#,
        description: "Focus workspace 7",
        cooldown_ms: None,
        group: WORKSPACES,
    },
    KeybindCatalogEntry {
        id: 100 + 8,
        chord: "Mod + 8",
        lua_action: r#"koma.act.workspace.focus("8")"#,
        description: "Focus workspace 8",
        cooldown_ms: None,
        group: WORKSPACES,
    },
    KeybindCatalogEntry {
        id: 100 + 9,
        chord: "Mod + 9",
        lua_action: r#"koma.act.workspace.focus("9")"#,
        description: "Focus workspace 9",
        cooldown_ms: None,
        group: WORKSPACES,
    },
    KeybindCatalogEntry {
        id: 110 + 1,
        chord: "Mod + Shift + 1",
        lua_action: r#"koma.act.window.move_to_workspace("1")"#,
        description: "Move window to workspace 1",
        cooldown_ms: None,
        group: WORKSPACES,
    },
    KeybindCatalogEntry {
        id: 110 + 2,
        chord: "Mod + Shift + 2",
        lua_action: r#"koma.act.window.move_to_workspace("2")"#,
        description: "Move window to workspace 2",
        cooldown_ms: None,
        group: WORKSPACES,
    },
    KeybindCatalogEntry {
        id: 110 + 3,
        chord: "Mod + Shift + 3",
        lua_action: r#"koma.act.window.move_to_workspace("3")"#,
        description: "Move window to workspace 3",
        cooldown_ms: None,
        group: WORKSPACES,
    },
    KeybindCatalogEntry {
        id: 110 + 4,
        chord: "Mod + Shift + 4",
        lua_action: r#"koma.act.window.move_to_workspace("4")"#,
        description: "Move window to workspace 4",
        cooldown_ms: None,
        group: WORKSPACES,
    },
    KeybindCatalogEntry {
        id: 110 + 5,
        chord: "Mod + Shift + 5",
        lua_action: r#"koma.act.window.move_to_workspace("5")"#,
        description: "Move window to workspace 5",
        cooldown_ms: None,
        group: WORKSPACES,
    },
    KeybindCatalogEntry {
        id: 110 + 6,
        chord: "Mod + Shift + 6",
        lua_action: r#"koma.act.window.move_to_workspace("6")"#,
        description: "Move window to workspace 6",
        cooldown_ms: None,
        group: WORKSPACES,
    },
    KeybindCatalogEntry {
        id: 110 + 7,
        chord: "Mod + Shift + 7",
        lua_action: r#"koma.act.window.move_to_workspace("7")"#,
        description: "Move window to workspace 7",
        cooldown_ms: None,
        group: WORKSPACES,
    },
    KeybindCatalogEntry {
        id: 110 + 8,
        chord: "Mod + Shift + 8",
        lua_action: r#"koma.act.window.move_to_workspace("8")"#,
        description: "Move window to workspace 8",
        cooldown_ms: None,
        group: WORKSPACES,
    },
    KeybindCatalogEntry {
        id: 110 + 9,
        chord: "Mod + Shift + 9",
        lua_action: r#"koma.act.window.move_to_workspace("9")"#,
        description: "Move window to workspace 9",
        cooldown_ms: None,
        group: WORKSPACES,
    },
    KeybindCatalogEntry {
        id: 26,
        chord: "Mod + MouseLeft",
        lua_action: "koma.act.window.drag_move()",
        description: "Drag move a floating or tiled window",
        cooldown_ms: None,
        group: POINTER,
    },
    KeybindCatalogEntry {
        id: 27,
        chord: "Mod + MouseRight",
        lua_action: "koma.act.window.drag_resize()",
        description: "Drag resize a corner or Dwindle split ratio",
        cooldown_ms: None,
        group: POINTER,
    },
    KeybindCatalogEntry {
        id: 24,
        chord: "Mod + Shift + R",
        lua_action: "koma.act.reload_config()",
        description: "Reload the configuration atomically",
        cooldown_ms: None,
        group: SESSION,
    },
    KeybindCatalogEntry {
        id: 25,
        chord: "Mod + Shift + E",
        lua_action: "koma.act.quit()",
        description: "Quit the compositor gracefully",
        cooldown_ms: None,
        group: SESSION,
    },
];

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::KEYBIND_CATALOG;

    #[test]
    fn catalog_chords_are_unique() {
        let mut seen = BTreeSet::new();
        for entry in KEYBIND_CATALOG {
            assert!(
                seen.insert(entry.chord),
                "duplicate catalog chord `{}`",
                entry.chord
            );
        }
    }

    #[test]
    fn catalog_entries_have_actions_and_descriptions() {
        for entry in KEYBIND_CATALOG {
            assert!(
                entry.lua_action.starts_with("koma.act."),
                "`{}` does not use a koma.act.* expression",
                entry.chord
            );
            assert!(
                !entry.description.trim().is_empty(),
                "`{}` is missing a description",
                entry.chord
            );
        }
    }
}
