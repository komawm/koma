//! Shared offline `init.lua` evaluation for `komactl validate`, `doctor`, and `keys`.
//!
//! Every command reads the configuration the same way the compositor does: the file source is
//! evaluated in a jailed `Phodopus` shadow VM (no `io`, no `os.execute`, bounded heap), and the
//! resulting [`KomaConfigRegistry`] is then checked for the declarative rules the sandbox cannot
//! enforce itself (window-rule regexes, gesture specs, layer-rule regexes).

use std::{
    env, fs,
    path::{Path, PathBuf},
};

use koma_lua::{DEFAULT_HEAP_LIMIT_BYTES, KomaConfigRegistry, phodopus_bridge::PhodopusSandbox};
use regex::Regex;
use serde_json::{Value, json};

use crate::admin_rpc::AdminClient;

/// Outcome of probing the live compositor's admin socket.
#[derive(Debug, Clone)]
pub(crate) enum LiveProbe {
    /// `config.status` answered; the compositor is running and reachable.
    Reachable {
        admin_socket: PathBuf,
        status: Value,
    },
    /// No compositor socket exists in any runtime directory.
    Offline { detail: String },
    /// A socket exists but the RPC failed (stale socket, permission, or version mismatch).
    Unreachable {
        admin_socket: PathBuf,
        detail: String,
    },
}

/// Connects to a live `koma-admin.sock` and reads `config.status` without mutating the session.
#[must_use]
pub(crate) fn probe_live() -> LiveProbe {
    match AdminClient::connect() {
        Ok(mut client) => {
            let admin_socket = client.instance().admin_socket.clone();
            match client.call_raw("config.status", json!({})) {
                Ok((status, _)) => LiveProbe::Reachable {
                    admin_socket,
                    status,
                },
                Err(err) => LiveProbe::Unreachable {
                    admin_socket,
                    detail: err.to_string(),
                },
            }
        }
        Err(err) => {
            let existing = crate::admin_rpc::existing_admin_sockets();
            match existing.first() {
                Some(socket) => LiveProbe::Unreachable {
                    admin_socket: socket.clone(),
                    detail: err.to_string(),
                },
                None => LiveProbe::Offline {
                    detail: err.to_string(),
                },
            }
        }
    }
}

/// Offline Shadow-VM validation summary (`komactl validate [path]`, modeled on `niri validate`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ValidationSummary {
    pub monitors: usize,
    pub workspaces: usize,
    pub binds: usize,
    pub gestures: usize,
    pub rules: usize,
    pub macros: usize,
}

/// One schema-level problem the sandbox accepts but the compositor would refuse to compile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SemanticIssue {
    /// Stable check identifier (`rule.window-regex`, `input.gesture`, ...).
    pub check: &'static str,
    pub message: String,
}

/// Result of reading and evaluating one `init.lua`.
#[derive(Debug, Clone)]
pub(crate) struct ConfigEvaluation {
    pub path: PathBuf,
    pub config_root: PathBuf,
    pub source: String,
    /// `None` when the sandbox evaluation failed; the error is preserved verbatim.
    pub registry: Option<KomaConfigRegistry>,
    pub eval_error: Option<String>,
    pub semantic_issues: Vec<SemanticIssue>,
}

impl ConfigEvaluation {
    #[must_use]
    pub fn validated_summary(&self) -> Option<ValidationSummary> {
        if self.eval_error.is_some() || !self.semantic_issues.is_empty() {
            return None;
        }
        let registry = self.registry.as_ref()?;
        Some(summary_of(registry))
    }

    /// All blocking problems in evaluation order, formatted for `komactl validate`.
    #[must_use]
    pub fn error_messages(&self) -> Vec<String> {
        let mut messages = Vec::new();
        if let Some(err) = &self.eval_error {
            messages.push(err.clone());
        }
        messages.extend(self.semantic_issues.iter().map(|i| i.message.clone()));
        messages
    }
}

#[must_use]
pub(crate) fn summary_of(registry: &KomaConfigRegistry) -> ValidationSummary {
    ValidationSummary {
        monitors: registry.monitors.len(),
        workspaces: registry.workspaces.len(),
        binds: registry.binds.len(),
        gestures: registry.gestures.len(),
        rules: registry.window_rules.len(),
        macros: registry.macros.len(),
    }
}

/// Expands `~` / `~/...` and resolves relative CLI paths against the current working directory.
#[must_use]
pub(crate) fn resolve_cli_path(raw: impl AsRef<Path>) -> PathBuf {
    let cwd = env::current_dir().ok();
    koma_lua::path_resolve::resolve_config_file_path_from_env(raw, cwd.as_deref())
}

/// Resolves the implicit configuration path: `$KOMA_CONFIG`, `$XDG_CONFIG_HOME/koma/init.lua`,
/// `~/.config/koma/init.lua`, then `/etc/xdg/koma/init.lua`.
#[must_use]
pub(crate) fn default_init_lua_path() -> PathBuf {
    let cwd = env::current_dir().ok();
    let home = koma_lua::path_resolve::home_dir_from_env();
    if let Some(explicit) = env::var_os("KOMA_CONFIG").filter(|v| !v.is_empty()) {
        return koma_lua::path_resolve::resolve_config_file_path(
            explicit,
            cwd.as_deref(),
            home.as_deref(),
        );
    }
    if let Some(xdg) = env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        return koma_lua::path_resolve::resolve_config_file_path(
            xdg,
            cwd.as_deref(),
            home.as_deref(),
        )
        .join("koma/init.lua");
    }
    home.map(|h| h.join(".config/koma/init.lua"))
        .unwrap_or_else(|| PathBuf::from("/etc/xdg/koma/init.lua"))
}

/// Template freshness reported by `komactl config-status` and used by `komactl doctor`.
#[must_use]
pub(crate) fn template_report_at(init_lua: &Path) -> Value {
    let present = init_lua.exists();
    let installed = if present {
        koma_lua::installed_template_version(init_lua)
            .ok()
            .flatten()
    } else {
        None
    };
    let outdated = installed.is_some_and(|version| version < koma_lua::TEMPLATE_VERSION);
    let config_root = init_lua.parent().unwrap_or_else(|| Path::new("."));
    let missing_files: Vec<&str> = koma_lua::render_config_template()
        .into_iter()
        .map(|file| file.relative_path)
        .filter(|relative| !config_root.join(relative).exists())
        .collect();
    json!({
        "init_lua": init_lua.display().to_string(),
        "present": present,
        "installed_version": installed,
        "shipped_version": koma_lua::TEMPLATE_VERSION,
        "outdated": outdated,
        "missing_files": missing_files,
    })
}

/// Reads and evaluates `path` in a jailed shadow VM without touching the running compositor.
pub(crate) fn evaluate(path: &Path) -> Result<ConfigEvaluation, String> {
    let path = resolve_cli_path(path);
    let source = fs::read_to_string(&path)
        .map_err(|e| format!("failed reading `{}`: {e}", path.display()))?;
    let config_root = koma_lua::path_resolve::config_file_directory(&path);
    let mut vm = PhodopusSandbox::new(DEFAULT_HEAP_LIMIT_BYTES, Some(&config_root))?;
    let (registry, eval_error) = match vm.eval_init_lua(&source) {
        Ok(registry) => (Some(registry), None),
        Err(err) => (None, Some(err)),
    };
    let semantic_issues = registry
        .as_ref()
        .map(semantic_issues_of)
        .unwrap_or_default();
    Ok(ConfigEvaluation {
        path,
        config_root,
        source,
        registry,
        eval_error,
        semantic_issues,
    })
}

/// Checks the declarative rules that the Lua bridge accepts but the compositor compiles later.
#[must_use]
pub(crate) fn semantic_issues_of(registry: &KomaConfigRegistry) -> Vec<SemanticIssue> {
    let mut issues = Vec::new();

    for (idx, rule) in registry.window_rules.iter().enumerate() {
        for matcher in rule.matches.iter().chain(rule.excludes.iter()) {
            if let Some(pattern) = &matcher.app_id
                && let Err(err) = Regex::new(pattern)
            {
                issues.push(SemanticIssue {
                    check: "rule.window-regex",
                    message: format!(
                        "window rule #{idx} invalid `app_id` regex `{pattern}`: {err}"
                    ),
                });
            }
            if let Some(pattern) = &matcher.title
                && let Err(err) = Regex::new(pattern)
            {
                issues.push(SemanticIssue {
                    check: "rule.window-regex",
                    message: format!("window rule #{idx} invalid `title` regex `{pattern}`: {err}"),
                });
            }
        }
    }

    for (idx, rule) in registry.layer_rules.iter().enumerate() {
        for matcher in rule.matches.iter().chain(rule.excludes.iter()) {
            if let Some(pattern) = &matcher.namespace
                && let Err(err) = Regex::new(pattern)
            {
                issues.push(SemanticIssue {
                    check: "rule.layer-regex",
                    message: format!(
                        "layer rule #{idx} invalid `namespace` regex `{pattern}`: {err}"
                    ),
                });
            }
        }
    }

    for (idx, gesture) in registry.gestures.iter().enumerate() {
        if let Err(err) =
            koma_lua::validate_gesture_spec(i64::from(gesture.fingers), &gesture.direction)
        {
            issues.push(SemanticIssue {
                check: "input.gesture",
                message: format!("gesture #{idx} invalid: {err}"),
            });
        }
    }

    issues
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::atomic::{AtomicU32, Ordering},
    };

    use super::*;

    static NEXT_ID: AtomicU32 = AtomicU32::new(0);

    fn temp_root(label: &str) -> PathBuf {
        let base = Path::new("/tmp/koma");
        fs::create_dir_all(base).expect("create /tmp/koma");
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let dir = base.join(format!("probe-{label}-{}-{id}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create fixture dir");
        dir
    }

    #[cfg(unix)]
    #[test]
    fn evaluate_reads_config_through_symlink_parent_with_non_utf8_filename() {
        use std::{ffi::OsStr, os::unix::ffi::OsStrExt, os::unix::fs::symlink};

        let root = temp_root("symlink-parent");
        let real = root.join("real");
        let filename = OsStr::from_bytes(b"init-\xff.lua");
        fs::create_dir_all(real.join("subdir")).expect("create target directory");
        symlink("real/subdir", root.join("link")).expect("make directory symlink");
        fs::write(root.join(filename), "error('wrong config')").expect("write decoy config");
        fs::create_dir(root.join("lua")).expect("create decoy module directory");
        fs::write(root.join("lua/marker.lua"), "return 'decoy'").expect("write decoy module");
        fs::create_dir(real.join("lua")).expect("create module directory");
        let real_module = real.join("lua/marker.lua");
        fs::write(&real_module, "return 'real'").expect("write expected module");
        let real_module = real_module.canonicalize().expect("resolve real module");
        let source = format!(
            "local marker = require('./marker')\nassert(marker == 'real')\nassert(require({:?}) == marker)",
            real_module.to_str().expect("UTF-8 test module path")
        );
        fs::write(real.join(filename), &source).expect("write expected config");

        let evaluation = evaluate(&root.join("link/..").join(filename)).expect("read config");
        assert_eq!(evaluation.source, source);
        assert_eq!(
            evaluation.config_root,
            real.canonicalize().expect("resolve real root")
        );
        assert!(
            evaluation.validated_summary().is_some(),
            "{:?}",
            evaluation.error_messages()
        );
        fs::remove_dir_all(root).expect("remove config fixture");
    }

    #[test]
    fn invalid_window_rule_regex_is_rejected_by_the_bridge() {
        let root = temp_root("regex");
        let init = root.join("init.lua");
        fs::write(
            &init,
            "koma.rule.window({ matches = { { app_id = \"[unclosed\" } } })\n",
        )
        .expect("write init.lua");
        let evaluation = evaluate(&init).expect("read config");
        assert!(
            evaluation.eval_error.is_some(),
            "the Lua bridge validates rule regexes during evaluation"
        );
        assert!(evaluation.validated_summary().is_none());
        assert!(!evaluation.error_messages().is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn semantic_issues_still_flag_patterns_and_gestures_defensively() {
        use koma_lua::schema::{LuaLayerMatchInput, LuaWindowMatchInput};
        use koma_lua::{ActionDescriptor, GestureSpec, LuaLayerRuleSpec, LuaWindowRuleSpec};

        let mut registry = KomaConfigRegistry::default();
        registry.window_rules.push(LuaWindowRuleSpec {
            matches: vec![LuaWindowMatchInput {
                app_id: Some("[unclosed".to_owned()),
                title: Some("(also broken".to_owned()),
                ..LuaWindowMatchInput::default()
            }],
            ..LuaWindowRuleSpec::default()
        });
        registry.layer_rules.push(LuaLayerRuleSpec {
            matches: vec![LuaLayerMatchInput {
                namespace: Some("[unclosed".to_owned()),
                ..LuaLayerMatchInput::default()
            }],
            ..LuaLayerRuleSpec::default()
        });
        registry.gestures.push(GestureSpec {
            id: None,
            enabled: true,
            fingers: 9,
            direction: "sideways".to_owned(),
            action: ActionDescriptor::WindowClose,
            cooldown_ms: None,
            allow_when_locked: false,
            when_preempted: "allow".to_owned(),
            desc: None,
        });

        let checks: Vec<&str> = semantic_issues_of(&registry)
            .iter()
            .map(|issue| issue.check)
            .collect();
        assert_eq!(
            checks,
            vec![
                "rule.window-regex",
                "rule.window-regex",
                "rule.layer-regex",
                "input.gesture"
            ]
        );
    }

    #[test]
    fn evaluation_preserves_sandbox_errors_without_failing_the_read() {
        let root = temp_root("syntax");
        let init = root.join("init.lua");
        fs::write(&init, "koma.layout(\n").expect("write init.lua");
        let evaluation = evaluate(&init).expect("reading must succeed");
        assert!(evaluation.registry.is_none());
        assert!(evaluation.eval_error.is_some());
        assert!(evaluation.validated_summary().is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn unreadable_path_is_a_caller_error() {
        let err = evaluate(Path::new("/tmp/koma/definitely-absent/init.lua"))
            .expect_err("missing file must be reported");
        assert!(err.contains("failed reading"));
    }
}
