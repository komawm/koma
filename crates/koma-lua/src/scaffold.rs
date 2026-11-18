//! Generated configuration scaffold shared by `komactl init` and Koma's first-run bootstrap.
//!
//! The tree is deliberately a set of small modules loaded by a thin `init.lua`
//! router. `lua/keybinds.lua` is rendered from [`KEYBIND_CATALOG`], so `komactl init`
//! and the built-in fallback keymap cannot drift.
//!
//! Generated files carry the machine-readable marker
//! `-- koma-config-template: <version>` in their header. `komactl config-status`
//! compares that marker with [`TEMPLATE_VERSION`] to report an outdated scaffold.
//!
//! Documentation: <https://github.com/komawm/docs/blob/main/docs/LUA_API_SPEC.md>.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use crate::keybind_catalog::{KEYBIND_CATALOG, KeybindCatalogEntry};

/// Version of the shipped configuration template. Bump when the generated tree
/// changes shape so `komactl config-status` can report outdated scaffolds.
pub const TEMPLATE_VERSION: u32 = 1;

/// Machine-readable marker prefix; the full line is `-- koma-config-template: N`.
pub const TEMPLATE_MARKER: &str = "koma-config-template:";

const DOCS_LUA_API: &str = "https://github.com/komawm/docs/blob/main/docs/LUA_API_SPEC.md";
const DOCS_ARCHITECTURE: &str = "https://github.com/komawm/docs/blob/main/docs/ARCHITECTURE.md";
const DOCS_SITE: &str = "https://komawm.org";

/// A rendered configuration file and its path relative to the config root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigTemplateFile {
    pub relative_path: &'static str,
    pub contents: String,
}

/// Renders the complete configuration tree without touching the filesystem.
///
/// Time O(total template bytes), space O(total template bytes).
#[must_use]
pub fn render_config_template() -> Vec<ConfigTemplateFile> {
    vec![
        file("init.lua", render_init_lua()),
        file("lua/input.lua", render_input_lua()),
        file("lua/workspaces.lua", render_workspaces_lua()),
        file("lua/appearance.lua", render_appearance_lua()),
        file("lua/window_rules.lua", render_window_rules_lua()),
        file("lua/keybinds.lua", render_keybinds_lua()),
        file("lua/macros/example.lua", render_macro_example_lua()),
    ]
}

fn file(relative_path: &'static str, contents: String) -> ConfigTemplateFile {
    ConfigTemplateFile {
        relative_path,
        contents,
    }
}

/// Header shared by every generated file.
fn header(title: &str, docs_url: &str) -> String {
    format!(
        "-- {TEMPLATE_MARKER} {TEMPLATE_VERSION}\n\
         -- {title} (template v{TEMPLATE_VERSION}).\n\
         -- Docs: {docs_url}\n\
         -- Regenerate with `komactl init --force`; existing files are kept otherwise.\n\
         ---@diagnostic disable: undefined-global\n\n"
    )
}

fn render_init_lua() -> String {
    format!(
        "{}-- This file is a router. Each module configures one concern and lives under
-- ./lua/. Edit the modules, then validate and hot-reload them:
--
--   komactl validate
--   komactl reload
--
-- The keymap is fully expanded in lua/keybinds.lua because lua/input.lua sets
-- `inherit_default_binds = false`.
--
-- Requires are sandboxed to the configuration directory: `require(\"input\")`
-- loads `<config>/lua/input.lua`, and symlinks may not escape that directory.
-- Docs: {DOCS_SITE} and {DOCS_LUA_API}

require(\"input\")
require(\"workspaces\")
require(\"appearance\")
require(\"window_rules\")
require(\"keybinds\")

-- Example timed macro. It is generated but not loaded by default; uncomment to use.
-- require(\"macros.example\")
",
        header("Koma configuration entry point", DOCS_SITE)
    )
}

fn render_input_lua() -> String {
    format!(
        "{}-- `inherit_default_binds = false` disables Koma's fallback keymap because
-- lua/keybinds.lua lists every built-in bind explicitly. Keep the two in sync:
-- when you delete a bind there, it is gone.
--
-- `mod_key` accepts \"Super\", \"Alt\", \"Ctrl\" or \"Shift\".

koma.input({{
    mod_key = \"Super\",
    inherit_default_binds = false,
    keyboard = {{
        repeat_delay = 600,
        repeat_rate = 35,
    }},
    touchpad = {{
        tap = true,
        natural_scroll = true,
    }},
}})
",
        header(
            "Input devices, modifier key and keymap inheritance",
            DOCS_LUA_API
        )
    )
}

fn render_workspaces_lua() -> String {
    format!(
        "{}-- No `koma.monitor({{ ... }})` block is required. A monitor rule only
-- describes per-connector overrides (mode, scale, transform); outputs are used
-- as they are when no rule matches.
--
-- Workspaces inherit the global layout unless they set one explicitly.
--
-- Koma publishes an empty workspace as `hidden` over `ext-workspace-v1`, so a
-- status bar with `ignore-hidden: true` (waybar `ext/workspaces`) lists only the
-- workspaces that hold a window instead of every configured one. Add
-- `persistent = true` to keep one empty workspace visible, for example:
-- koma.workspace({{ name = \"9\", persistent = true }})

koma.layout(\"dwindle\")

for i = 1, 9 do
    koma.workspace({{
        name = tostring(i),
        presentation = \"headed\",
        layout = \"dwindle\",
    }})
end

koma.workspace({{
    name = \"special:magic\",
    presentation = \"headed\",
    layout = \"dwindle\",
    gaps = {{ inner = 10, outer = 24 }},
}})

koma.dwindle({{
    preserve_split = true,
    pseudotile = true,
}})

koma.scrolling({{
    preset_column_widths = {{ 1.0 / 3.0, 0.5, 2.0 / 3.0 }},
    preset_window_heights = {{ 1.0 / 3.0, 0.5, 2.0 / 3.0 }},
    default_column_width = 0.5,
    center_focused_column = \"on_overflow\",
}})
",
        header("Workspaces and layout tuning", DOCS_LUA_API)
    )
}

fn render_appearance_lua() -> String {
    format!(
        "{}-- Colors accept #RRGGBB or #RRGGBBAA. Animations are critically damped
-- springs by default; `koma.bezier(name, ...)` and `koma.spring(name, {{ ... }})`
-- register custom curves that `animations.*.curve` can reference.

koma.appearance({{
    corner_radius = 10,
    border = {{
        enabled = true,
        width = 2.0,
        active_color = \"#58a6ff\",
        inactive_color = \"#30363d\",
        agent_leased_color = \"#d2a8ff\",
        human_preempt_color = \"#f0883e\",
    }},
    shadow = {{
        enabled = true,
        range = 16,
        render_power = 3,
        color = \"#00000073\",
        offset_x = 0.0,
        offset_y = 4.0,
    }},
    blur = {{
        enabled = true,
        size = 6,
        passes = 3,
    }},
    animations = {{
        enabled = true,
        damping_ratio = 1.0,
        stiffness = 800.0,
    }},
}})
",
        header("Borders, shadows, blur and animations", DOCS_LUA_API)
    )
}

fn render_window_rules_lua() -> String {
    format!(
        "{}-- Rules are matched in order. `matches` and `excludes` entries are
-- regexes over `app_id`, `title`, `xwayland` or `workspace`.
--
-- `block_out_from = \"all\"` keeps credential managers out of agent captures,
-- screencasts and screencopy while still rendering them for the human user.

koma.rule.window({{
    name = \"terminal-polish\",
    matches = {{
        {{ app_id = \"^(com\\\\.mitchellh\\\\.ghostty|org\\\\.wezfurlong\\\\.wezterm|foot|run\\\\.bitty\\\\.Bitty)$\" }},
    }},
    opacity = 0.95,
    inactive_opacity = 0.9,
    corner_radius = 10,
}})

koma.rule.window({{
    name = \"protect-credential-managers\",
    matches = {{
        {{ app_id = \"^(org\\\\.keepassxc\\\\.KeePassXC|com\\\\.bitwarden\\\\.Bitwarden)$\" }},
    }},
    block_out_from = \"all\",
}})
",
        header("Window rules and protected surfaces", DOCS_ARCHITECTURE)
    )
}

fn render_keybinds_lua() -> String {
    let mut out = header("Complete built-in keymap", DOCS_LUA_API);
    out.push_str(
        "-- This is the full keymap, not a set of overrides, because input.lua sets
-- `inherit_default_binds = false`. Every built-in bind is spelled out below.
-- Change a chord, swap an action, add `cooldown_ms = ...`, or delete a line to
-- remove that binding.
--
-- Add your own binds at the end; no `koma.unbind(...)` call is needed.
",
    );

    let mut active_group = "";
    for entry in KEYBIND_CATALOG {
        if entry.group != active_group {
            active_group = entry.group;
            out.push('\n');
            out.push_str(&format!("-- {active_group}\n"));
        }
        out.push_str(&render_keybind_line(entry));
        out.push('\n');
    }
    out
}

fn render_keybind_line(entry: &KeybindCatalogEntry) -> String {
    let options = match entry.cooldown_ms {
        Some(cooldown) => format!(
            "{{ cooldown_ms = {cooldown}, desc = {} }}",
            lua_quote(entry.description)
        ),
        None => format!("{{ desc = {} }}", lua_quote(entry.description)),
    };
    format!(
        "koma.bind({}, {}, {options})",
        lua_quote(entry.chord),
        entry.lua_action
    )
}

fn render_macro_example_lua() -> String {
    format!(
        "{}-- Macros run on a timed coroutine. Host operations such as `ctx:spawn`,
-- `ctx:sleep_ms`, `ctx:key`, `ctx:text` and `ctx:drag` may suspend without
-- blocking the compositor event loop.
--
-- Bind the macro from lua/keybinds.lua:
--
--   koma.bind(\"Mod + Shift + T\",
--       koma.act.macro.run(\"example.open_terminal\"),
--       {{ desc = \"Open a terminal from a timed macro\" }})

koma.macro.define({{
    id = \"example.open_terminal\",
    app_id = \".*\",
    description = \"Example: wait 250 ms, then spawn a terminal\",
    params = {{}},
    run = function(ctx)
        ctx:sleep_ms(250)
        ctx:spawn({{ \"bitty\" }})
    end,
}})
",
        header("Example timed macro", DOCS_LUA_API)
    )
}

fn lua_quote(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            other => quoted.push(other),
        }
    }
    quoted.push('"');
    quoted
}

/// Scaffold failure; `--force` is required to replace generated files.
#[derive(Debug, thiserror::Error)]
pub enum ScaffoldError {
    /// At least one target file already exists and `force` was not requested.
    #[error(
        "refusing to overwrite existing file(s): {existing}\n\
         re-run with `komactl init --force` to replace the generated template"
    )]
    Existing { existing: String },
    /// Filesystem failure while creating the tree.
    #[error("failed to {action} `{path}`: {source}")]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// Writes the rendered configuration tree under `root`.
///
/// Without `force`, any existing target file aborts the whole operation before
/// anything is written, and each file is then created with `create_new`. With
/// `force`, generated files are truncated and rewritten. Returns the absolute
/// path of every written file.
///
/// Time O(total template bytes), space O(total template bytes).
pub fn scaffold_config_tree(root: &Path, force: bool) -> Result<Vec<PathBuf>, ScaffoldError> {
    let files = render_config_template();
    let targets: Vec<(PathBuf, String)> = files
        .into_iter()
        .map(|file| (root.join(file.relative_path), file.contents))
        .collect();

    if !force {
        let existing: Vec<String> = targets
            .iter()
            .filter(|(path, _)| path.exists())
            .map(|(path, _)| format!("`{}`", path.display()))
            .collect();
        if !existing.is_empty() {
            return Err(ScaffoldError::Existing {
                existing: existing.join(", "),
            });
        }
    }

    let mut written = Vec::with_capacity(targets.len());
    for (path, contents) in targets {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| ScaffoldError::Io {
                action: "create directory",
                path: parent.to_path_buf(),
                source,
            })?;
        }
        if force {
            fs::write(&path, contents).map_err(|source| ScaffoldError::Io {
                action: "write",
                path: path.clone(),
                source,
            })?;
        } else {
            use std::io::Write as _;
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .map_err(|source| ScaffoldError::Io {
                    action: "create",
                    path: path.clone(),
                    source,
                })?;
            file.write_all(contents.as_bytes())
                .map_err(|source| ScaffoldError::Io {
                    action: "write",
                    path: path.clone(),
                    source,
                })?;
        }
        written.push(path);
    }
    Ok(written)
}

/// Extracts the template version marker from Lua source, if present.
///
/// Only the first ten lines are considered so a user comment deeper in the file
/// cannot shadow the header.
#[must_use]
pub fn template_version_in(source: &str) -> Option<u32> {
    source.lines().take(10).find_map(|line| {
        line.trim()
            .strip_prefix("--")?
            .trim()
            .strip_prefix(TEMPLATE_MARKER)?
            .trim()
            .parse()
            .ok()
    })
}

/// Reads the template marker from an existing `init.lua`.
///
/// Returns `Ok(None)` when the file exists but has no marker (hand-written or
/// pre-template configuration), and `Err` only for I/O failures.
pub fn installed_template_version(init_lua: &Path) -> io::Result<Option<u32>> {
    let source = fs::read_to_string(init_lua)?;
    Ok(template_version_in(&source))
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU32, Ordering},
    };

    use super::*;

    static NEXT_ID: AtomicU32 = AtomicU32::new(0);

    fn temp_root(label: &str) -> PathBuf {
        let base = Path::new("/tmp/koma");
        fs::create_dir_all(base).expect("create /tmp/koma");
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let dir = base.join(format!("scaffold-{label}-{}-{id}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn rendered_tree_contains_every_expected_file() {
        let files = render_config_template();
        let paths: Vec<&str> = files.iter().map(|file| file.relative_path).collect();
        assert_eq!(
            paths,
            vec![
                "init.lua",
                "lua/input.lua",
                "lua/workspaces.lua",
                "lua/appearance.lua",
                "lua/window_rules.lua",
                "lua/keybinds.lua",
                "lua/macros/example.lua",
            ]
        );
        for file in &files {
            assert!(
                file.contents.contains(TEMPLATE_MARKER),
                "{} is missing the template version marker",
                file.relative_path
            );
            assert!(
                file.contents.contains("https://"),
                "{} is missing a documentation URL",
                file.relative_path
            );
        }
    }

    #[test]
    fn keybinds_module_is_the_full_catalog_expansion() {
        let files = render_config_template();
        let keybinds = files
            .iter()
            .find(|file| file.relative_path == "lua/keybinds.lua")
            .expect("keybinds module");
        let bind_lines = keybinds
            .contents
            .lines()
            .filter(|line| line.starts_with("koma.bind("))
            .count();
        assert_eq!(
            bind_lines,
            KEYBIND_CATALOG.len(),
            "keybinds.lua must list every catalog entry"
        );
        for entry in KEYBIND_CATALOG {
            assert!(
                keybinds.contents.contains(entry.chord),
                "keybinds.lua is missing `{}`",
                entry.chord
            );
            assert!(
                keybinds.contents.contains(entry.lua_action),
                "keybinds.lua is missing the action for `{}`",
                entry.chord
            );
        }
    }

    #[test]
    fn input_module_disables_inherited_binds_and_router_requires_every_module() {
        let files = render_config_template();
        let input = files
            .iter()
            .find(|file| file.relative_path == "lua/input.lua")
            .expect("input module");
        assert!(input.contents.contains("inherit_default_binds = false"));
        let init = files
            .iter()
            .find(|file| file.relative_path == "init.lua")
            .expect("init module");
        for module in [
            "require(\"input\")",
            "require(\"workspaces\")",
            "require(\"appearance\")",
            "require(\"window_rules\")",
            "require(\"keybinds\")",
        ] {
            assert!(
                init.contents.contains(module),
                "init.lua is missing {module}"
            );
        }
        assert!(
            !files.iter().any(|file| file
                .contents
                .lines()
                .map(str::trim_start)
                .any(|line| !line.starts_with("--") && line.contains("koma.monitor("))),
            "the scaffold must not declare a monitor block"
        );
    }

    #[test]
    fn scaffold_refuses_existing_files_without_force() {
        let root = temp_root("refuse");
        let written = scaffold_config_tree(&root, false).expect("fresh scaffold");
        assert_eq!(written.len(), 7);

        let err = scaffold_config_tree(&root, false).expect_err("second run must refuse");
        let message = err.to_string();
        assert!(message.contains("refusing to overwrite"), "{message}");
        assert!(message.contains("--force"), "{message}");

        let with_force = scaffold_config_tree(&root, true).expect("force rewrite");
        assert_eq!(with_force.len(), 7);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scaffold_aborts_before_writing_when_one_target_exists() {
        let root = temp_root("partial");
        fs::create_dir_all(root.join("lua")).expect("create lua dir");
        fs::write(root.join("lua/input.lua"), "-- user file\n").expect("seed user file");

        let err = scaffold_config_tree(&root, false).expect_err("must refuse");
        assert!(err.to_string().contains("lua/input.lua"));
        assert!(
            !root.join("init.lua").exists(),
            "no file may be written when the operation is refused"
        );
        assert_eq!(
            fs::read_to_string(root.join("lua/input.lua")).unwrap(),
            "-- user file\n",
            "existing content must be preserved"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn template_version_marker_round_trips() {
        let root = temp_root("version");
        scaffold_config_tree(&root, false).expect("scaffold");
        assert_eq!(
            installed_template_version(&root.join("init.lua")).unwrap(),
            Some(TEMPLATE_VERSION)
        );
        assert_eq!(template_version_in("-- koma-config-template: 7\n"), Some(7));
        assert_eq!(template_version_in("-- no marker here\n"), None);
        assert_eq!(
            template_version_in(&"-- filler\n".repeat(20)),
            None,
            "markers beyond the header are ignored"
        );
        let _ = fs::remove_dir_all(&root);
    }
}
