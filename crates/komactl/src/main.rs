#![forbid(unsafe_code)]

//! `adb`-inspired CLI controller, config validator/reloader & sandbox container launcher for `Koma`.

mod admin_rpc;
mod completions;
mod sandbox_runner;

use std::{
    env, fs,
    fs::File,
    io::{self, Read as _, Write as _},
    path::{Path, PathBuf},
    process,
};

use koma_lua::{
    DEFAULT_HEAP_LIMIT_BYTES, TEMPLATE_VERSION, generate_luals_stubs, installed_template_version,
    phodopus_bridge::PhodopusSandbox, render_config_template, scaffold_config_tree,
    validate_gesture_spec,
};
use regex::Regex;
use serde_json::json;

use crate::{
    admin_rpc::AdminClient,
    sandbox_runner::{dispatch_sandbox_subcommand, print_sandbox_help, print_sandbox_help_to},
};

fn default_init_lua_path() -> PathBuf {
    env::var_os("KOMA_CONFIG")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("XDG_CONFIG_HOME")
                .filter(|v| !v.is_empty())
                .map(|p| PathBuf::from(p).join("koma/init.lua"))
        })
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/koma/init.lua")))
        .unwrap_or_else(|| PathBuf::from("/etc/xdg/koma/init.lua"))
}

/// Offline Shadow-VM validation summary (`komactl validate [path]`, modeled on `niri validate`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ValidationSummary {
    monitors: usize,
    workspaces: usize,
    binds: usize,
    gestures: usize,
    rules: usize,
    macros: usize,
}

fn validate_init_lua_offline(path: &Path) -> Result<ValidationSummary, String> {
    let src = fs::read_to_string(path)
        .map_err(|e| format!("failed reading `{}`: {e}", path.display()))?;
    let cfg_dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut vm = PhodopusSandbox::new(DEFAULT_HEAP_LIMIT_BYTES, Some(cfg_dir))?;
    let registry = vm.eval_init_lua(&src)?;

    for (idx, rule) in registry.window_rules.iter().enumerate() {
        for m in rule.matches.iter().chain(rule.excludes.iter()) {
            if let Some(pat) = &m.app_id {
                Regex::new(pat)
                    .map_err(|e| format!("window rule #{idx} invalid app_id regex `{pat}`: {e}"))?;
            }
            if let Some(pat) = &m.title {
                Regex::new(pat)
                    .map_err(|e| format!("window rule #{idx} invalid title regex `{pat}`: {e}"))?;
            }
        }
    }

    for (idx, g) in registry.gestures.iter().enumerate() {
        validate_gesture_spec(i64::from(g.fingers), &g.direction)
            .map_err(|e| format!("gesture #{idx} invalid: {e}"))?;
    }

    Ok(ValidationSummary {
        monitors: registry.monitors.len(),
        workspaces: registry.workspaces.len(),
        binds: registry.binds.len(),
        gestures: registry.gestures.len(),
        rules: registry.window_rules.len(),
        macros: registry.macros.len(),
    })
}

/// Template freshness reported by `komactl config-status`.
fn template_report_at(init_lua: &Path) -> serde_json::Value {
    let present = init_lua.exists();
    let installed = if present {
        installed_template_version(init_lua).ok().flatten()
    } else {
        None
    };
    let outdated = installed.is_some_and(|version| version < TEMPLATE_VERSION);
    let config_root = init_lua.parent().unwrap_or_else(|| Path::new("."));
    let missing_files: Vec<&str> = render_config_template()
        .into_iter()
        .map(|file| file.relative_path)
        .filter(|relative| !config_root.join(relative).exists())
        .collect();
    json!({
        "init_lua": init_lua.display().to_string(),
        "present": present,
        "installed_version": installed,
        "shipped_version": TEMPLATE_VERSION,
        "outdated": outdated,
        "missing_files": missing_files,
    })
}

/// Reports an outdated scaffold on stderr; returns `true` when the on-disk
/// template is older than the shipped one.
fn warn_if_template_outdated(report: &serde_json::Value) -> bool {
    let outdated = report
        .get("outdated")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if outdated {
        let installed = report
            .get("installed_version")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        eprintln!(
            "komactl config-status: on-disk template v{installed} is older than the shipped \
             v{TEMPLATE_VERSION}; review the generated tree and refresh it with \
             `komactl init --force` if you want the new template"
        );
    }
    outdated
}

/// Exit status for usage errors (no or unknown subcommand).
const USAGE_EXIT_CODE: i32 = 2;

fn usage_text() -> &'static str {
    "Usage:
  komactl validate [path/to/init.lua]
  komactl reload [path/to/init.lua]
  komactl config-status
  komactl init [dir] [--force]
  komactl gen-types
  komactl inventory
  komactl ws focus <ws_id|name>
  komactl win move-to-workspace <ws_id|name> [--follow]
  komactl token mint [label]
  komactl capture [ws_id] [out.rgba]
  komactl sandbox <run|create-jail|revoke-jail> [OPTIONS]
  komactl completions <bash|zsh|fish>
  komactl -h | --help
  komactl -V | --version
"
}

fn main() -> io::Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("completions") {
        let Some(name) = args.get(1) else {
            eprintln!("usage: komactl completions <bash|zsh|fish>");
            process::exit(2);
        };
        let Some(shell) = completions::Shell::parse(name) else {
            eprintln!(
                "unknown shell `{name}` (expected {})",
                [
                    completions::Shell::Bash.name(),
                    completions::Shell::Zsh.name(),
                    completions::Shell::Fish.name()
                ]
                .join(", ")
            );
            process::exit(2);
        };
        print!("{}", completions::render(shell));
        return Ok(());
    }

    match args.first().map(String::as_str) {
        Some("gen-types") => {
            print!("{}", generate_luals_stubs());
        }
        Some("validate") => {
            let target = args
                .get(1)
                .map(PathBuf::from)
                .unwrap_or_else(default_init_lua_path);
            match validate_init_lua_offline(&target) {
                Ok(summary) => println!(
                    "init.lua valid ({}) — {} monitor(s), {} workspace(s), {} bind(s), {} gesture(s), {} rule(s), {} macro(s)",
                    target.display(),
                    summary.monitors,
                    summary.workspaces,
                    summary.binds,
                    summary.gestures,
                    summary.rules,
                    summary.macros
                ),
                Err(err) => {
                    eprintln!("komactl validate failed: {err}");
                    process::exit(1);
                }
            }
        }
        Some("reload") => {
            let explicit_path = args.get(1).cloned();
            let mut client = AdminClient::connect()?;
            let (val, _) = client.call_raw("config.reload", json!({ "path": explicit_path }))?;
            println!("{}", serde_json::to_string_pretty(&val).unwrap());
        }
        Some("config-status") => {
            let template = template_report_at(&default_init_lua_path());
            warn_if_template_outdated(&template);
            let mut report = json!({ "template": template });
            match AdminClient::connect() {
                Ok(mut client) => match client.call_raw("config.status", json!({})) {
                    Ok((val, _)) => {
                        report["compositor"] = val;
                        println!("{}", serde_json::to_string_pretty(&report).unwrap());
                    }
                    Err(err) => {
                        report["error"] = json!(err.to_string());
                        println!("{}", serde_json::to_string_pretty(&report).unwrap());
                        process::exit(1);
                    }
                },
                Err(err) => {
                    report["error"] = json!(err.to_string());
                    println!("{}", serde_json::to_string_pretty(&report).unwrap());
                    process::exit(1);
                }
            }
        }
        Some("init") => {
            let mut force = false;
            let mut target_dir: Option<PathBuf> = None;
            for arg in &args[1..] {
                match arg.as_str() {
                    "--force" => force = true,
                    other if other.starts_with('-') => {
                        eprintln!("komactl init: unknown option `{other}`\n\n{}", usage_text());
                        process::exit(USAGE_EXIT_CODE);
                    }
                    other if target_dir.is_some() => {
                        eprintln!(
                            "komactl init: unexpected argument `{other}` (target directory already given)\n"
                        );
                        process::exit(USAGE_EXIT_CODE);
                    }
                    other => target_dir = Some(PathBuf::from(other)),
                }
            }
            let root = target_dir.unwrap_or_else(|| {
                default_init_lua_path()
                    .parent()
                    .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
            });
            match scaffold_config_tree(&root, force) {
                Ok(written) => {
                    for path in &written {
                        println!("{}", path.display());
                    }
                    println!(
                        "scaffolded {} file(s) under {} (template v{TEMPLATE_VERSION})",
                        written.len(),
                        root.display()
                    );
                    println!("next: komactl validate && komactl reload");
                }
                Err(err) => {
                    eprintln!("komactl init: {err}");
                    process::exit(1);
                }
            }
        }
        Some("inventory") => {
            let mut client = AdminClient::connect()?;
            let (val, _) = client.call_raw("scene.inventory", json!({}))?;
            println!("{}", serde_json::to_string_pretty(&val).unwrap());
        }
        Some("ws") => match args.get(1).map(String::as_str) {
            Some("focus") => {
                let Some(target) = args.get(2).cloned() else {
                    eprintln!("komactl ws focus: missing <ws_id|name>");
                    process::exit(2);
                };
                let mut client = AdminClient::connect()?;
                let (val, _) = client.call_raw("ws.focus", json!({ "ws_id": target }))?;
                println!("{}", serde_json::to_string_pretty(&val).unwrap());
            }
            other => {
                eprintln!(
                    "komactl ws: unknown subcommand {:?} (expected: focus <ws_id|name>)",
                    other.unwrap_or("")
                );
                process::exit(2);
            }
        },
        Some("win") => match args.get(1).map(String::as_str) {
            Some("move-to-workspace") => {
                let Some(target) = args.get(2).cloned() else {
                    eprintln!("komactl win move-to-workspace: missing <ws_id|name>");
                    process::exit(2);
                };
                let follow = args.iter().skip(3).any(|arg| arg == "--follow");
                let mut client = AdminClient::connect()?;
                let (val, _) = client.call_raw(
                    "win.move_to_workspace",
                    json!({ "workspace": target, "follow": follow }),
                )?;
                println!("{}", serde_json::to_string_pretty(&val).unwrap());
            }
            other => {
                eprintln!(
                    "komactl win: unknown subcommand {:?} (expected: move-to-workspace <ws_id|name> [--follow])",
                    other.unwrap_or("")
                );
                process::exit(2);
            }
        },
        Some("token") if args.get(1).map(String::as_str) == Some("mint") => {
            let label = args
                .get(2)
                .cloned()
                .unwrap_or_else(|| "koma mcp".to_owned());
            let mut client = AdminClient::connect()?;
            let (val, _) = client.call_raw(
                "auth.mint_token",
                json!({
                    "label": label,
                    "ttl_secs": 28_800,
                    "can_create_headless": true,
                    "max_owned_workspaces": 4,
                }),
            )?;
            println!("{}", serde_json::to_string_pretty(&val).unwrap());
        }
        Some("capture") => {
            let ws_id = args.get(1).cloned().unwrap_or_else(|| "ws_01".to_owned());
            let out_path = args
                .get(2)
                .cloned()
                .unwrap_or_else(|| "capture.rgba".to_owned());
            let mut client = AdminClient::connect()?;
            let (meta, mut fds) = client.call_raw(
                "capture.frame",
                json!({
                    "ws_id": ws_id,
                    "win_id": null,
                    "overlay_kind": "grid_225",
                    "max_edge": 1920,
                    "format": "raw_rgba8",
                    "transport": "shm_fd",
                }),
            )?;
            let fd = fds.pop().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "missing SCM_RIGHTS memfd in reply",
                )
            })?;
            let mut shm_file = File::from(fd);
            let mut raw = Vec::new();
            shm_file.read_to_end(&mut raw)?;
            let mut dst = File::create(&out_path)?;
            dst.write_all(&raw)?;
            println!(
                "saved {} bytes from zero-copy sealed memfd to {} (meta: {})",
                raw.len(),
                out_path,
                meta
            );
        }
        Some("sandbox") => match dispatch_sandbox_subcommand(&args[1..]) {
            Ok(code) => process::exit(code),
            Err(err) => {
                eprintln!("komactl sandbox error: {err}");
                process::exit(1);
            }
        },
        Some("-h" | "--help" | "help") => {
            print!("{}", usage_text());
            println!();
            print_sandbox_help_to(&mut io::stdout())?;
        }
        Some("-V" | "--version") => println!("komactl {}", env!("CARGO_PKG_VERSION")),
        other => {
            if let Some(cmd) = other {
                eprintln!("komactl: unknown command `{cmd}`\n");
            }
            eprint!("{}", usage_text());
            eprintln!();
            print_sandbox_help();
            process::exit(USAGE_EXIT_CODE);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU32, Ordering},
    };

    use koma_lua::{KEYBIND_CATALOG, scaffold_config_tree};

    use super::{template_report_at, usage_text, validate_init_lua_offline};

    static NEXT_ID: AtomicU32 = AtomicU32::new(0);

    fn temp_root(label: &str) -> PathBuf {
        let base = Path::new("/tmp/koma");
        fs::create_dir_all(base).expect("create /tmp/koma");
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let dir = base.join(format!("komactl-{label}-{}-{id}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn usage_lists_every_subcommand_and_help() {
        let text = usage_text();
        for cmd in [
            "validate",
            "reload",
            "config-status",
            "init",
            "gen-types",
            "inventory",
            "token mint",
            "capture",
            "sandbox",
            "--help",
            "--version",
        ] {
            assert!(text.contains(cmd), "usage is missing {cmd}");
        }
    }

    #[test]
    fn scaffolded_tree_validates_with_the_full_builtin_keymap() {
        let root = temp_root("validate");
        scaffold_config_tree(&root, false).expect("scaffold");
        let summary = validate_init_lua_offline(&root.join("init.lua"))
            .expect("a freshly scaffolded tree must validate");

        assert_eq!(
            summary.binds,
            KEYBIND_CATALOG.len(),
            "the generated keymap must spell out every built-in bind"
        );
        assert_eq!(
            summary.monitors, 0,
            "the scaffold must not require a koma.monitor block"
        );
        assert!(summary.workspaces >= 5);
        assert!(summary.rules >= 1);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn re_running_init_on_the_scaffold_requires_force() {
        let root = temp_root("force");
        scaffold_config_tree(&root, false).expect("first scaffold");
        let err = scaffold_config_tree(&root, false).expect_err("must refuse");
        assert!(err.to_string().contains("--force"));
        scaffold_config_tree(&root, true).expect("force refresh");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn template_report_flags_an_older_on_disk_marker() {
        let root = temp_root("status");
        fs::create_dir_all(root.join("lua")).expect("create lua dir");
        fs::write(
            root.join("init.lua"),
            "-- koma-config-template: 0\nrequire(\"input\")\n",
        )
        .expect("write init.lua");

        let report = template_report_at(&root.join("init.lua"));
        assert_eq!(report["present"], true);
        assert_eq!(report["installed_version"], 0);
        assert_eq!(report["shipped_version"], koma_lua::TEMPLATE_VERSION);
        assert_eq!(report["outdated"], true);
        assert!(
            report["missing_files"]
                .as_array()
                .is_some_and(|files| files.iter().any(|file| file == "lua/keybinds.lua")),
            "report must list missing generated files"
        );

        let missing = template_report_at(&root.join("absent/init.lua"));
        assert_eq!(missing["present"], false);
        assert_eq!(missing["installed_version"], serde_json::Value::Null);
        assert_eq!(missing["outdated"], false);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn shipped_scaffold_is_not_reported_as_outdated() {
        let root = temp_root("current");
        scaffold_config_tree(&root, false).expect("scaffold");
        let report = template_report_at(&root.join("init.lua"));
        assert_eq!(report["outdated"], false);
        assert!(
            report["missing_files"]
                .as_array()
                .is_some_and(Vec::is_empty),
            "a complete scaffold has no missing files"
        );
        let _ = fs::remove_dir_all(&root);
    }
}
