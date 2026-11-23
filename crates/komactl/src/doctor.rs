//! `komactl doctor` — non-destructive configuration diagnostics.
//!
//! `doctor` reuses the offline Shadow-VM evaluation of `komactl validate` and adds the checks that
//! `validate` cannot perform: template freshness, `require()` targets that do not exist, table keys
//! the schema silently ignores, binds that shadow a built-in chord, monitor/workspace output names
//! the machine does not report, runtime-directory and admin-socket reachability, and whether the
//! running binary's template generation matches the configuration's.
//!
//! It is a diagnostic, not a gate: a configuration with a syntax error is reported as `error` and
//! still leaves the exit code at `0`, because the compositor keeps running on its last valid
//! configuration. `--strict` turns any `warn` or `error` into exit code `1` for CI use.

use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

use koma_lua::schema::TriggerEdge;
use koma_lua::{
    ActionDescriptor, BindSpec, DEFAULT_HEAP_LIMIT_BYTES, KEYBIND_CATALOG, KomaConfigRegistry,
    TEMPLATE_VERSION, phodopus_bridge::PhodopusSandbox,
};
use koma_proto::overview::{OverviewPaths, OverviewProbe, probe_overview_daemon};
use serde_json::{Value, json};

use crate::{
    config_probe::{self, ConfigEvaluation, LiveProbe},
    keys::{chord_identity, format_chord, lua_quote, merge_binds, short_action},
    lua_scan,
};

/// Severity of one diagnostic row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Severity {
    /// The check ran and found nothing to report.
    Ok,
    /// The configuration will still load, but something is probably a mistake.
    Warn,
    /// The configuration is broken; Koma keeps its last valid configuration.
    Error,
    /// The check could not run in this environment.
    Skip,
}

impl Severity {
    #[must_use]
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Error => "error",
            Self::Skip => "skip",
        }
    }

    fn as_json(self) -> &'static str {
        self.label()
    }
}

/// One diagnostic row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Finding {
    /// Grouping used by the human renderer (`config`, `template`, `binds`, ...).
    pub area: &'static str,
    /// Stable check identifier (`config.syntax`, `bind.shadow-builtin`, ...).
    pub check: &'static str,
    pub severity: Severity,
    pub message: String,
}

/// Where the set of connected outputs comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outputs {
    /// Connector names reported by the kernel (`/sys/class/drm/*/status == connected`).
    Reported(Vec<String>),
    /// The environment exposes no connector status (containers, non-DRM hosts).
    Unavailable(String),
}

impl Outputs {
    #[must_use]
    pub(crate) fn detect() -> Self {
        let Ok(entries) = fs::read_dir("/sys/class/drm") else {
            return Self::Unavailable("/sys/class/drm is not available".to_owned());
        };
        let mut connectors = BTreeSet::new();
        let mut saw_connector = false;
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some((_, connector)) = name.split_once('-') else {
                continue;
            };
            let Ok(status) = fs::read_to_string(entry.path().join("status")) else {
                continue;
            };
            saw_connector = true;
            if status.trim() == "connected" {
                connectors.insert(connector.to_owned());
            }
        }
        if !saw_connector {
            return Self::Unavailable(
                "/sys/class/drm exposes no connector status; run doctor on the display host"
                    .to_owned(),
            );
        }
        Self::Reported(connectors.into_iter().collect())
    }
}

/// Inputs that make a diagnosis hermetic enough to unit test.
#[derive(Debug, Clone)]
pub(crate) struct DoctorContext {
    pub outputs: Outputs,
    pub live: LiveProbe,
    /// Control socket, pid file and respawn bookkeeping of the session's `koma-overview` daemon.
    ///
    /// `None` when no Koma instance runtime directory could be resolved at all; tests pass explicit
    /// paths so the check is hermetic.
    pub overview: Option<OverviewPaths>,
    /// Program lookup roots for `bind.spawn-*`; `None` reads the process `PATH`.
    pub spawn_path: Option<Vec<PathBuf>>,
}

impl DoctorContext {
    #[must_use]
    pub(crate) fn detect() -> Self {
        let live = config_probe::probe_live();
        let admin_socket = match &live {
            LiveProbe::Reachable { admin_socket, .. }
            | LiveProbe::Unreachable { admin_socket, .. } => Some(admin_socket.clone()),
            LiveProbe::Offline { .. } => None,
        };
        let overview = crate::admin_rpc::discover_overview_paths(admin_socket.as_deref());
        Self {
            outputs: Outputs::detect(),
            live,
            overview,
            spawn_path: None,
        }
    }

    /// Directories searched for a bare program name, honoring `koma.env("PATH", ...)` when
    /// `spawn_path` is not explicitly overridden by a test.
    fn effective_program_paths(
        &self,
        registry: &KomaConfigRegistry,
        config_root: &Path,
    ) -> Vec<PathBuf> {
        if let Some(dirs) = &self.spawn_path {
            return dirs.clone();
        }
        let mut dirs = Vec::new();
        if let Some(cfg_path) = registry.env_vars.get("PATH") {
            let resolved = koma_lua::path_resolve::resolve_env_value_from_env(
                "PATH",
                cfg_path,
                Some(config_root),
            );
            for dir in env::split_paths(&resolved) {
                if !dir.as_os_str().is_empty() && !dirs.contains(&dir) {
                    dirs.push(dir);
                }
            }
        }
        if let Some(proc_path) = env::var_os("PATH") {
            for dir in env::split_paths(&proc_path) {
                if !dir.as_os_str().is_empty() && !dirs.contains(&dir) {
                    dirs.push(dir);
                }
            }
        }
        dirs
    }
}

/// Complete diagnostic report.
#[derive(Debug, Clone)]
pub(crate) struct Report {
    pub config_path: PathBuf,
    pub findings: Vec<Finding>,
    /// `false` when the configuration file could not be read at all.
    pub readable: bool,
    pub strict: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Counts {
    pub ok: usize,
    pub warn: usize,
    pub error: usize,
    pub skipped: usize,
}

impl Counts {
    #[must_use]
    pub(crate) fn total(self) -> usize {
        self.ok + self.warn + self.error + self.skipped
    }
}

impl Report {
    #[must_use]
    pub(crate) fn counts(&self) -> Counts {
        let mut counts = Counts::default();
        for finding in &self.findings {
            match finding.severity {
                Severity::Ok => counts.ok += 1,
                Severity::Warn => counts.warn += 1,
                Severity::Error => counts.error += 1,
                Severity::Skip => counts.skipped += 1,
            }
        }
        counts
    }

    #[must_use]
    pub(crate) fn has_error(&self) -> bool {
        self.findings
            .iter()
            .any(|finding| finding.severity == Severity::Error)
    }

    #[must_use]
    pub(crate) fn has_warn(&self) -> bool {
        self.findings
            .iter()
            .any(|finding| finding.severity == Severity::Warn)
    }

    /// Exit status: `0` unless `--strict`, `1` under `--strict` with any `warn` or `error`.
    #[must_use]
    pub(crate) fn exit_code(&self) -> i32 {
        if self.strict && (self.has_error() || self.has_warn()) {
            1
        } else {
            0
        }
    }

    fn push(
        &mut self,
        area: &'static str,
        check: &'static str,
        severity: Severity,
        message: String,
    ) {
        self.findings.push(Finding {
            area,
            check,
            severity,
            message,
        });
    }
}

const AREA_ORDER: &[&str] = &[
    "config",
    "template",
    "binds",
    "monitors",
    "workspaces",
    "rules",
    "environment",
];

fn collapse(text: &str) -> String {
    let first_line = text.lines().next().unwrap_or_default().trim();
    let extra = text.lines().count() > 1;
    if extra {
        format!("{first_line} ...")
    } else {
        first_line.to_owned()
    }
}

// =============================================================================
// Schema key whitelist
// =============================================================================

/// `koma.*` calls whose literal table argument is checked for unknown keys.
const CHECKED_CALLS: &[(&str, usize)] = &[
    ("monitor", 0),
    ("workspace", 0),
    ("dwindle", 0),
    ("scrolling", 0),
    ("input", 0),
    ("device", 0),
    ("appearance", 0),
    ("xwayland", 0),
    ("gesture", 0),
    ("bind", 2),
];

fn serialized_keys<T: serde::Serialize + Default>(aliases: &[&str]) -> BTreeSet<String> {
    let value = serde_json::to_value(T::default()).unwrap_or(Value::Null);
    let mut keys: BTreeSet<String> = value
        .as_object()
        .map(|map| map.keys().cloned().collect())
        .unwrap_or_default();
    keys.extend(aliases.iter().map(|alias| (*alias).to_owned()));
    keys
}

fn checked_call_whitelist() -> BTreeMap<&'static str, BTreeSet<String>> {
    let mut map = BTreeMap::new();
    map.insert(
        "monitor",
        serialized_keys::<koma_lua::MonitorSpec>(&["non_desktop"]),
    );
    map.insert("workspace", serialized_keys::<koma_lua::WorkspaceSpec>(&[]));
    map.insert("dwindle", serialized_keys::<koma_lua::DwindleConfig>(&[]));
    map.insert(
        "scrolling",
        serialized_keys::<koma_lua::ScrollingConfig>(&[]),
    );
    map.insert("input", serialized_keys::<koma_lua::InputConfigSpec>(&[]));
    map.insert(
        "device",
        serialized_keys::<koma_lua::DeviceOverrideSpec>(&[
            "tap_to_click",
            "disable_while_typing",
            "disable_while_trackpointing",
        ]),
    );
    map.insert(
        "appearance",
        serialized_keys::<koma_lua::AppearanceSpec>(&[]),
    );
    map.insert(
        "xwayland",
        serialized_keys::<koma_lua::XwaylandConfigSpec>(&[]),
    );
    map.insert("gesture", gesture_keys());
    map.insert("bind", serialized_keys::<koma_lua::BindOptions>(&[]));
    map
}

fn gesture_keys() -> BTreeSet<String> {
    // `GestureSpec` has no `Default` (fingers/direction/action are required), so the keys the
    // bridge reads are listed explicitly.
    [
        "id",
        "enabled",
        "fingers",
        "direction",
        "action",
        "cooldown_ms",
        "allow_when_locked",
        "when_preempted",
        "desc",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

// =============================================================================
// Catalogue actions (for shadow detection)
// =============================================================================

/// Evaluates every catalogue entry in the sandbox so shadow detection compares real
/// [`ActionDescriptor`] values instead of duplicating the built-in action table.
fn catalogue_actions(
    mod_key: Option<&str>,
) -> Result<BTreeMap<String, (ActionDescriptor, &'static str)>, String> {
    let mut source = String::new();
    for entry in KEYBIND_CATALOG {
        source.push_str(&format!(
            "koma.bind({}, {}, {{ desc = {} }})\n",
            lua_quote(entry.chord),
            entry.lua_action,
            lua_quote(entry.description)
        ));
    }
    let mut vm = PhodopusSandbox::new(DEFAULT_HEAP_LIMIT_BYTES, None)?;
    let registry = vm.eval_init_lua(&source)?;
    let mut actions = BTreeMap::new();
    for bind in registry.binds {
        let description = KEYBIND_CATALOG
            .iter()
            .find(|entry| entry.chord.eq_ignore_ascii_case(&bind.chord))
            .map_or("", |entry| entry.description);
        actions.insert(
            chord_identity(&bind.chord, mod_key),
            (bind.action, description),
        );
    }
    Ok(actions)
}

// =============================================================================
// Individual checks
// =============================================================================

fn check_template(report: &mut Report, config_path: &Path, detail: &Value) {
    let installed = detail.get("installed_version").and_then(Value::as_u64);
    let present = detail
        .get("present")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let missing: Vec<&str> = detail
        .get("missing_files")
        .and_then(Value::as_array)
        .map(|files| files.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();

    match installed {
        Some(version) if version < u64::from(TEMPLATE_VERSION) => report.push(
            "template",
            "template.version",
            Severity::Warn,
            format!(
                "configuration template v{version} is older than the shipped v{TEMPLATE_VERSION}; \
                 review the generated tree and refresh it with `komactl init --force`"
            ),
        ),
        Some(version) if version > u64::from(TEMPLATE_VERSION) => report.push(
            "template",
            "template.version",
            Severity::Warn,
            format!(
                "configuration template v{version} is newer than this binary's v{TEMPLATE_VERSION} \
                 (komactl {}); the running compositor may be too old for it",
                env!("CARGO_PKG_VERSION")
            ),
        ),
        Some(version) => report.push(
            "template",
            "template.version",
            Severity::Ok,
            format!("template v{version} matches the shipped v{TEMPLATE_VERSION}"),
        ),
        None if present => report.push(
            "template",
            "template.version",
            Severity::Ok,
            "hand-written configuration without a template marker; nothing to refresh".to_owned(),
        ),
        None => report.push(
            "template",
            "template.version",
            Severity::Error,
            format!("{} does not exist", config_path.display()),
        ),
    }

    if installed.is_some() {
        if missing.is_empty() {
            report.push(
                "template",
                "template.files",
                Severity::Ok,
                "every generated file is present".to_owned(),
            );
        } else {
            report.push(
                "template",
                "template.files",
                Severity::Warn,
                format!(
                    "{} generated file(s) missing: {} (regenerate with `komactl init --force`)",
                    missing.len(),
                    missing.join(", ")
                ),
            );
        }
    }
}

fn check_require_targets(evaluation: &ConfigEvaluation, report: &mut Report) {
    let lua_root = evaluation.config_root.join("lua");
    let mut sources: Vec<(PathBuf, String)> =
        vec![(evaluation.path.clone(), evaluation.source.clone())];
    collect_lua_sources(&lua_root, &mut sources);

    let mut missing: Vec<String> = Vec::new();
    for (path, source) in &sources {
        for target in lua_scan::require_targets(&lua_scan::tokenize(source)) {
            let label = if *path == evaluation.path {
                "init.lua".to_owned()
            } else {
                path.file_name().map_or_else(
                    || path.display().to_string(),
                    |name| name.to_string_lossy().into_owned(),
                )
            };
            let module = match koma_lua::path_resolve::resolve_require_target_from_env(
                &target.module,
                Some(&evaluation.config_root),
            ) {
                Ok(resolved) => resolved,
                Err(err) => {
                    missing.push(format!(
                        "`require(\"{}\")` in {label}:{}: {err}",
                        target.module, target.line
                    ));
                    continue;
                }
            };
            let module_path = module.replace('.', "/");
            let flat = lua_root.join(format!("{module_path}.lua"));
            let nested = lua_root.join(&module_path).join("init.lua");
            if flat.is_file() || nested.is_file() {
                continue;
            }
            missing.push(format!(
                "`require(\"{}\")` in {label}:{} has no lua/{module_path}.lua",
                target.module, target.line
            ));
        }
    }

    if missing.is_empty() {
        report.push(
            "config",
            "config.require",
            Severity::Ok,
            "every require() target under lua/ exists".to_owned(),
        );
    } else {
        for message in missing {
            report.push("config", "config.require", Severity::Warn, message);
        }
    }
}

fn collect_lua_sources(dir: &Path, out: &mut Vec<(PathBuf, String)>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "lua"))
        .collect();
    paths.sort();
    for path in paths {
        if let Ok(source) = fs::read_to_string(&path) {
            out.push((path, source));
        }
    }
}

fn check_unknown_keys(evaluation: &ConfigEvaluation, report: &mut Report) {
    let calls: Vec<&str> = CHECKED_CALLS.iter().map(|(call, _)| *call).collect();
    let keys = lua_scan::table_keys_for_calls(&lua_scan::tokenize(&evaluation.source), &calls);
    let whitelist = checked_call_whitelist();
    let mut unknown = Vec::new();
    for key in keys {
        let Some((_, arg_index)) = CHECKED_CALLS.iter().find(|(call, _)| *call == key.call) else {
            continue;
        };
        if key.arg_index != *arg_index {
            continue;
        }
        let Some(allowed) = whitelist.get(key.call.as_str()) else {
            continue;
        };
        if allowed.contains(&key.key) {
            continue;
        }
        unknown.push(format!(
            "koma.{}: unknown key `{}` on line {} (the schema ignores it)",
            key.call, key.key, key.line
        ));
    }

    if unknown.is_empty() {
        report.push(
            "config",
            "config.unknown-key",
            Severity::Ok,
            format!(
                "no unknown keys across {} checked koma.* tables",
                CHECKED_CALLS.len()
            ),
        );
    } else {
        for message in unknown {
            report.push("config", "config.unknown-key", Severity::Warn, message);
        }
    }
}

fn check_bind_chords(registry: &KomaConfigRegistry, report: &mut Report) {
    let mut invalid = 0usize;
    for spec in &registry.binds {
        let tokens: Vec<&str> = spec
            .chord
            .split('+')
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .collect();
        let Some((trigger, modifiers)) = tokens.split_last() else {
            report.push(
                "binds",
                "bind.chord",
                Severity::Error,
                "a bind has an empty chord".to_owned(),
            );
            invalid += 1;
            continue;
        };
        if modifier_only_chord(&tokens) {
            report.push(
                "binds",
                "bind.chord",
                Severity::Error,
                format!("bind `{}` has no key after its modifiers", spec.chord),
            );
            invalid += 1;
            continue;
        }
        if trigger.is_empty() {
            report.push(
                "binds",
                "bind.chord",
                Severity::Error,
                format!("bind `{}` has no key after its modifiers", spec.chord),
            );
            invalid += 1;
            continue;
        }
        for modifier in modifiers {
            if canonical_modifier_name(modifier).is_none() {
                report.push(
                    "binds",
                    "bind.chord",
                    Severity::Error,
                    format!(
                        "bind `{}` uses unknown modifier `{modifier}` \
                         (valid: Mod, Super, Ctrl, Shift, Alt, Mod5)",
                        spec.chord
                    ),
                );
                invalid += 1;
            }
        }
    }
    if invalid == 0 {
        report.push(
            "binds",
            "bind.chord",
            Severity::Ok,
            format!(
                "{} bind chord(s) use valid modifier tokens",
                registry.binds.len()
            ),
        );
    }
}

fn canonical_modifier_name(token: &str) -> Option<&'static str> {
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

/// `true` when the chord lists only modifiers (`Mod + Shift`), so the trigger token is missing.
fn modifier_only_chord(tokens: &[&str]) -> bool {
    tokens.len() == 1 && canonical_modifier_name(tokens[0]).is_some()
}

fn check_unbinds(registry: &KomaConfigRegistry, report: &mut Report) {
    if registry.unbinds.is_empty() {
        report.push(
            "binds",
            "bind.unbind",
            Severity::Ok,
            "no koma.unbind entries".to_owned(),
        );
        return;
    }
    let mod_key = registry.input.mod_key.as_deref();
    for raw in &registry.unbinds {
        let identity = chord_identity(raw, mod_key);
        let catalogue = KEYBIND_CATALOG
            .iter()
            .find(|entry| chord_identity(entry.chord, mod_key) == identity);
        let config_bound = registry
            .binds
            .iter()
            .any(|bind| chord_identity(&bind.chord, mod_key) == identity);
        if let Some(entry) = catalogue {
            report.push(
                "binds",
                "bind.unbind",
                Severity::Ok,
                format!(
                    "`{}` removes the built-in binding `{}`",
                    format_chord(raw),
                    entry.description
                ),
            );
        } else if !config_bound {
            report.push(
                "binds",
                "bind.unbind",
                Severity::Warn,
                format!(
                    "`{}` matches no built-in chord or config bind, so it has no effect",
                    format_chord(raw)
                ),
            );
        }
    }
}

fn check_duplicate_binds(registry: &KomaConfigRegistry, report: &mut Report) {
    let mod_key = registry.input.mod_key.as_deref();
    let mut groups: BTreeMap<(Option<String>, String, String), Vec<&BindSpec>> = BTreeMap::new();
    for spec in &registry.binds {
        let key = (
            spec.mode.clone(),
            format!("{:?}", spec.options.edge),
            chord_identity(&spec.chord, mod_key),
        );
        groups.entry(key).or_default().push(spec);
    }
    let duplicates: Vec<_> = groups.values().filter(|specs| specs.len() > 1).collect();
    if duplicates.is_empty() {
        report.push(
            "binds",
            "bind.duplicate",
            Severity::Ok,
            "no duplicate chord, mode, and edge combinations".to_owned(),
        );
    } else {
        for specs in duplicates {
            let scope = specs[0]
                .mode
                .as_deref()
                .map_or_else(|| "global".to_owned(), |mode| format!("mode `{mode}`"));
            let edge = match specs[0].options.edge {
                TriggerEdge::Press => "press",
                TriggerEdge::Release => "release",
                TriggerEdge::Both => "press and release",
            };
            report.push(
                "binds",
                "bind.duplicate",
                Severity::Warn,
                format!(
                    "`{}` is bound {} times for {scope} ({edge}); the last binding wins",
                    format_chord(&specs[0].chord),
                    specs.len()
                ),
            );
        }
    }
}

fn check_shadowed_builtins(registry: &KomaConfigRegistry, report: &mut Report) {
    let mod_key = registry.input.mod_key.as_deref();
    let catalogue = match catalogue_actions(mod_key) {
        Ok(catalogue) => catalogue,
        Err(err) => {
            report.push(
                "binds",
                "bind.shadow-builtin",
                Severity::Skip,
                format!("could not evaluate the built-in catalogue: {err}"),
            );
            return;
        }
    };

    let mut shadows = 0usize;
    let mut identical = 0usize;
    for spec in &registry.binds {
        let identity = chord_identity(&spec.chord, mod_key);
        let Some((builtin_action, description)) = catalogue.get(&identity) else {
            continue;
        };
        if builtin_action == &spec.action {
            identical += 1;
            continue;
        }
        shadows += 1;
        report.push(
            "binds",
            "bind.shadow-builtin",
            Severity::Warn,
            format!(
                "config bind `{}` replaces built-in `{description}` (`{}`) with `{}`",
                format_chord(&spec.chord),
                short_action(&crate::keys::describe_action(builtin_action)),
                short_action(&crate::keys::describe_action(&spec.action))
            ),
        );
    }
    if shadows == 0 && identical == 0 {
        report.push(
            "binds",
            "bind.shadow-builtin",
            Severity::Ok,
            "no config bind shadows a built-in chord".to_owned(),
        );
    } else if shadows == 0 {
        report.push(
            "binds",
            "bind.shadow-builtin",
            Severity::Ok,
            format!("{identical} config bind(s) repeat a built-in chord with the same action"),
        );
    }
}

// =============================================================================
// Spawn target resolution
// =============================================================================

/// Shells that put the real program in a `-c` command string.
const COMMAND_SHELLS: &[&str] = &["sh", "bash", "dash", "zsh", "ksh", "mksh", "fish"];

/// Wrappers that exec a program named in their own arguments.
const SPAWN_WRAPPERS: &[&str] = &[
    "env", "sudo", "doas", "pkexec", "busybox", "nohup", "timeout", "xargs",
];

/// Shell syntax that is not a program: POSIX sh keywords/builtins plus fish builtins.
const SHELL_WORDS: &[&str] = &[
    "!",
    ".",
    ":",
    "[",
    "abbr",
    "alias",
    "and",
    "begin",
    "bg",
    "bind",
    "block",
    "break",
    "builtin",
    "case",
    "cd",
    "command",
    "contains",
    "continue",
    "count",
    "do",
    "done",
    "echo",
    "elif",
    "else",
    "emit",
    "end",
    "esac",
    "eval",
    "exec",
    "exit",
    "export",
    "false",
    "fg",
    "fi",
    "fish_add_path",
    "for",
    "function",
    "functions",
    "getopts",
    "hash",
    "if",
    "in",
    "jobs",
    "kill",
    "let",
    "local",
    "math",
    "not",
    "or",
    "path",
    "printf",
    "pwd",
    "random",
    "read",
    "readonly",
    "return",
    "set",
    "shift",
    "source",
    "status",
    "string",
    "switch",
    "test",
    "then",
    "time",
    "times",
    "trap",
    "true",
    "type",
    "ulimit",
    "umask",
    "unalias",
    "unset",
    "until",
    "wait",
    "while",
];

/// Characters that make a token depend on expansion rather than on a literal program.
const EXPANSION_CHARS: &[char] = &['$', '*', '?', '[', ']', '~', '{', '}', '"', '\'', '`'];

/// Shell operators that turn a command string into a compound statement.
const SHELL_OPERATORS: &[char] = &['|', ';', '&', '<', '>', '`', '(', ')', '{', '}', '\n'];

/// How a bare name that is not on `PATH` is interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BareName {
    /// `koma.act.spawn` calls `execve`, so a missing name is a dead bind.
    Execve,
    /// A word inside a `fish -c` string may be a fish function or alias.
    FishWord,
    /// A word inside another shell's `-c` string; only `PATH` can be trusted offline.
    ShellWord,
}

/// Outcome of resolving one program token without executing anything.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Resolution {
    /// An executable file at this path.
    Executable(PathBuf),
    /// The program is unusable; the clause completes ``` `chord` spawns `program`, <clause> ```.
    Missing(String),
    /// Doctor cannot decide offline; the clause explains why.
    Skip(String),
}

/// First program-like word of a `-c` command string.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ShellWord {
    Program(String),
    Skip(String),
}

/// One effective spawn bind after the built-in/config merge.
struct SpawnBind {
    /// Chord label, including a mode suffix when the bind is mode-scoped.
    chord: String,
    argv: Vec<String>,
    /// `true` when the bind comes from the built-in catalogue.
    builtin: bool,
}

fn is_executable_file(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

fn program_name(program: &str) -> &str {
    program.rsplit('/').next().unwrap_or(program)
}

/// Resolves `token` the way `execve` would (plus `~` and config-relative paths), without running anything.
fn resolve_program(
    token: &str,
    path_dirs: &[PathBuf],
    bare: BareName,
    config_root: &Path,
) -> Resolution {
    let token = token.trim();
    if token.is_empty() {
        return Resolution::Skip("is empty".to_owned());
    }
    if token.starts_with('-') {
        return Resolution::Skip(format!("is the option `{token}`, not a program"));
    }
    let resolved_token = if token == "~" || token.starts_with("~/") {
        if let Some(ch) = token[1..].chars().find(|ch| EXPANSION_CHARS.contains(ch)) {
            return Resolution::Skip(format!(
                "needs shell expansion (`{ch}`), which doctor does not perform"
            ));
        }
        let expanded = koma_lua::path_resolve::expand_tilde_from_env(token);
        if expanded.starts_with('~') {
            return Resolution::Skip("uses `~` and `HOME` is not set".to_owned());
        }
        expanded
    } else {
        if let Some(ch) = token.chars().find(|ch| EXPANSION_CHARS.contains(ch)) {
            return Resolution::Skip(format!(
                "needs shell expansion (`{ch}`), which doctor does not perform"
            ));
        }
        token.to_owned()
    };

    if resolved_token.contains('/') {
        let path = koma_lua::path_resolve::resolve_config_path_from_env(
            &resolved_token,
            Some(config_root),
        );
        if is_executable_file(&path) {
            return Resolution::Executable(path);
        }
        if path.exists() {
            return Resolution::Missing("exists but is not executable".to_owned());
        }
        return Resolution::Missing("is not installed".to_owned());
    }

    let mut existing = None;
    for dir in path_dirs {
        if dir.as_os_str().is_empty() || !dir.is_absolute() {
            continue;
        }
        let candidate = dir.join(&resolved_token);
        if is_executable_file(&candidate) {
            return Resolution::Executable(candidate);
        }
        if existing.is_none() && candidate.exists() {
            existing = Some(candidate);
        }
    }
    if let Some(path) = existing {
        return Resolution::Missing(format!(
            "exists at {} but is not executable",
            path.display()
        ));
    }
    match bare {
        BareName::Execve | BareName::ShellWord => Resolution::Missing("is not on PATH".to_owned()),
        BareName::FishWord => Resolution::Skip(
            "is not an executable on PATH (it may be a fish function or alias, which doctor does \
             not run a shell to confirm)"
                .to_owned(),
        ),
    }
}

/// `Some(command)` when `argv` is `<shell> -c <command>` (also `-lc`, `-ec`, ...).
fn command_string(argv: &[String]) -> Option<&str> {
    let program = program_name(argv.first()?);
    if !COMMAND_SHELLS.contains(&program) {
        return None;
    }
    let flag = argv.get(1)?;
    let reads_command = flag.starts_with('-')
        && !flag.starts_with("--")
        && flag.chars().skip(1).any(|ch| ch == 'c');
    if !reads_command {
        return None;
    }
    argv.get(2).map(String::as_str)
}

fn is_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && name
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_alphabetic() || ch == '_')
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

/// Extracts the first program word of a shell command string, skipping assignments.
fn first_shell_word(command: &str) -> ShellWord {
    let command = command.trim();
    if command.is_empty() {
        return ShellWord::Skip("is empty".to_owned());
    }
    if let Some(op) = command.chars().find(|ch| SHELL_OPERATORS.contains(ch)) {
        return ShellWord::Skip(format!(
            "uses the shell operator `{op}`, so its program is part of a compound command"
        ));
    }
    let mut rest = command;
    loop {
        let (word, tail) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        if !is_assignment(word) {
            return classify_shell_word(word);
        }
        let tail = tail.trim_start();
        if tail.is_empty() {
            return ShellWord::Skip(
                "only assigns environment variables and runs no program".to_owned(),
            );
        }
        rest = tail;
    }
}

fn classify_shell_word(word: &str) -> ShellWord {
    let check_slice = if word == "~" || word.starts_with("~/") {
        &word[1..]
    } else {
        word
    };
    if let Some(ch) = check_slice.chars().find(|ch| EXPANSION_CHARS.contains(ch)) {
        return ShellWord::Skip(if ch == '$' && word.starts_with('$') {
            format!("starts with the variable `{word}`, which doctor does not expand")
        } else if ch == '"' || ch == '\'' {
            "starts with a quoted word, which doctor does not parse".to_owned()
        } else {
            format!("needs shell expansion (`{ch}`), which doctor does not perform")
        });
    }
    let name = program_name(word);
    if SHELL_WORDS.contains(&name) {
        return ShellWord::Skip(format!(
            "starts with `{name}`, a shell builtin or keyword rather than a program"
        ));
    }
    if SPAWN_WRAPPERS.contains(&name) {
        return ShellWord::Skip(format!(
            "starts with `{name}`, which wraps the real program in its own arguments"
        ));
    }
    if word.starts_with('-') || word.contains('=') {
        return ShellWord::Skip(format!("starts with `{word}`, which is not a program name"));
    }
    ShellWord::Program(word.to_owned())
}

/// Truncated, quote-safe rendering of a shell command string for messages.
fn shell_snippet(command: &str) -> String {
    let trimmed = command.trim();
    let mut text: String = trimmed.chars().take(48).collect();
    if trimmed.chars().count() > 48 {
        text.push_str("...");
    }
    format!("\"{}\"", text.replace('"', "'"))
}

/// Collects the effective spawn binds: built-ins after unbinds, then config overrides.
fn effective_spawns(
    registry: &KomaConfigRegistry,
    config_path: &Path,
    catalogue: &BTreeMap<String, (ActionDescriptor, &'static str)>,
) -> Vec<SpawnBind> {
    let mod_key = registry.input.mod_key.as_deref();
    let mut config_actions: BTreeMap<(Option<String>, String, String), ActionDescriptor> =
        BTreeMap::new();
    for spec in &registry.binds {
        let key = (
            spec.mode.clone(),
            format!("{:?}", spec.options.edge),
            chord_identity(&spec.chord, mod_key),
        );
        // Later definitions win, matching `merge_binds`.
        config_actions.insert(key, spec.action.clone());
    }

    let mut spawns = Vec::new();
    for row in merge_binds(registry, config_path, KEYBIND_CATALOG) {
        let identity = chord_identity(&row.raw_chord, mod_key);
        let key = (
            row.mode.clone(),
            format!("{:?}", row.edge),
            identity.clone(),
        );
        let action = config_actions
            .get(&key)
            .or_else(|| catalogue.get(&identity).map(|(action, _)| action));
        let Some(ActionDescriptor::Spawn { argv }) = action else {
            continue;
        };
        let chord = match &row.mode {
            Some(mode) => format!("{} (mode `{mode}`)", row.chord),
            None => row.chord.clone(),
        };
        spawns.push(SpawnBind {
            chord,
            argv: argv.clone(),
            builtin: row.source == "builtin",
        });
    }
    spawns
}

fn check_spawn_targets(
    registry: &KomaConfigRegistry,
    config_path: &Path,
    config_root: &Path,
    path_dirs: &[PathBuf],
    report: &mut Report,
) {
    let catalogue = match catalogue_actions(registry.input.mod_key.as_deref()) {
        Ok(catalogue) => catalogue,
        Err(err) => {
            for check in ["bind.spawn-target", "bind.spawn-target-deep"] {
                report.push(
                    "binds",
                    check,
                    Severity::Skip,
                    format!("could not evaluate the built-in catalogue: {err}"),
                );
            }
            return;
        }
    };

    let spawns = effective_spawns(registry, config_path, &catalogue);
    if spawns.is_empty() {
        report.push(
            "binds",
            "bind.spawn-target",
            Severity::Ok,
            "no bind spawns a program".to_owned(),
        );
        report.push(
            "binds",
            "bind.spawn-target-deep",
            Severity::Ok,
            "no `<shell> -c` command string to follow".to_owned(),
        );
    } else {
        check_spawn_programs(&spawns, path_dirs, config_root, report);
    }
}

fn check_spawn_programs(
    spawns: &[SpawnBind],
    path_dirs: &[PathBuf],
    config_root: &Path,
    report: &mut Report,
) {
    let mut resolved = 0usize;
    let mut deep_resolved = 0usize;
    for spawn in spawns {
        let Some(program) = spawn.argv.first() else {
            report.push(
                "binds",
                "bind.spawn-target",
                Severity::Skip,
                format!("`{}` spawns an empty argv", spawn.chord),
            );
            continue;
        };
        let origin = if spawn.builtin {
            " (built-in default)"
        } else {
            ""
        };
        let name = program_name(program);
        if SPAWN_WRAPPERS.contains(&name) {
            report.push(
                "binds",
                "bind.spawn-target",
                Severity::Skip,
                format!(
                    "`{}` runs `{name}`, which wraps the real program in its arguments",
                    spawn.chord
                ),
            );
            continue;
        }

        let resolution = resolve_program(program, path_dirs, BareName::Execve, config_root);
        match &resolution {
            Resolution::Executable(path) => {
                if COMMAND_SHELLS.contains(&name) && command_string(&spawn.argv).is_none() {
                    report.push(
                        "binds",
                        "bind.spawn-target",
                        Severity::Skip,
                        format!(
                            "`{}` runs `{}` with interpreter arguments, which doctor does not \
                             follow",
                            spawn.chord,
                            path.display()
                        ),
                    );
                } else {
                    resolved += 1;
                }
            }
            // A missing program behind a *built-in default* is not a configuration mistake:
            // the compositor already warns and falls back when the bind fires, and a stock
            // install has no reason to be told that the shipped default terminal is absent.
            Resolution::Missing(reason) if spawn.builtin => report.push(
                "binds",
                "bind.spawn-target",
                Severity::Skip,
                format!(
                    "`{}` spawns the built-in default `{program}`, which {reason}; the \
                     compositor falls back when the bind fires",
                    spawn.chord
                ),
            ),
            Resolution::Missing(reason) => report.push(
                "binds",
                "bind.spawn-target",
                Severity::Warn,
                format!(
                    "`{}` spawns `{program}`, which {reason}{origin}",
                    spawn.chord
                ),
            ),
            Resolution::Skip(reason) => report.push(
                "binds",
                "bind.spawn-target",
                Severity::Skip,
                format!("`{}` spawns `{program}`, which {reason}", spawn.chord),
            ),
        }

        let Some(command) = command_string(&spawn.argv) else {
            continue;
        };
        if matches!(resolution, Resolution::Missing(_)) {
            continue;
        }
        let snippet = shell_snippet(command);
        match first_shell_word(command) {
            ShellWord::Program(word) => {
                let bare = if name == "fish" {
                    BareName::FishWord
                } else {
                    BareName::ShellWord
                };
                match resolve_program(&word, path_dirs, bare, config_root) {
                    Resolution::Executable(_) => deep_resolved += 1,
                    Resolution::Missing(reason) => report.push(
                        "binds",
                        "bind.spawn-target-deep",
                        Severity::Warn,
                        format!(
                            "`{}` runs `{name} -c {snippet}`, whose program `{word}` {reason}",
                            spawn.chord
                        ),
                    ),
                    Resolution::Skip(reason) => report.push(
                        "binds",
                        "bind.spawn-target-deep",
                        Severity::Skip,
                        format!(
                            "`{}` runs `{name} -c {snippet}`, whose program `{word}` {reason}",
                            spawn.chord
                        ),
                    ),
                }
            }
            ShellWord::Skip(reason) => report.push(
                "binds",
                "bind.spawn-target-deep",
                Severity::Skip,
                format!(
                    "`{}` runs `{name} -c {snippet}`, which {reason}",
                    spawn.chord
                ),
            ),
        }
    }

    if resolved > 0 {
        report.push(
            "binds",
            "bind.spawn-target",
            Severity::Ok,
            format!("{resolved} spawn program(s) resolve to an executable"),
        );
    }
    if deep_resolved > 0 {
        report.push(
            "binds",
            "bind.spawn-target-deep",
            Severity::Ok,
            format!("{deep_resolved} `<shell> -c` program(s) resolve to an executable"),
        );
    }
}

fn check_monitors(registry: &KomaConfigRegistry, outputs: &Outputs, report: &mut Report) {
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    for monitor in &registry.monitors {
        if let Some(output) = &monitor.output {
            *seen.entry(output.as_str().to_owned()).or_default() += 1;
        } else if monitor.edid_match.is_none() {
            report.push(
                "monitors",
                "monitor.match",
                Severity::Warn,
                "a koma.monitor block has neither `output` nor `match`; it can never apply"
                    .to_owned(),
            );
        }
    }
    for (connector, count) in seen {
        if count > 1 {
            report.push(
                "monitors",
                "monitor.duplicate",
                Severity::Warn,
                format!("`{connector}` has {count} koma.monitor blocks; the last one wins"),
            );
        }
    }

    let named: Vec<String> = registry
        .monitors
        .iter()
        .filter_map(|monitor| monitor.output.as_ref().map(|o| o.as_str().to_owned()))
        .collect();
    match outputs {
        Outputs::Unavailable(reason) => report.push(
            "monitors",
            "monitor.connector",
            Severity::Skip,
            format!("connector check skipped: {reason}"),
        ),
        Outputs::Reported(reported) => {
            let connected = if reported.is_empty() {
                "none".to_owned()
            } else {
                reported.join(", ")
            };
            let mut warned = false;
            for connector in &named {
                if reported.iter().any(|name| name == connector) {
                    continue;
                }
                warned = true;
                report.push(
                    "monitors",
                    "monitor.connector",
                    Severity::Warn,
                    format!(
                        "koma.monitor output `{connector}` is not reported as connected \
                         (connected: {connected})"
                    ),
                );
            }
            for workspace in &registry.workspaces {
                if let Some(target) = &workspace.preferred_output
                    && !reported.iter().any(|name| name.as_str() == target.as_str())
                {
                    warned = true;
                    report.push(
                        "workspaces",
                        "workspace.preferred-output",
                        Severity::Warn,
                        format!(
                            "workspace `{}` prefers `{}`, which is not reported as connected",
                            workspace.name,
                            target.as_str()
                        ),
                    );
                }
            }
            if !warned {
                report.push(
                    "monitors",
                    "monitor.connector",
                    Severity::Ok,
                    format!(
                        "{} monitor rule(s) match reported connectors ({connected})",
                        named.len()
                    ),
                );
            }
        }
    }
}

fn check_workspaces(registry: &KomaConfigRegistry, report: &mut Report) {
    let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
    for workspace in &registry.workspaces {
        *seen.entry(workspace.name.as_str()).or_default() += 1;
    }
    let duplicates: Vec<(&str, usize)> = seen.into_iter().filter(|(_, count)| *count > 1).collect();
    if duplicates.is_empty() {
        report.push(
            "workspaces",
            "workspace.duplicate",
            Severity::Ok,
            format!("{} workspace name(s) are unique", registry.workspaces.len()),
        );
    } else {
        for (name, count) in duplicates {
            report.push(
                "workspaces",
                "workspace.duplicate",
                Severity::Warn,
                format!("workspace `{name}` is declared {count} times"),
            );
        }
    }
}

fn check_rules(evaluation: &ConfigEvaluation, report: &mut Report) {
    let issues: Vec<&config_probe::SemanticIssue> = evaluation
        .semantic_issues
        .iter()
        .filter(|issue| issue.check.starts_with("rule."))
        .collect();
    if issues.is_empty() {
        report.push(
            "rules",
            "rule.regex",
            Severity::Ok,
            "window and layer rule patterns compile".to_owned(),
        );
    } else {
        for issue in issues {
            report.push(
                "rules",
                "rule.regex",
                Severity::Error,
                issue.message.clone(),
            );
        }
    }
}

fn check_gestures(
    evaluation: &ConfigEvaluation,
    registry: &KomaConfigRegistry,
    report: &mut Report,
) {
    let issues: Vec<&config_probe::SemanticIssue> = evaluation
        .semantic_issues
        .iter()
        .filter(|issue| issue.check == "input.gesture")
        .collect();
    if issues.is_empty() {
        report.push(
            "binds",
            "input.gesture",
            Severity::Ok,
            format!("{} gesture(s) are valid", registry.gestures.len()),
        );
    } else {
        for issue in issues {
            report.push(
                "binds",
                "input.gesture",
                Severity::Error,
                issue.message.clone(),
            );
        }
    }
}

fn check_runtime_dir(report: &mut Report) {
    match env::var_os("XDG_RUNTIME_DIR").filter(|value| !value.is_empty()) {
        None => report.push(
            "environment",
            "env.runtime-dir",
            Severity::Warn,
            "XDG_RUNTIME_DIR is unset; a compositor started from this environment cannot publish \
             its admin socket"
                .to_owned(),
        ),
        Some(dir) => {
            let path = PathBuf::from(&dir);
            let meta = fs::metadata(&path);
            match meta {
                Err(err) => report.push(
                    "environment",
                    "env.runtime-dir",
                    Severity::Warn,
                    format!("XDG_RUNTIME_DIR {} is not usable: {err}", path.display()),
                ),
                Ok(meta) => {
                    let euid = rustix::process::geteuid().as_raw();
                    let mode = meta.mode() & 0o777;
                    if meta.uid() != euid {
                        report.push(
                            "environment",
                            "env.runtime-dir",
                            Severity::Warn,
                            format!(
                                "XDG_RUNTIME_DIR {} is owned by uid {}, not {euid}",
                                path.display(),
                                meta.uid()
                            ),
                        );
                    } else if mode != 0o700 {
                        report.push(
                            "environment",
                            "env.runtime-dir",
                            Severity::Warn,
                            format!(
                                "XDG_RUNTIME_DIR {} has mode {mode:o}, expected 700",
                                path.display()
                            ),
                        );
                    } else {
                        report.push(
                            "environment",
                            "env.runtime-dir",
                            Severity::Ok,
                            format!("XDG_RUNTIME_DIR {} is private to this user", path.display()),
                        );
                    }
                }
            }
        }
    }
}

/// Reports the state of the session's `koma-overview` daemon (`Mod + Tab`).
///
/// The daemon is a separate process with its own lifetime: a control socket file can outlive it,
/// which is exactly the state in which every toggle used to fail with `ECONNREFUSED`
/// (komawm/koma#75). The distinction between "answers", "file present but listener dead" and "never
/// started" is what makes that diagnosable.
fn check_overview_daemon(overview: Option<&OverviewPaths>, report: &mut Report) {
    const CHECK: &str = "environment.overview-daemon";
    let Some(paths) = overview else {
        report.push(
            "environment",
            CHECK,
            Severity::Skip,
            "no Koma instance runtime directory found; start a session or set \
             KOMA_INSTANCE_SIGNATURE to check the overview daemon"
                .to_owned(),
        );
        return;
    };

    match probe_overview_daemon(paths) {
        OverviewProbe::Live { pid } => report.push(
            "environment",
            CHECK,
            Severity::Ok,
            format!(
                "{} answers{}",
                paths.socket.display(),
                pid.map_or_else(String::new, |pid| format!(" (daemon pid {pid})"))
            ),
        ),
        OverviewProbe::Stale {
            recorded_pid,
            live_pid,
            detail,
        } => {
            let pid_note = match (recorded_pid, live_pid) {
                (Some(pid), None) => format!("; recorded daemon pid {pid} is gone"),
                (Some(pid), Some(_)) => {
                    format!("; recorded daemon pid {pid} is still running without serving it")
                }
                (None, _) => String::new(),
            };
            report.push(
                "environment",
                CHECK,
                Severity::Warn,
                format!(
                    "{} exists but nothing is listening ({detail}){pid_note}; press the overview \
                     chord to restart it (`Mod + Tab`)",
                    paths.socket.display()
                ),
            );
        }
        OverviewProbe::Absent => report.push(
            "environment",
            CHECK,
            Severity::Skip,
            format!(
                "no koma-overview control socket in {}; no overview daemon was started for this \
                 instance",
                paths.runtime_dir().display()
            ),
        ),
    }
}

fn check_live(
    live: &LiveProbe,
    config_path: &Path,
    installed_template: Option<u64>,
    report: &mut Report,
) {
    match live {
        LiveProbe::Offline { detail } => report.push(
            "environment",
            "env.socket",
            Severity::Ok,
            format!("no live compositor ({detail}); every offline check ran"),
        ),
        LiveProbe::Unreachable {
            admin_socket,
            detail,
        } => report.push(
            "environment",
            "env.socket",
            Severity::Warn,
            format!(
                "{} exists but config.status failed ({detail}); stale socket or a different user",
                admin_socket.display()
            ),
        ),
        LiveProbe::Reachable {
            admin_socket,
            status,
        } => {
            let generation = status
                .get("generation")
                .and_then(Value::as_u64)
                .map_or_else(|| "?".to_owned(), |value| value.to_string());
            report.push(
                "environment",
                "env.socket",
                Severity::Ok,
                format!(
                    "{} answered config.status (generation {generation})",
                    admin_socket.display()
                ),
            );

            let mut problems = 0usize;
            if let Some(active) = status.get("active_path").and_then(Value::as_str)
                && !active.is_empty()
                && Path::new(active) != config_path
            {
                problems += 1;
                report.push(
                    "environment",
                    "env.live-config",
                    Severity::Warn,
                    format!(
                        "the running compositor loaded `{active}`, not `{}`",
                        config_path.display()
                    ),
                );
            }
            if let Some(failures) = status.get("reload_failures").and_then(Value::as_u64)
                && failures > 0
            {
                problems += 1;
                report.push(
                    "environment",
                    "env.live-config",
                    Severity::Warn,
                    format!("the compositor rejected {failures} configuration reload(s)"),
                );
            }
            if let Some(error) = status.get("last_error").and_then(Value::as_str)
                && !error.is_empty()
            {
                problems += 1;
                report.push(
                    "environment",
                    "env.live-config",
                    Severity::Warn,
                    format!("last reload error: {}", collapse(error)),
                );
            }
            if problems == 0 {
                report.push(
                    "environment",
                    "env.live-config",
                    Severity::Ok,
                    "the compositor is running the configuration checked here".to_owned(),
                );
            }

            match installed_template {
                Some(installed) if installed != u64::from(TEMPLATE_VERSION) => report.push(
                    "environment",
                    "env.version",
                    Severity::Warn,
                    format!(
                        "configuration template v{installed} does not match the running \
                         komactl {} (template v{TEMPLATE_VERSION})",
                        env!("CARGO_PKG_VERSION")
                    ),
                ),
                Some(_) => report.push(
                    "environment",
                    "env.version",
                    Severity::Ok,
                    format!(
                        "komactl {} matches the running configuration template v{TEMPLATE_VERSION}",
                        env!("CARGO_PKG_VERSION")
                    ),
                ),
                None => report.push(
                    "environment",
                    "env.version",
                    Severity::Skip,
                    "no template marker in the live configuration; generation comparison skipped"
                        .to_owned(),
                ),
            }

            if let Some(locked) = status.get("locked").and_then(Value::as_bool) {
                report.push(
                    "environment",
                    "env.lock",
                    Severity::Ok,
                    format!(
                        "session lock state: {}",
                        if locked { "locked" } else { "unlocked" }
                    ),
                );
            }
        }
    }
}

// =============================================================================
// Diagnosis
// =============================================================================

/// Runs every check against `config_path`, using `context` for the non-hermetic inputs.
#[must_use]
pub(crate) fn diagnose(config_path: &Path, context: &DoctorContext) -> Report {
    let template = config_probe::template_report_at(config_path);
    let installed_template = template.get("installed_version").and_then(Value::as_u64);
    let mut report = Report {
        config_path: config_path.to_path_buf(),
        findings: Vec::new(),
        readable: true,
        strict: false,
    };

    let evaluation = match config_probe::evaluate(config_path) {
        Ok(evaluation) => Some(evaluation),
        Err(err) => {
            report.readable = false;
            report.push("config", "config.path", Severity::Error, err);
            None
        }
    };

    if let Some(evaluation) = &evaluation {
        report.push(
            "config",
            "config.path",
            Severity::Ok,
            format!(
                "{} ({} bytes, lua modules under {})",
                config_path.display(),
                evaluation.source.len(),
                evaluation.config_root.join("lua").display()
            ),
        );
        match &evaluation.eval_error {
            Some(err) => report.push("config", "config.syntax", Severity::Error, collapse(err)),
            None => report.push(
                "config",
                "config.syntax",
                Severity::Ok,
                "init.lua evaluated in the jailed shadow VM".to_owned(),
            ),
        }
        check_require_targets(evaluation, &mut report);
        check_unknown_keys(evaluation, &mut report);
    }

    check_template(&mut report, config_path, &template);

    if let Some(evaluation) = &evaluation
        && let Some(registry) = &evaluation.registry
    {
        check_rules(evaluation, &mut report);
        check_bind_chords(registry, &mut report);
        check_unbinds(registry, &mut report);
        check_duplicate_binds(registry, &mut report);
        check_shadowed_builtins(registry, &mut report);
        let effective_paths = context.effective_program_paths(registry, &evaluation.config_root);
        check_spawn_targets(
            registry,
            &evaluation.path,
            &evaluation.config_root,
            &effective_paths,
            &mut report,
        );
        check_gestures(evaluation, registry, &mut report);
        check_monitors(registry, &context.outputs, &mut report);
        check_workspaces(registry, &mut report);
        report.push(
            "binds",
            "bind.summary",
            Severity::Ok,
            format!(
                "{} bind(s), {} unbind(s), {} gesture(s), {} macro(s) are effective",
                merge_binds(registry, config_path, KEYBIND_CATALOG).len(),
                registry.unbinds.len(),
                registry.gestures.len(),
                registry.macros.len()
            ),
        );
    } else if evaluation.is_some() {
        for (check, area) in [
            ("rule.regex", "rules"),
            ("bind.chord", "binds"),
            ("bind.unbind", "binds"),
            ("bind.duplicate", "binds"),
            ("bind.shadow-builtin", "binds"),
            ("bind.spawn-target", "binds"),
            ("bind.spawn-target-deep", "binds"),
            ("input.gesture", "binds"),
            ("monitor.connector", "monitors"),
            ("workspace.duplicate", "workspaces"),
        ] {
            report.push(
                area,
                check,
                Severity::Skip,
                "skipped: the configuration did not evaluate".to_owned(),
            );
        }
    }

    check_runtime_dir(&mut report);
    check_overview_daemon(context.overview.as_ref(), &mut report);
    check_live(&context.live, config_path, installed_template, &mut report);

    report
}

// =============================================================================
// Rendering
// =============================================================================

fn sorted_findings(report: &Report) -> Vec<&Finding> {
    let mut findings: Vec<&Finding> = report.findings.iter().collect();
    findings.sort_by_key(|finding| {
        let area = AREA_ORDER
            .iter()
            .position(|known| *known == finding.area)
            .unwrap_or(AREA_ORDER.len());
        (area, finding.check)
    });
    findings
}

/// Renders the grouped, human-readable report.
#[must_use]
pub(crate) fn render_human(report: &Report) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "komactl doctor — {} (komactl {}, template v{TEMPLATE_VERSION})\n",
        report.config_path.display(),
        env!("CARGO_PKG_VERSION")
    ));
    out.push_str(
        "diagnostic only: errors describe a configuration Koma will refuse to load, but the\n\
         compositor keeps running on its last valid configuration; `--strict` makes this exit 1.\n\n",
    );

    let findings = sorted_findings(report);
    let mut area = "";
    for finding in &findings {
        if finding.area != area {
            area = finding.area;
            out.push_str(&format!("{area}\n"));
        }
        out.push_str(&format!(
            "  {:<5} {:<28} {}\n",
            finding.severity.label(),
            finding.check,
            finding.message
        ));
    }

    let counts = report.counts();
    out.push_str(&format!(
        "\nsummary: {} ok, {} warn, {} error, {} skipped ({} check(s))\n",
        counts.ok,
        counts.warn,
        counts.error,
        counts.skipped,
        counts.total()
    ));
    out.push_str(&format!(
        "exit: {} ({})\n",
        report.exit_code(),
        if report.strict {
            "--strict: any warn or error exits 1"
        } else {
            "diagnostics never fail the compositor; pass --strict to gate on warn/error"
        }
    ));
    if !report.readable {
        out.push_str(&format!(
            "hard failure: `{}` could not be read; nothing was diagnosed\n",
            report.config_path.display()
        ));
    }
    out
}

/// Builds the `--json` document.
#[must_use]
pub(crate) fn render_json(report: &Report) -> Value {
    let counts = report.counts();
    json!({
        "config": report.config_path.display().to_string(),
        "komactl_version": env!("CARGO_PKG_VERSION"),
        "template_version": TEMPLATE_VERSION,
        "strict": report.strict,
        "readable": report.readable,
        "ok": counts.error == 0,
        "counts": {
            "ok": counts.ok,
            "warn": counts.warn,
            "error": counts.error,
            "skipped": counts.skipped,
            "total": counts.total(),
        },
        "exit_code": report.exit_code(),
        "findings": sorted_findings(report)
            .into_iter()
            .map(|finding| json!({
                "area": finding.area,
                "check": finding.check,
                "severity": finding.severity.as_json(),
                "message": finding.message,
            }))
            .collect::<Vec<Value>>(),
    })
}

// =============================================================================
// Command
// =============================================================================

const DOCTOR_USAGE_EXIT: i32 = 2;

fn doctor_help() -> String {
    format!(
        "komactl doctor — diagnose the configuration without touching the compositor

Usage:
  komactl doctor [path/to/init.lua] [--json] [--strict] [-h | --help]

Runs every non-destructive check and groups the rows by area:

  config       file readability, Lua sandbox evaluation, require() targets under lua/
  template     template generation vs the shipped v{TEMPLATE_VERSION}, missing generated files
  binds        chord syntax, duplicate chords, binds that shadow a built-in chord, and spawn
               programs that are missing or not executable
  monitors     koma.monitor output names vs the kernel's connected connectors
  workspaces   duplicate workspace names, preferred outputs that are not connected
  rules        window/layer rule regexes, gesture specs
  environment  XDG_RUNTIME_DIR, admin-socket reachability, koma-overview daemon state,
               live generation, lock state

Spawn checks never execute a bind: absolute paths must exist and be executable, bare
names resolve through `PATH`, and `<shell> -c` command strings are followed one level
to their first program word. Shell variables, globs, keywords, built-ins and pipelines
are `skip`, and a fish function or alias is never resolved by running a shell.

`doctor` reuses the same jailed Shadow-VM evaluation as `komactl validate`. A
syntax error is reported as `error` and can never fail the compositor: Koma keeps
running on its last valid configuration, which is why errors do not change the exit
code unless `--strict` is passed. Checks that need hardware or a compositor are
marked `skip` instead of failing.

Severities:
  ok     the check ran and found nothing
  warn   the configuration loads but is probably not what was intended
  error  the configuration would be rejected on the next reload
  skip   the check cannot run in this environment

Options:
  --json     Emit one JSON document with counts and per-check rows
  --strict   Exit 1 when any warn or error is reported
  -h, --help Print this help

Exit codes:
  0  diagnostics ran; warnings and configuration errors are informational
  1  `--strict` was passed and at least one warn or error was reported
  2  the configuration could not be read at all, a usage error, or an internal failure
"
    )
}

#[derive(Debug, Clone, Default)]
struct DoctorArgs {
    path: Option<PathBuf>,
    json: bool,
    strict: bool,
    help: bool,
}

fn parse_args(args: &[String]) -> Result<DoctorArgs, String> {
    let mut parsed = DoctorArgs::default();
    for arg in args {
        match arg.as_str() {
            "--json" => parsed.json = true,
            "--strict" => parsed.strict = true,
            "-h" | "--help" => parsed.help = true,
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
    }
    Ok(parsed)
}

/// Entry point for `komactl doctor`.
pub(crate) fn run(args: &[String]) -> i32 {
    let parsed = match parse_args(args) {
        Ok(parsed) => parsed,
        Err(err) => {
            eprintln!("komactl doctor: {err}\n\n{}", doctor_help());
            return DOCTOR_USAGE_EXIT;
        }
    };
    if parsed.help {
        crate::emit(&doctor_help());
        return 0;
    }
    let path = parsed
        .path
        .map(|p| config_probe::resolve_cli_path(&p.to_string_lossy()))
        .unwrap_or_else(config_probe::default_init_lua_path);
    let context = DoctorContext::detect();
    let mut report = diagnose(&path, &context);
    report.strict = parsed.strict;

    if parsed.json {
        let mut text = serde_json::to_string_pretty(&render_json(&report)).unwrap();
        text.push('\n');
        crate::emit(&text);
    } else {
        crate::emit(&render_human(&report));
    }

    if !report.readable {
        return DOCTOR_USAGE_EXIT;
    }
    report.exit_code()
}

#[cfg(test)]
mod tests {
    use std::{
        fmt::Write as _,
        fs,
        os::unix::fs::PermissionsExt,
        sync::atomic::{AtomicU32, Ordering},
    };

    use koma_lua::{KEYBIND_CATALOG, scaffold_config_tree};
    use serde_json::Value;

    use super::*;

    static NEXT_ID: AtomicU32 = AtomicU32::new(0);

    fn temp_root(label: &str) -> PathBuf {
        let base = Path::new("/tmp/koma");
        fs::create_dir_all(base).expect("create /tmp/koma");
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let dir = base.join(format!("doctor-{label}-{}-{id}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create fixture dir");
        dir
    }

    fn context(outputs: Outputs) -> DoctorContext {
        DoctorContext {
            outputs,
            live: LiveProbe::Offline {
                detail: "unit test".to_owned(),
            },
            overview: None,
            spawn_path: None,
        }
    }

    fn context_with_overview(outputs: Outputs, overview: OverviewPaths) -> DoctorContext {
        DoctorContext {
            overview: Some(overview),
            ..context(outputs)
        }
    }

    /// Context with an explicit program search path, so spawn checks never depend on the host.
    fn context_with_path(outputs: Outputs, spawn_path: Vec<PathBuf>) -> DoctorContext {
        DoctorContext {
            spawn_path: Some(spawn_path),
            ..context(outputs)
        }
    }

    /// Creates `<root>/bin` with executable stand-ins (symlinks to `/bin/sh`).
    fn fixture_bin(root: &Path, names: &[&str]) -> Vec<PathBuf> {
        let bin = root.join("bin");
        fs::create_dir_all(&bin).expect("create fixture bin");
        for name in names {
            let path = bin.join(name);
            if !path.exists() {
                std::os::unix::fs::symlink("/bin/sh", &path).expect("symlink fixture program");
            }
        }
        vec![bin]
    }

    fn offline_context() -> DoctorContext {
        context(Outputs::Unavailable("unit test".to_owned()))
    }

    fn fixture(label: &str, source: &str) -> PathBuf {
        let root = temp_root(label);
        fs::write(root.join("init.lua"), source).expect("write init.lua");
        root
    }

    fn finding<'a>(report: &'a Report, check: &str) -> Vec<&'a Finding> {
        report
            .findings
            .iter()
            .filter(|finding| finding.check == check)
            .collect()
    }

    #[test]
    fn syntax_error_is_reported_as_error_but_exit_stays_non_fatal() {
        let root = fixture("syntax", "koma.bind(\"Mod + X\",\n");
        let init = root.join("init.lua");
        let mut report = diagnose(&init, &offline_context());
        let syntax = finding(&report, "config.syntax");
        assert_eq!(syntax.len(), 1);
        assert_eq!(syntax[0].severity, Severity::Error);
        assert!(syntax[0].message.contains("init.lua"));
        assert!(report.has_error());
        assert_eq!(report.exit_code(), 0, "errors are non-fatal by default");

        report.strict = true;
        assert_eq!(report.exit_code(), 1, "--strict gates on errors");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn severity_counts_and_exit_code_mapping() {
        let mut report = Report {
            config_path: PathBuf::from("/tmp/koma/init.lua"),
            findings: Vec::new(),
            readable: true,
            strict: false,
        };
        assert_eq!(report.exit_code(), 0);
        report.push("config", "a", Severity::Ok, "fine".to_owned());
        assert_eq!(
            report.counts(),
            Counts {
                ok: 1,
                ..Counts::default()
            }
        );
        report.push("binds", "b", Severity::Warn, "meh".to_owned());
        assert_eq!(report.exit_code(), 0, "warnings are non-fatal by default");
        report.strict = true;
        assert_eq!(report.exit_code(), 1, "strict gates on warnings");
        report.push("rules", "c", Severity::Skip, "unavailable".to_owned());
        report.push("config", "d", Severity::Error, "broken".to_owned());
        let counts = report.counts();
        assert_eq!(
            counts,
            Counts {
                ok: 1,
                warn: 1,
                error: 1,
                skipped: 1
            }
        );
        assert_eq!(counts.total(), 4);
        report.strict = false;
        assert_eq!(report.exit_code(), 0);
    }

    #[test]
    fn render_groups_rows_and_summarises_counts() {
        let mut report = Report {
            config_path: PathBuf::from("/tmp/koma/init.lua"),
            findings: Vec::new(),
            readable: true,
            strict: false,
        };
        report.push(
            "environment",
            "env.socket",
            Severity::Ok,
            "offline".to_owned(),
        );
        report.push(
            "config",
            "config.syntax",
            Severity::Error,
            "boom".to_owned(),
        );
        report.push(
            "binds",
            "bind.duplicate",
            Severity::Warn,
            "twice".to_owned(),
        );
        let human = render_human(&report);
        assert!(human.contains("config\n"));
        assert!(human.contains("binds\n"));
        assert!(human.contains("environment\n"));
        assert!(human.contains("config.syntax"));
        assert!(human.contains("summary: 1 ok, 1 warn, 1 error, 0 skipped (3 check(s))"));
        assert!(human.contains("exit: 0"));
        let json = render_json(&report);
        assert_eq!(json["counts"]["error"], 1);
        assert_eq!(json["counts"]["warn"], 1);
        assert_eq!(json["exit_code"], 0);
        assert_eq!(json["findings"].as_array().map(Vec::len), Some(3));
        let _ = fs::remove_dir_all("/tmp/koma/init.lua");
    }

    #[test]
    fn json_reports_the_strict_exit_code() {
        let mut report = Report {
            config_path: PathBuf::from("/tmp/koma/init.lua"),
            findings: Vec::new(),
            readable: true,
            strict: true,
        };
        report.push(
            "config",
            "config.syntax",
            Severity::Warn,
            "warning".to_owned(),
        );
        let json: Value = render_json(&report);
        assert_eq!(json["exit_code"], 1);
        assert_eq!(json["strict"], true);
    }

    #[test]
    fn scaffold_is_clean_and_matches_the_shipped_template() {
        let root = temp_root("scaffold");
        scaffold_config_tree(&root, false).expect("scaffold");
        let init = root.join("init.lua");
        let report = diagnose(&init, &context(Outputs::Reported(vec!["eDP-1".to_owned()])));
        assert!(!report.has_error(), "{:#?}", report.findings);
        assert!(report.readable);
        let template = finding(&report, "template.version");
        assert_eq!(template[0].severity, Severity::Ok);
        assert_eq!(finding(&report, "template.files")[0].severity, Severity::Ok);
        assert_eq!(
            finding(&report, "config.unknown-key")[0].severity,
            Severity::Ok
        );
        assert_eq!(finding(&report, "bind.chord")[0].severity, Severity::Ok);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn unknown_keys_are_reported_with_the_line() {
        let root = fixture(
            "unknown-key",
            "koma.monitor({ output = \"eDP-1\", widht = 100 })\n\
             koma.appearance({ border = { bogus_nested = 1 } })\n",
        );
        let report = diagnose(&root.join("init.lua"), &offline_context());
        let unknown = finding(&report, "config.unknown-key");
        assert_eq!(unknown.len(), 1, "nested tables are a different schema");
        assert_eq!(unknown[0].severity, Severity::Warn);
        assert!(unknown[0].message.contains("`widht`"));
        assert!(unknown[0].message.contains("line 1"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_lua_module_is_reported() {
        let root = fixture("require", "require(\"macros.wps\")\n");
        let report = diagnose(&root.join("init.lua"), &offline_context());
        let requires = finding(&report, "config.require");
        assert_eq!(requires.len(), 1);
        assert!(requires[0].message.contains("macros/wps.lua"));
        assert_eq!(requires[0].severity, Severity::Warn);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn unbind_reports_what_it_removes_and_flags_no_ops() {
        let root = fixture(
            "unbind",
            "koma.unbind(\"Mod + P\")\nkoma.unbind(\"Mod + F13\")\n",
        );
        let report = diagnose(&root.join("init.lua"), &offline_context());
        let unbinds = finding(&report, "bind.unbind");
        assert!(
            unbinds.iter().any(|row| row.severity == Severity::Ok
                && row.message.contains("Toggle Dwindle pseudotile")),
            "{unbinds:#?}"
        );
        assert!(
            unbinds
                .iter()
                .any(|row| row.severity == Severity::Warn && row.message.contains("Mod + F13")),
            "{unbinds:#?}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn duplicate_and_shadowed_binds_are_warnings() {
        let mut source = String::new();
        writeln!(
            source,
            "koma.bind(\"Mod + Q\", koma.act.spawn({{ \"a\" }}))\n\
             koma.bind(\"Mod + D\", koma.act.window.close())\n\
             koma.bind(\"mod + d\", koma.act.window.close())\n"
        )
        .expect("write to string");
        let root = fixture("binds", &source);
        let report = diagnose(&root.join("init.lua"), &offline_context());
        let duplicates = finding(&report, "bind.duplicate");
        assert_eq!(duplicates.len(), 1);
        assert_eq!(duplicates[0].severity, Severity::Warn);
        assert!(duplicates[0].message.contains("Mod + D"));
        let shadows = finding(&report, "bind.shadow-builtin");
        assert!(
            shadows
                .iter()
                .any(|row| row.severity == Severity::Warn && row.message.contains("Mod + Q")),
            "{shadows:#?}"
        );
        assert!(
            shadows.iter().all(|row| !row.message.contains("Mod + D")),
            "a config-only chord is not a shadow"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scaffold_repeating_builtin_binds_is_not_a_shadow() {
        let root = temp_root("parity");
        scaffold_config_tree(&root, false).expect("scaffold");
        let report = diagnose(&root.join("init.lua"), &offline_context());
        let shadow = finding(&report, "bind.shadow-builtin");
        assert_eq!(shadow.len(), 1);
        assert_eq!(shadow[0].severity, Severity::Ok);
        assert!(
            shadow[0]
                .message
                .contains(&KEYBIND_CATALOG.len().to_string())
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_modifier_is_an_error() {
        let root = fixture(
            "chord",
            "koma.bind(\"Hyper + Q\", koma.act.window.close())\n",
        );
        let report = diagnose(&root.join("init.lua"), &offline_context());
        let chords = finding(&report, "bind.chord");
        assert_eq!(chords.len(), 1);
        assert_eq!(chords[0].severity, Severity::Error);
        assert!(chords[0].message.contains("Hyper"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn monitor_connector_is_checked_against_reported_outputs() {
        let root = fixture(
            "monitor",
            "koma.monitor({ output = \"DP-9\" })\n\
             koma.monitor({ output = \"eDP-1\" })\n\
             koma.workspace({ name = \"1\", preferred_output = \"HDMI-A-1\" })\n\
             koma.monitor({})\n",
        );
        let report = diagnose(
            &root.join("init.lua"),
            &context(Outputs::Reported(vec!["eDP-1".to_owned()])),
        );
        let connectors = finding(&report, "monitor.connector");
        assert_eq!(connectors.len(), 1);
        assert!(connectors[0].message.contains("DP-9"));
        assert!(connectors[0].message.contains("eDP-1"));
        let preferred = finding(&report, "workspace.preferred-output");
        assert_eq!(preferred.len(), 1);
        assert!(preferred[0].message.contains("HDMI-A-1"));
        let matches = finding(&report, "monitor.match");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].severity, Severity::Warn);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn monitor_connector_check_is_skipped_without_drm_status() {
        let root = fixture("skip", "koma.monitor({ output = \"DP-9\" })\n");
        let report = diagnose(&root.join("init.lua"), &offline_context());
        let connectors = finding(&report, "monitor.connector");
        assert_eq!(connectors.len(), 1);
        assert_eq!(connectors[0].severity, Severity::Skip);
        let _ = fs::remove_dir_all(&root);
    }

    /// `environment.overview-daemon` — `ok` when the daemon answers.
    #[test]
    fn overview_daemon_answering_is_reported_as_ok() {
        use std::os::unix::net::UnixListener;

        let root = fixture("overview-ok", "koma.layout(\"dwindle\")\n");
        let instance = temp_root("overview-ok-instance");
        let paths = OverviewPaths::in_runtime_dir(&instance);
        let listener = UnixListener::bind(&paths.socket).expect("bind the control socket");
        fs::write(&paths.pid_file, format!("{}\n", std::process::id())).expect("write pid file");

        let report = diagnose(
            &root.join("init.lua"),
            &context_with_overview(Outputs::Unavailable("unit test".to_owned()), paths.clone()),
        );
        let rows = finding(&report, "environment.overview-daemon");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].severity, Severity::Ok);
        assert!(
            rows[0].message.contains("koma-overview.ctl.sock"),
            "{}",
            rows[0].message
        );
        assert!(
            rows[0].message.contains(&std::process::id().to_string()),
            "the answering daemon pid must be named: {}",
            rows[0].message
        );
        // Deliberately not `!report.has_warn()`: the shared fixture also runs the environment
        // checks, and `XDG_RUNTIME_DIR` legitimately warns in a bare CI image (seen on OpenBSD),
        // which made this test fail for a reason unrelated to the overview daemon.
        assert!(
            rows.iter().all(|row| row.severity == Severity::Ok),
            "the overview row must be the only verdict here: {rows:?}"
        );

        drop(listener);
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&instance);
    }

    /// A missing program behind a built-in default is `skip`, not `warn`: the shipped keymap
    /// must not make a stock install look misconfigured (the compositor falls back when it fires).
    #[test]
    fn missing_builtin_default_program_is_skipped_instead_of_warned() {
        let root = fixture("builtin-spawn-missing", "koma.layout(\"dwindle\")\n");
        let report = diagnose(
            &root.join("init.lua"),
            &context_with_path(Outputs::Unavailable("unit test".to_owned()), Vec::new()),
        );
        let rows = finding(&report, "bind.spawn-target");
        let terminal = rows
            .iter()
            .find(|row| row.message.contains("Mod + Return"))
            .expect("the built-in terminal bind must still be reported");
        assert_eq!(
            terminal.severity,
            Severity::Skip,
            "a missing built-in default must not warn: {}",
            terminal.message
        );
        assert!(
            terminal.message.contains("built-in default"),
            "{}",
            terminal.message
        );
        assert!(
            rows.iter().all(|row| row.severity != Severity::Warn),
            "a stock config with an empty PATH must not warn about spawn targets: {:?}",
            report
                .findings
                .iter()
                .filter(|f| f.severity == Severity::Warn)
                .map(|f| f.message.clone())
                .collect::<Vec<_>>()
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// `environment.overview-daemon` — `warn` when the socket file outlived the listener.
    #[test]
    fn overview_socket_without_listener_is_a_warning_naming_the_stale_pid() {
        use std::os::unix::net::UnixListener;

        let root = fixture("overview-warn", "koma.layout(\"dwindle\")\n");
        let instance = temp_root("overview-warn-instance");
        let paths = OverviewPaths::in_runtime_dir(&instance);
        // Bind and drop: the socket inode survives, the listener does not — the exact state every
        // `Mod + Tab` press failed against in komawm/koma#75.
        let listener = UnixListener::bind(&paths.socket).expect("bind the control socket");
        drop(listener);
        fs::write(&paths.pid_file, "4000000\n").expect("write a dead pid");

        let report = diagnose(
            &root.join("init.lua"),
            &context_with_overview(Outputs::Unavailable("unit test".to_owned()), paths.clone()),
        );
        let rows = finding(&report, "environment.overview-daemon");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].severity, Severity::Warn);
        assert!(
            rows[0].message.contains("nothing is listening"),
            "{}",
            rows[0].message
        );
        assert!(rows[0].message.contains("4000000"), "{}", rows[0].message);
        assert!(rows[0].message.contains("is gone"), "{}", rows[0].message);
        assert!(report.has_warn());
        assert_eq!(
            report.exit_code(),
            0,
            "warnings stay non-fatal without --strict"
        );

        let mut strict = report.clone();
        strict.strict = true;
        assert_eq!(strict.exit_code(), 1, "--strict gates the warning");

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&instance);
    }

    /// `environment.overview-daemon` — `skip` when no daemon was ever started for the instance.
    #[test]
    fn overview_daemon_absent_is_skipped() {
        let root = fixture("overview-skip", "koma.layout(\"dwindle\")\n");
        let instance = temp_root("overview-skip-instance");
        let paths = OverviewPaths::in_runtime_dir(&instance);

        let report = diagnose(
            &root.join("init.lua"),
            &context_with_overview(Outputs::Unavailable("unit test".to_owned()), paths),
        );
        let rows = finding(&report, "environment.overview-daemon");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].severity, Severity::Skip);
        assert!(
            rows[0].message.contains("no overview daemon"),
            "{}",
            rows[0].message
        );

        // Without a resolvable instance directory the row is still present and skipped.
        let report = diagnose(&root.join("init.lua"), &offline_context());
        let rows = finding(&report, "environment.overview-daemon");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].severity, Severity::Skip);
        assert!(
            rows[0].message.contains("KOMA_INSTANCE_SIGNATURE"),
            "{}",
            rows[0].message
        );

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&instance);
    }

    #[test]
    fn template_generation_mismatch_is_a_warning() {
        let root = temp_root("template");
        fs::create_dir_all(root.join("lua")).expect("lua dir");
        fs::write(
            root.join("init.lua"),
            "-- koma-config-template: 0\nrequire(\"input\")\n",
        )
        .expect("write init.lua");
        let report = diagnose(&root.join("init.lua"), &offline_context());
        let template = finding(&report, "template.version");
        assert!(template.iter().any(|row| {
            row.severity == Severity::Warn && row.message.contains("older than the shipped")
        }));
        let files = finding(&report, "template.files");
        assert!(
            files
                .iter()
                .any(|row| row.severity == Severity::Warn
                    && row.message.contains("lua/keybinds.lua")),
            "{files:#?}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn unreadable_config_reports_a_hard_failure() {
        let report = diagnose(
            Path::new("/tmp/koma/doctor-absent/init.lua"),
            &offline_context(),
        );
        assert!(!report.readable);
        assert!(report.has_error());
        assert_eq!(report.exit_code(), 0, "exit 2 is applied by the caller");
        let human = render_human(&report);
        assert!(human.contains("hard failure"));
    }

    #[test]
    fn help_documents_severities_and_exit_codes() {
        let help = doctor_help();
        for needle in [
            "--json",
            "--strict",
            "Exit codes:",
            "skip",
            "never fail",
            "Spawn checks never execute a bind",
        ] {
            assert!(help.contains(needle), "help is missing `{needle}`");
        }
    }

    #[test]
    fn argument_parsing_rejects_unknown_flags() {
        let parsed =
            parse_args(&["/tmp/koma/x.lua".to_owned(), "--strict".to_owned()]).expect("parse");
        assert!(parsed.strict);
        assert_eq!(parsed.path, Some(PathBuf::from("/tmp/koma/x.lua")));
        assert!(parse_args(&["--bogus".to_owned()]).is_err());
        assert!(parse_args(&["a".to_owned(), "b".to_owned()]).is_err());
    }

    fn spawn_warnings(report: &Report) -> Vec<&Finding> {
        report
            .findings
            .iter()
            .filter(|row| row.check.starts_with("bind.spawn-") && row.severity == Severity::Warn)
            .collect()
    }

    #[test]
    fn spawn_targets_warn_for_missing_programs_and_follow_shell_strings() {
        let root = temp_root("spawn-missing");
        let bin = fixture_bin(&root, &["bitty", "koma-doctor-terminal"]);
        fs::write(
            root.join("init.lua"),
            "koma.bind(\"Mod + T\", koma.act.spawn({ \"koma-doctor-terminal\" }))\n\
             koma.bind(\"Mod + R\", koma.act.spawn({ \"/usr/bin/koma-doctor-absent\", \"-a\" }))\n\
             koma.bind(\"Mod + Y\", koma.act.spawn({ \"koma-doctor-absent\" }))\n\
             koma.bind(\"Mod + U\", koma.act.spawn({ \"/bin/sh\", \"-c\", \
             \"koma-doctor-absent --flag\" }))\n",
        )
        .expect("write init.lua");
        let init = root.join("init.lua");
        let mut report = diagnose(
            &init,
            &context_with_path(Outputs::Unavailable("unit test".to_owned()), bin),
        );

        let warnings = spawn_warnings(&report);
        assert_eq!(warnings.len(), 3, "{warnings:#?}");
        assert!(
            warnings.iter().any(|row| row.check == "bind.spawn-target"
                && row.message.contains("`Mod + R`")
                && row.message.contains("/usr/bin/koma-doctor-absent")
                && row.message.contains("is not installed")),
            "{warnings:#?}"
        );
        assert!(
            warnings.iter().any(|row| row.check == "bind.spawn-target"
                && row.message.contains("`Mod + Y`")
                && row.message.contains("is not on PATH")),
            "{warnings:#?}"
        );
        assert!(
            warnings
                .iter()
                .any(|row| row.check == "bind.spawn-target-deep"
                    && row.message.contains("`Mod + U`")
                    && row.message.contains("koma-doctor-absent")
                    && row.message.contains("is not on PATH")),
            "{warnings:#?}"
        );
        assert!(
            finding(&report, "bind.spawn-target")
                .iter()
                .any(|row| row.severity == Severity::Ok && row.message.contains("resolve")),
            "the valid spawn must be counted as resolved"
        );
        assert_eq!(report.exit_code(), 0, "warnings never change the exit code");
        report.strict = true;
        assert_eq!(report.exit_code(), 1, "--strict gates on spawn warnings");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn spawn_checks_skip_shell_syntax_variables_and_pipelines() {
        let root = temp_root("spawn-skip");
        let bin = fixture_bin(&root, &["bitty", "fish"]);
        fs::write(
            root.join("init.lua"),
            "koma.bind(\"Mod + A\", koma.act.spawn({ \"/bin/sh\", \"-c\", \"cd /tmp\" }))\n\
             koma.bind(\"Mod + B\", koma.act.spawn({ \"/bin/sh\", \"-c\", \"if true\" }))\n\
             koma.bind(\"Mod + C\", koma.act.spawn({ \"/bin/sh\", \"-c\", \"$TERMINAL\" }))\n\
             koma.bind(\"Mod + E\", koma.act.spawn({ \"/bin/sh\", \"-c\", \
             \"cliphist list | wofi --show dmenu\" }))\n\
             koma.bind(\"Mod + F\", koma.act.spawn({ \"fish\", \"-c\", \"changewp\" }))\n\
             koma.bind(\"Mod + G\", koma.act.spawn({ \"fish\", \"togglemode.fish\" }))\n\
             koma.bind(\"Mod + H\", koma.act.spawn({ \"$TERMINAL\" }))\n\
             koma.bind(\"Mod + J\", koma.act.spawn({ \"~other/script.sh\" }))\n\
             koma.bind(\"Mod + K\", koma.act.spawn({ \"env\", \"FOO=1\", \"kitty\" }))\n",
        )
        .expect("write init.lua");
        let init = root.join("init.lua");
        let report = diagnose(
            &init,
            &context_with_path(Outputs::Unavailable("unit test".to_owned()), bin),
        );

        assert!(
            spawn_warnings(&report).is_empty(),
            "{:#?}",
            spawn_warnings(&report)
        );
        let skips: Vec<&Finding> = report
            .findings
            .iter()
            .filter(|row| row.check.starts_with("bind.spawn-") && row.severity == Severity::Skip)
            .collect();
        let messages: Vec<&str> = skips.iter().map(|row| row.message.as_str()).collect();
        assert!(
            messages
                .iter()
                .any(|message| message.contains("`Mod + A`") && message.contains("`cd`")),
            "{messages:#?}"
        );
        assert!(
            messages.iter().any(|message| message.contains("`Mod + B`")
                && message.contains("shell builtin or keyword")),
            "{messages:#?}"
        );
        assert!(
            messages
                .iter()
                .any(|message| message.contains("`Mod + C`") && message.contains("variable")),
            "{messages:#?}"
        );
        assert!(
            messages
                .iter()
                .any(|message| message.contains("`Mod + E`") && message.contains("operator `|`")),
            "{messages:#?}"
        );
        assert!(
            messages.iter().any(|message| message.contains("`Mod + F`")
                && message.contains("fish function or alias")),
            "{messages:#?}"
        );
        assert!(
            messages.iter().any(|message| message.contains("`Mod + G`")
                && message.contains("interpreter arguments")),
            "{messages:#?}"
        );
        assert!(
            messages
                .iter()
                .any(|message| message.contains("`Mod + H`")
                    && message.contains("shell expansion")),
            "{messages:#?}"
        );
        assert!(
            messages
                .iter()
                .any(|message| message.contains("`Mod + J`")
                    && message.contains("shell expansion")),
            "{messages:#?}"
        );
        assert!(
            messages
                .iter()
                .any(|message| message.contains("`Mod + K`") && message.contains("wraps")),
            "{messages:#?}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn spawn_targets_warn_for_files_that_are_not_executable() {
        let root = temp_root("spawn-mode");
        let bin = root.join("bin");
        fs::create_dir_all(&bin).expect("create fixture bin");
        std::os::unix::fs::symlink("/bin/sh", bin.join("bitty")).expect("symlink bitty");
        let script = bin.join("koma-doctor-script");
        fs::write(&script, "#!/bin/sh\nexit 0\n").expect("write script");
        fs::set_permissions(&script, fs::Permissions::from_mode(0o644)).expect("chmod script");
        fs::write(
            root.join("init.lua"),
            format!(
                "koma.bind(\"Mod + P\", koma.act.spawn({{ {} }}))\n\
                 koma.bind(\"Mod + O\", koma.act.spawn({{ \"koma-doctor-script\" }}))\n",
                lua_quote(&script.display().to_string())
            ),
        )
        .expect("write init.lua");
        let report = diagnose(
            &root.join("init.lua"),
            &context_with_path(Outputs::Unavailable("unit test".to_owned()), vec![bin]),
        );

        let warnings = spawn_warnings(&report);
        assert_eq!(warnings.len(), 2, "{warnings:#?}");
        assert!(
            warnings.iter().any(|row| row.message.contains("`Mod + P`")
                && row.message.contains("is not executable")),
            "{warnings:#?}"
        );
        assert!(
            warnings.iter().any(|row| row.message.contains("`Mod + O`")
                && row.message.contains("exists at")
                && row.message.contains("is not executable")),
            "{warnings:#?}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scaffold_spawn_targets_resolve() {
        let root = temp_root("scaffold-spawn");
        scaffold_config_tree(&root, false).expect("scaffold");
        let bin = fixture_bin(&root, &["bitty"]);
        let report = diagnose(
            &root.join("init.lua"),
            &context_with_path(Outputs::Unavailable("unit test".to_owned()), bin),
        );

        let targets = finding(&report, "bind.spawn-target");
        assert!(!targets.is_empty());
        assert!(
            targets.iter().all(|row| row.severity == Severity::Ok),
            "{targets:#?}"
        );
        assert!(
            finding(&report, "bind.spawn-target-deep")
                .iter()
                .all(|row| row.severity == Severity::Ok)
        );
        assert!(spawn_warnings(&report).is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn spawn_resolver_and_shell_word_extraction_cover_the_edge_cases() {
        let root = temp_root("resolver-edge");
        let script = root.join("script.sh");
        fs::write(&script, "#!/bin/sh\nexit 0\n").expect("write script");
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod script");

        assert!(matches!(
            resolve_program("/bin/sh", &[], BareName::Execve, &root),
            Resolution::Executable(_)
        ));
        assert!(matches!(
            resolve_program("$TERMINAL", &[], BareName::Execve, &root),
            Resolution::Skip(_)
        ));
        assert!(matches!(
            resolve_program("fire*", &[], BareName::Execve, &root),
            Resolution::Skip(_)
        ));
        assert_eq!(
            resolve_program("./script.sh", &[], BareName::Execve, &root),
            Resolution::Executable(script.clone())
        );
        assert!(matches!(
            resolve_program("./absent.sh", &[], BareName::Execve, &root),
            Resolution::Missing(_)
        ));
        assert!(matches!(
            resolve_program("koma-doctor-absent", &[], BareName::Execve, &root),
            Resolution::Missing(_)
        ));
        assert!(matches!(
            resolve_program("koma-doctor-absent", &[], BareName::FishWord, &root),
            Resolution::Skip(_)
        ));

        assert_eq!(
            first_shell_word("koma-doctor-absent --flag"),
            ShellWord::Program("koma-doctor-absent".to_owned())
        );
        assert_eq!(
            first_shell_word("~/bin/my-tool --flag"),
            ShellWord::Program("~/bin/my-tool".to_owned())
        );
        assert_eq!(
            first_shell_word("FOO=1 BAR=2 /usr/bin/kitty"),
            ShellWord::Program("/usr/bin/kitty".to_owned())
        );
        for command in [
            "",
            "   ",
            "cd /tmp",
            "if true",
            "$TERMINAL",
            "a | b",
            "a && b",
            "FOO=1",
            "\"quoted word\"",
        ] {
            assert!(
                matches!(first_shell_word(command), ShellWord::Skip(_)),
                "`{command}` should be skipped"
            );
        }
        assert_eq!(
            command_string(&["sh".to_owned(), "-c".to_owned(), "x".to_owned()]),
            Some("x")
        );
        assert_eq!(
            command_string(&["/bin/bash".to_owned(), "-lc".to_owned(), "x".to_owned()]),
            Some("x")
        );
        assert_eq!(
            command_string(&["bash".to_owned(), "script.sh".to_owned()]),
            None
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn require_targets_and_relative_spawns_and_env_path_are_resolved() {
        let root = temp_root("paths-doctor");
        fs::create_dir_all(root.join("lua")).expect("create lua");
        fs::create_dir_all(root.join("scripts")).expect("create scripts");
        fs::create_dir_all(root.join("custom-bin")).expect("create custom-bin");
        fs::write(root.join("lua/input.lua"), "return {}\n").expect("write lua/input.lua");
        fs::write(root.join("lua/keys.lua"), "return {}\n").expect("write lua/keys.lua");
        fs::write(root.join("lua/theme.lua"), "return {}\n").expect("write lua/theme.lua");
        let local_script = root.join("scripts/launch.sh");
        fs::write(&local_script, "#!/bin/sh\nexit 0\n").expect("write launch.sh");
        fs::set_permissions(&local_script, fs::Permissions::from_mode(0o755))
            .expect("chmod launch.sh");
        std::os::unix::fs::symlink("/bin/sh", root.join("custom-bin/my-custom-tool"))
            .expect("symlink custom tool");
        std::os::unix::fs::symlink("/bin/sh", root.join("custom-bin/bitty"))
            .expect("symlink bitty");

        fs::write(
            root.join("init.lua"),
            "require(\"./lua/input\")\n\
             require(\"./keys\")\n\
             require(\"lua/theme\")\n\
             koma.env(\"PATH\", \"./custom-bin:/usr/bin\")\n\
             koma.bind(\"Mod + A\", koma.act.spawn({ \"./scripts/launch.sh\" }))\n\
             koma.bind(\"Mod + B\", koma.act.spawn({ \"my-custom-tool\" }))\n\
             koma.bind(\"Mod + C\", koma.act.spawn({ \"/bin/sh\", \"-c\", \"./scripts/launch.sh\" }))\n\
             koma.bind(\"Mod + D\", koma.act.spawn({ \"./scripts/missing.sh\" }))\n",
        )
        .expect("write init.lua");

        // Context with spawn_path = None so koma.env("PATH", ...) is honored!
        let report = diagnose(&root.join("init.lua"), &offline_context());
        let requires = finding(&report, "config.require");
        assert!(
            requires.iter().all(|row| row.severity == Severity::Ok),
            "{requires:#?}"
        );
        let warnings = spawn_warnings(&report);
        assert_eq!(warnings.len(), 1, "{warnings:#?}");
        assert!(warnings[0].message.contains("`Mod + D`"));

        // Escaping require() produces a warning with access violation instead of false-missing lua/
        fs::write(root.join("init.lua"), "require(\"../outside\")\n")
            .expect("write escape init.lua");
        let escape_report = diagnose(&root.join("init.lua"), &offline_context());
        let esc_req = finding(&escape_report, "config.require");
        assert_eq!(esc_req.len(), 1, "{esc_req:#?}");
        assert_eq!(esc_req[0].severity, Severity::Warn);
        assert!(
            esc_req[0].message.contains("access violation"),
            "{esc_req:#?}"
        );
        assert!(!esc_req[0].message.contains("has no lua/"), "{esc_req:#?}");

        let _ = fs::remove_dir_all(&root);
    }
}
