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
use koma_lua::{KEYBIND_CATALOG, KomaConfigRegistry};
// Re-exported for `doctor.rs`, which shares the chord/merge helpers with the listing.
pub(crate) use koma_lua::{
    chord_identity, describe_action, format_chord, lua_quote, merge_binds, short_action,
};
use regex::RegexBuilder;
use serde_json::{Value, json};

use crate::config_probe;

// The effective-keymap merge is shared with the compositor's keybind overlay.
pub(crate) use koma_lua::effective_keymap::ChordKind;
pub(crate) use koma_lua::effective_keymap::EffectiveBind as BindRow;
#[cfg(test)]
pub(crate) use koma_lua::effective_keymap::chord_kind;

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
// Effective listing
// =============================================================================

fn edge_label(edge: TriggerEdge) -> &'static str {
    match edge {
        TriggerEdge::Press => "press",
        TriggerEdge::Release => "release",
        TriggerEdge::Both => "both",
    }
}

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
            let description = if row.description.is_empty() {
                String::from("-")
            } else {
                row.description.clone()
            };
            vec![
                format!("{}{mode}", row.chord),
                format!("{}{edge}{disabled}", row.action),
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
the original binding stays visible. Pointer and gesture binds are listed in
their own sections. Nothing is hidden: binds the compositor cannot translate
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

    use koma_lua::{ActionDescriptor, BindHandleId, scaffold_config_tree};

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
    fn builtin_keybind_overview_chord_is_listed() {
        let (root, registry) = listing_for("", &[]);
        let rows = merge_binds(&registry, &root.join("init.lua"), KEYBIND_CATALOG);
        let row = rows
            .iter()
            .find(|row| row.chord == "Mod + Shift + Slash")
            .expect("the overlay toggle is part of `komactl keys`");
        assert_eq!(row.description, "Show the keybind overview");
        assert_eq!(row.expression, "koma.act.overlay.keybinds_toggle()");
        assert_eq!(row.source, "builtin");
        assert_eq!(row.group, "Session");
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
