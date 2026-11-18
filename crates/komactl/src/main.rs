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
    DEFAULT_HEAP_LIMIT_BYTES, generate_luals_stubs, phodopus_bridge::PhodopusSandbox,
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

/// Offline Shadow-VM validator (`komactl validate [path]`, modeled on `niri validate`).
fn validate_init_lua_offline(path: &Path) -> Result<(), String> {
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

    println!(
        "init.lua valid ({}) — {} monitor(s), {} workspace(s), {} bind(s), {} gesture(s), {} rule(s), {} macro(s)",
        path.display(),
        registry.monitors.len(),
        registry.workspaces.len(),
        registry.binds.len(),
        registry.gestures.len(),
        registry.window_rules.len(),
        registry.macros.len()
    );
    Ok(())
}

/// Exit status for usage errors (no or unknown subcommand).
const USAGE_EXIT_CODE: i32 = 2;

fn usage_text() -> &'static str {
    "Usage:
  komactl validate [path/to/init.lua]
  komactl reload [path/to/init.lua]
  komactl config-status
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
            if let Err(err) = validate_init_lua_offline(&target) {
                eprintln!("komactl validate failed: {err}");
                process::exit(1);
            }
        }
        Some("reload") => {
            let explicit_path = args.get(1).cloned();
            let mut client = AdminClient::connect()?;
            let (val, _) = client.call_raw("config.reload", json!({ "path": explicit_path }))?;
            println!("{}", serde_json::to_string_pretty(&val).unwrap());
        }
        Some("config-status") => {
            let mut client = AdminClient::connect()?;
            let (val, _) = client.call_raw("config.status", json!({}))?;
            println!("{}", serde_json::to_string_pretty(&val).unwrap());
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
                .unwrap_or_else(|| "koma-mcp".to_owned());
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
mod usage_tests {
    use super::usage_text;

    #[test]
    fn usage_lists_every_subcommand_and_help() {
        let text = usage_text();
        for cmd in [
            "validate",
            "reload",
            "config-status",
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
}
