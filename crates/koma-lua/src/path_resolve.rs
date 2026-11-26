//! Path-resolution helpers for Koma configuration paths, spawn argvs,
//! environment variables, and jailed `require()` module targets.

use std::{
    borrow::Cow,
    ffi::OsStr,
    path::{Component, Path, PathBuf},
};

const SHELL_INTERPRETERS: &[&str] = &[
    "sh", "bash", "fish", "zsh", "dash", "python", "python3", "env",
];

/// Returns `$HOME` when set and non-empty.
#[must_use]
pub fn home_dir_from_env() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Expands leading `"~"` to `home` and `"~/..."` to `<home>/...`.
///
/// Leaves `"~other/..."` and any non-tilde string untouched; if `home` is `None` or empty,
/// returns `raw` unchanged.
#[must_use]
pub fn expand_tilde(raw: &str, home: Option<&Path>) -> String {
    expand_tilde_path(Path::new(raw), home)
        .to_string_lossy()
        .into_owned()
}

fn expand_tilde_path<'a>(raw: &'a Path, home: Option<&'a Path>) -> Cow<'a, Path> {
    let Some(home) = home.filter(|h| !h.as_os_str().is_empty()) else {
        return Cow::Borrowed(raw);
    };
    let spelling = raw.as_os_str();
    if spelling == "~" {
        return Cow::Borrowed(home);
    }
    if let Some(suffix) = tilde_path_suffix(spelling) {
        return Cow::Owned(home.join(suffix));
    }
    Cow::Borrowed(raw)
}

#[cfg(unix)]
fn tilde_path_suffix(raw: &OsStr) -> Option<&OsStr> {
    use std::os::unix::ffi::OsStrExt;

    let suffix = raw.as_bytes().strip_prefix(b"~/")?;
    let extra_slashes = suffix.iter().take_while(|&&byte| byte == b'/').count();
    Some(OsStr::from_bytes(&suffix[extra_slashes..]))
}

#[cfg(not(unix))]
fn tilde_path_suffix(raw: &OsStr) -> Option<&OsStr> {
    let suffix = raw.to_str()?.strip_prefix("~/")?;
    Some(OsStr::new(suffix.trim_start_matches('/')))
}

/// Convenience wrapper over [`expand_tilde`] reading `HOME` from the process environment.
#[must_use]
pub fn expand_tilde_from_env(raw: &str) -> String {
    expand_tilde(raw, home_dir_from_env().as_deref())
}

/// Lexically normalizes `.` and `..` components without touching the filesystem.
#[must_use]
pub fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out: Vec<Component<'_>> = Vec::new();
    let mut has_root = false;

    for comp in path.components() {
        match comp {
            Component::Prefix(p) => out.push(Component::Prefix(p)),
            Component::RootDir => {
                has_root = true;
                out.push(Component::RootDir);
            }
            Component::CurDir => {}
            Component::ParentDir => match out.last() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::RootDir) => {}
                _ if has_root => {}
                _ => out.push(Component::ParentDir),
            },
            Component::Normal(seg) => out.push(Component::Normal(seg)),
        }
    }

    if out.is_empty() {
        PathBuf::from(".")
    } else {
        out.iter().collect()
    }
}

/// Expands `~` / `~/...` first, then resolves relative paths against `config_dir` (if `Some`),
/// normalizing `.` and `..` lexically.
#[must_use]
pub fn resolve_config_path(raw: &str, config_dir: Option<&Path>, home: Option<&Path>) -> PathBuf {
    let expanded = expand_tilde(raw, home);
    let path = Path::new(&expanded);
    if path.is_relative()
        && !expanded.starts_with('~')
        && let Some(dir) = config_dir
    {
        return normalize_lexical(&dir.join(path));
    }
    normalize_lexical(path)
}

/// Convenience wrapper over [`resolve_config_path`] reading `HOME` from the process environment.
#[must_use]
pub fn resolve_config_path_from_env(raw: &str, config_dir: Option<&Path>) -> PathBuf {
    resolve_config_path(raw, config_dir, home_dir_from_env().as_deref())
}

/// Expands `~` and anchors a relative config file path without changing filesystem traversal.
///
/// The filesystem resolves `..` after following any preceding symlinks. Keep these components
/// and the original path encoding intact when selecting a config file.
#[must_use]
pub fn resolve_config_file_path(
    raw: impl AsRef<Path>,
    config_dir: Option<&Path>,
    home: Option<&Path>,
) -> PathBuf {
    let path = expand_tilde_path(raw.as_ref(), home);
    if path.is_relative()
        && !path.as_os_str().as_encoded_bytes().starts_with(b"~")
        && let Some(dir) = config_dir
    {
        return dir.join(path.as_ref());
    }
    path.into_owned()
}

/// Convenience wrapper over [`resolve_config_file_path`] reading `HOME` from the environment.
#[must_use]
pub fn resolve_config_file_path_from_env(
    raw: impl AsRef<Path>,
    config_dir: Option<&Path>,
) -> PathBuf {
    resolve_config_file_path(raw, config_dir, home_dir_from_env().as_deref())
}

/// Selects the directory used for paths and jailed modules in a config.
///
/// When the file path has a parent traversal, resolve the directory before Lua applies its
/// lexical path rules. Keep a symlink on the config file itself independent of this directory.
#[must_use]
pub fn config_file_directory(path: &Path) -> PathBuf {
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    if directory
        .components()
        .any(|part| part == Component::ParentDir)
        && let Ok(resolved) = directory.canonicalize()
    {
        return resolved;
    }
    directory.to_path_buf()
}

fn resolve_spawn_arg_token(
    arg: &str,
    config_dir: Option<&Path>,
    home: Option<&Path>,
    is_interpreter: bool,
) -> String {
    if arg.starts_with("--")
        && let Some((flag, value)) = arg.split_once('=')
    {
        if value == "~" || value.starts_with("~/") {
            return format!("{flag}={}", expand_tilde(value, home));
        }
        if (value == "." || value == ".." || value.starts_with("./") || value.starts_with("../"))
            && let Some(dir) = config_dir
        {
            return format!("{flag}={}", normalize_lexical(&dir.join(value)).display());
        }
        return arg.to_owned();
    }

    if arg == "~" || arg.starts_with("~/") {
        return expand_tilde(arg, home);
    }
    if (arg == "." || arg == ".." || arg.starts_with("./") || arg.starts_with("../"))
        && let Some(dir) = config_dir
    {
        return normalize_lexical(&dir.join(arg))
            .to_string_lossy()
            .into_owned();
    }
    if is_interpreter
        && !arg.starts_with('-')
        && !arg.starts_with('~')
        && !arg.contains("://")
        && !Path::new(arg).is_absolute()
        && arg.contains('/')
        && let Some(dir) = config_dir
        && dir.join(arg).exists()
    {
        return normalize_lexical(&dir.join(arg))
            .to_string_lossy()
            .into_owned();
    }

    arg.to_owned()
}

/// Resolves tilde and config-directory-relative paths in a spawn `argv` vector.
///
/// Bare program names in `argv[0]` without `'/'` (e.g. `"bitty"`, `"bash"`) are preserved
/// unchanged for `PATH` lookup.
#[must_use]
pub fn resolve_spawn_argv(
    argv: &[String],
    config_dir: Option<&Path>,
    home: Option<&Path>,
) -> Vec<String> {
    let Some(first) = argv.first() else {
        return Vec::new();
    };

    let resolved_first = if first == "~" || first.starts_with("~/") {
        expand_tilde(first, home)
    } else if first.starts_with('~') {
        first.clone()
    } else if !Path::new(first).is_absolute() && first.contains('/') && !first.contains("://") {
        if let Some(dir) = config_dir {
            normalize_lexical(&dir.join(first))
                .to_string_lossy()
                .into_owned()
        } else {
            first.clone()
        }
    } else {
        first.clone()
    };

    let is_interpreter = Path::new(&resolved_first)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| SHELL_INTERPRETERS.contains(&name));

    let mut out = Vec::with_capacity(argv.len());
    out.push(resolved_first);
    let mut skip_next_shell_cmd = false;
    for arg in &argv[1..] {
        if skip_next_shell_cmd {
            out.push(arg.clone());
            skip_next_shell_cmd = false;
            continue;
        }
        if is_interpreter
            && arg.starts_with('-')
            && !arg.starts_with("--")
            && arg.chars().skip(1).any(|c| c == 'c')
        {
            skip_next_shell_cmd = true;
            out.push(arg.clone());
            continue;
        }
        out.push(resolve_spawn_arg_token(
            arg,
            config_dir,
            home,
            is_interpreter,
        ));
    }
    out
}

/// Convenience wrapper over [`resolve_spawn_argv`] reading `HOME` from the process environment.
#[must_use]
pub fn resolve_spawn_argv_from_env(argv: &[String], config_dir: Option<&Path>) -> Vec<String> {
    resolve_spawn_argv(argv, config_dir, home_dir_from_env().as_deref())
}

fn expand_env_segment(seg: &str, config_dir: Option<&Path>, home: Option<&Path>) -> String {
    if seg == "~" || seg.starts_with("~/") {
        return expand_tilde(seg, home);
    }
    if (seg.starts_with("./") || seg.starts_with("../"))
        && let Some(dir) = config_dir
    {
        return normalize_lexical(&dir.join(seg))
            .to_string_lossy()
            .into_owned();
    }
    seg.to_owned()
}

/// Expands `~` / `~/...`, `./...`, and `../...` in an environment variable value, including
/// individual segments of colon-separated path lists (when `key == "PATH"` or `raw` contains `:`
/// without `://`).
#[must_use]
pub fn resolve_env_value(
    key: &str,
    raw: &str,
    config_dir: Option<&Path>,
    home: Option<&Path>,
) -> String {
    if key == "PATH" || (raw.contains(':') && !raw.contains("://")) {
        raw.split(':')
            .map(|seg| expand_env_segment(seg, config_dir, home))
            .collect::<Vec<_>>()
            .join(":")
    } else {
        expand_env_segment(raw, config_dir, home)
    }
}

/// Convenience wrapper over [`resolve_env_value`] reading `HOME` from the process environment.
#[must_use]
pub fn resolve_env_value_from_env(key: &str, raw: &str, config_dir: Option<&Path>) -> String {
    resolve_env_value(key, raw, config_dir, home_dir_from_env().as_deref())
}

fn access_violation_err(raw: &str) -> String {
    format!("access violation: refusing module path '{raw}' (escapes jailed lua directory)")
}

/// Resolves path-like `require(name)` targets (`./lua/input`, `./input`, `~/.config/koma/lua/input`,
/// `lua/input`) into Phodopus's dot-separated module name while enforcing that the resolved path
/// stays strictly inside `<config_dir>/lua`.
///
/// Standard module names without `'/'` (e.g. `"input"`, `"macros.wps"`) are returned untouched.
pub fn resolve_require_target(
    raw: &str,
    config_dir: Option<&Path>,
    home: Option<&Path>,
) -> Result<String, String> {
    let is_path_like = raw.contains('/') || raw == "~" || raw == "." || raw == "..";
    if !is_path_like {
        return Ok(raw.to_owned());
    }

    let Some(cfg_dir) = config_dir else {
        return Err(access_violation_err(raw));
    };
    let cfg_dir = normalize_lexical(cfg_dir);
    let lua_dir = normalize_lexical(&cfg_dir.join("lua"));

    let expanded = expand_tilde(raw, home);
    if expanded.starts_with('~') {
        return Err(access_violation_err(raw));
    }

    let exp_path = Path::new(&expanded);
    let resolved = if exp_path.is_absolute() {
        normalize_lexical(exp_path)
    } else {
        let norm_rel = normalize_lexical(exp_path);
        if norm_rel.components().next() == Some(Component::ParentDir) {
            return Err(access_violation_err(raw));
        }

        let in_cfg = normalize_lexical(&cfg_dir.join(&norm_rel));
        if in_cfg.strip_prefix(&lua_dir).is_ok() {
            in_cfg
        } else if expanded.starts_with("./") {
            let in_lua = normalize_lexical(&lua_dir.join(&norm_rel));
            let cfg_target_exists = in_cfg.exists()
                || in_cfg.with_extension("lua").exists()
                || in_cfg.join("init.lua").exists();
            let lua_target_exists = in_lua.exists()
                || in_lua.with_extension("lua").exists()
                || in_lua.join("init.lua").exists();
            if cfg_target_exists && !lua_target_exists {
                in_cfg
            } else {
                in_lua
            }
        } else {
            in_cfg
        }
    };

    if let Ok(rel) = resolved.strip_prefix(&lua_dir) {
        let mut parts: Vec<String> = rel
            .iter()
            .map(|os| os.to_string_lossy().into_owned())
            .collect();
        if let Some(last) = parts.last_mut() {
            if last == "init.lua" {
                parts.pop();
            } else if let Some(stripped) = last.strip_suffix(".lua") {
                *last = stripped.to_owned();
            }
        }
        if !parts.is_empty() && !parts.iter().any(String::is_empty) {
            return Ok(parts.join("."));
        }
    }

    Err(access_violation_err(raw))
}

/// Convenience wrapper over [`resolve_require_target`] reading `HOME` from the process environment.
pub fn resolve_require_target_from_env(
    raw: &str,
    config_dir: Option<&Path>,
) -> Result<String, String> {
    resolve_require_target(raw, config_dir, home_dir_from_env().as_deref())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    static NEXT_ID: AtomicU32 = AtomicU32::new(0);

    fn temp_root(label: &str) -> PathBuf {
        let base = Path::new("/tmp/koma");
        fs::create_dir_all(base).expect("create /tmp/koma");
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let dir = base.join(format!("path-resolve-{label}-{}-{id}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create fixture dir");
        dir
    }

    #[test]
    fn expand_tilde_handles_bare_subpath_other_user_and_missing_home() {
        let home = Path::new("/home/hiro");
        assert_eq!(expand_tilde("~", Some(home)), "/home/hiro");
        assert_eq!(
            expand_tilde("~/scripts/bar.sh", Some(home)),
            "/home/hiro/scripts/bar.sh"
        );
        assert_eq!(
            expand_tilde("~//scripts/bar.sh", Some(home)),
            "/home/hiro/scripts/bar.sh"
        );
        assert_eq!(expand_tilde("~/", Some(home)), "/home/hiro/");
        assert_eq!(expand_tilde("~other/file", Some(home)), "~other/file");
        assert_eq!(expand_tilde("/abs/path", Some(home)), "/abs/path");
        assert_eq!(expand_tilde("~/scripts/bar.sh", None), "~/scripts/bar.sh");
        assert_eq!(expand_tilde("~", None), "~");
    }

    #[test]
    fn normalize_lexical_resolves_dot_and_dotdot_without_fs() {
        assert_eq!(
            normalize_lexical(Path::new("/a/b/../c/./d")),
            PathBuf::from("/a/c/d")
        );
        assert_eq!(
            normalize_lexical(Path::new("/../../a")),
            PathBuf::from("/a")
        );
        assert_eq!(normalize_lexical(Path::new("./a/b")), PathBuf::from("a/b"));
        assert_eq!(normalize_lexical(Path::new("a/../b")), PathBuf::from("b"));
        assert_eq!(normalize_lexical(Path::new("a/..")), PathBuf::from("."));
        assert_eq!(normalize_lexical(Path::new("../a")), PathBuf::from("../a"));
        assert_eq!(
            normalize_lexical(Path::new("a/../../b")),
            PathBuf::from("../b")
        );
        assert_eq!(normalize_lexical(Path::new("/")), PathBuf::from("/"));
        assert_eq!(normalize_lexical(Path::new(".")), PathBuf::from("."));
    }

    #[test]
    fn resolve_config_path_expands_tilde_and_config_relative_paths() {
        let cfg = Path::new("/home/hiro/.config/koma");
        let home = Path::new("/home/hiro");

        assert_eq!(
            resolve_config_path("~/.local/bin/tool", Some(cfg), Some(home)),
            PathBuf::from("/home/hiro/.local/bin/tool")
        );
        assert_eq!(
            resolve_config_path("./scripts/run.sh", Some(cfg), Some(home)),
            PathBuf::from("/home/hiro/.config/koma/scripts/run.sh")
        );
        assert_eq!(
            resolve_config_path("../shared/tool", Some(cfg), Some(home)),
            PathBuf::from("/home/hiro/.config/shared/tool")
        );
        assert_eq!(
            resolve_config_path("/usr/bin/xwayland-satellite", Some(cfg), Some(home)),
            PathBuf::from("/usr/bin/xwayland-satellite")
        );
        assert_eq!(
            resolve_config_path("~/foo", Some(cfg), None),
            PathBuf::from("~/foo")
        );
    }

    #[cfg(unix)]
    #[test]
    fn config_file_lookup_follows_symlink_before_parent_component() {
        let root = temp_root("config-symlink-parent");
        let config_dir = root.join("config");
        let real_dir = root.join("real");
        fs::create_dir(&config_dir).expect("create config directory");
        fs::create_dir_all(real_dir.join("subdir")).expect("create symlink target");
        std::os::unix::fs::symlink("../real/subdir", config_dir.join("linked"))
            .expect("create config symlink");
        fs::write(config_dir.join("init.lua"), "wrong config").expect("write alternate config");
        fs::write(real_dir.join("init.lua"), "expected config").expect("write expected config");

        for (raw, home) in [
            (Path::new("linked/../init.lua"), None),
            (
                Path::new("~//linked/../init.lua"),
                Some(config_dir.as_path()),
            ),
        ] {
            let resolved = resolve_config_file_path(raw, Some(&config_dir), home);
            assert_eq!(resolved, config_dir.join("linked/../init.lua"));
            assert_eq!(
                fs::read_to_string(resolved).expect("read resolved config"),
                "expected config"
            );
        }
        fs::remove_dir_all(root).expect("remove config fixture");
    }

    #[cfg(unix)]
    #[test]
    fn config_file_lookup_preserves_non_utf8_home_and_filename() {
        use std::{ffi::OsStr, os::unix::ffi::OsStrExt};

        let home = Path::new(OsStr::from_bytes(b"/home/config\xff"));
        let raw = Path::new(OsStr::from_bytes(b"~/init\xfe.lua"));
        let expected = Path::new(OsStr::from_bytes(b"/home/config\xff/init\xfe.lua"));
        assert_eq!(resolve_config_file_path(raw, None, Some(home)), expected);
    }

    #[test]
    fn resolve_spawn_argv_expands_program_args_flags_and_interpreter_scripts() {
        let root = temp_root("spawn");
        fs::create_dir_all(root.join("scripts")).expect("create scripts");
        fs::write(root.join("scripts/helper.py"), "print('hi')\n").expect("write helper");
        let home = Path::new("/home/hiro");

        assert_eq!(
            resolve_spawn_argv(
                &[
                    "./scripts/foo.sh".to_owned(),
                    "~/notes".to_owned(),
                    "./data.json".to_owned(),
                    "--config=~/app.conf".to_owned(),
                    "--out=../out.log".to_owned(),
                ],
                Some(&root),
                Some(home),
            ),
            vec![
                root.join("scripts/foo.sh").display().to_string(),
                "/home/hiro/notes".to_owned(),
                root.join("data.json").display().to_string(),
                "--config=/home/hiro/app.conf".to_owned(),
                format!(
                    "--out={}",
                    normalize_lexical(&root.join("../out.log")).display()
                ),
            ]
        );

        assert_eq!(
            resolve_spawn_argv(
                &["scripts/foo.sh".to_owned(), ".".to_owned(), "..".to_owned()],
                Some(&root),
                Some(home),
            ),
            vec![
                root.join("scripts/foo.sh").display().to_string(),
                root.display().to_string(),
                root.parent().unwrap().display().to_string(),
            ]
        );

        assert_eq!(
            resolve_spawn_argv(
                &["bitty".to_owned(), "--title=hello".to_owned()],
                Some(&root),
                Some(home),
            ),
            vec!["bitty".to_owned(), "--title=hello".to_owned()]
        );

        assert_eq!(
            resolve_spawn_argv(
                &[
                    "python3".to_owned(),
                    "scripts/helper.py".to_owned(),
                    "scripts/missing.py".to_owned(),
                    "https://example.com/a/b".to_owned(),
                ],
                Some(&root),
                Some(home),
            ),
            vec![
                "python3".to_owned(),
                root.join("scripts/helper.py").display().to_string(),
                "scripts/missing.py".to_owned(),
                "https://example.com/a/b".to_owned(),
            ]
        );

        assert_eq!(
            resolve_spawn_argv(
                &["~/bin/tool".to_owned(), "~".to_owned()],
                Some(&root),
                Some(home),
            ),
            vec!["/home/hiro/bin/tool".to_owned(), "/home/hiro".to_owned()]
        );

        assert_eq!(
            resolve_spawn_argv(
                &[
                    "sh".to_owned(),
                    "-c".to_owned(),
                    "./run.sh ../../input.txt".to_owned(),
                ],
                Some(&root),
                Some(home),
            ),
            vec![
                "sh".to_owned(),
                "-c".to_owned(),
                "./run.sh ../../input.txt".to_owned(),
            ]
        );
        assert_eq!(
            resolve_spawn_argv(
                &[
                    "/bin/bash".to_owned(),
                    "-lc".to_owned(),
                    ".".to_owned(),
                    "./extra-arg".to_owned(),
                ],
                Some(&root),
                Some(home),
            ),
            vec![
                "/bin/bash".to_owned(),
                "-lc".to_owned(),
                ".".to_owned(),
                root.join("extra-arg").display().to_string(),
            ]
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_env_value_handles_single_values_path_lists_and_urls() {
        let cfg = Path::new("/home/hiro/.config/koma");
        let home = Path::new("/home/hiro");

        assert_eq!(
            resolve_env_value("KOMA_SCRIPTS", "./scripts", Some(cfg), Some(home)),
            "/home/hiro/.config/koma/scripts"
        );
        assert_eq!(
            resolve_env_value("SHARED", "../shared", Some(cfg), Some(home)),
            "/home/hiro/.config/shared"
        );
        assert_eq!(
            resolve_env_value("DATA", "~/data", Some(cfg), Some(home)),
            "/home/hiro/data"
        );
        assert_eq!(
            resolve_env_value(
                "PATH",
                "~/.local/bin:./bin:../tools:/usr/bin",
                Some(cfg),
                Some(home)
            ),
            "/home/hiro/.local/bin:/home/hiro/.config/koma/bin:/home/hiro/.config/tools:/usr/bin"
        );
        assert_eq!(
            resolve_env_value(
                "XDG_DATA_DIRS",
                "~/share:./share:/usr/share",
                Some(cfg),
                Some(home)
            ),
            "/home/hiro/share:/home/hiro/.config/koma/share:/usr/share"
        );
        assert_eq!(
            resolve_env_value(
                "API_URL",
                "https://example.com:8443/v1",
                Some(cfg),
                Some(home)
            ),
            "https://example.com:8443/v1"
        );
        assert_eq!(
            resolve_env_value("XCURSOR_SIZE", "24", Some(cfg), Some(home)),
            "24"
        );
    }

    #[test]
    fn resolve_require_target_maps_valid_paths_and_rejects_escapes() {
        let root = temp_root("require");
        fs::create_dir_all(root.join("lua/sub")).expect("create lua/sub");
        fs::write(root.join("init.lua"), "").expect("write init.lua");
        fs::write(root.join("secret.lua"), "").expect("write secret.lua");
        fs::write(root.join("lua/input.lua"), "").expect("write lua/input.lua");
        fs::write(root.join("lua/sub/init.lua"), "").expect("write lua/sub/init.lua");
        let home = Path::new("/home/hiro");

        assert_eq!(
            resolve_require_target("input", Some(&root), Some(home)).as_deref(),
            Ok("input")
        );
        assert_eq!(
            resolve_require_target("./lua/input", Some(&root), Some(home)).as_deref(),
            Ok("input")
        );
        assert_eq!(
            resolve_require_target("./lua/input.lua", Some(&root), Some(home)).as_deref(),
            Ok("input")
        );
        assert_eq!(
            resolve_require_target("lua/input", Some(&root), Some(home)).as_deref(),
            Ok("input")
        );
        assert_eq!(
            resolve_require_target("./input", Some(&root), Some(home)).as_deref(),
            Ok("input")
        );
        assert_eq!(
            resolve_require_target("./lua/sub/init.lua", Some(&root), Some(home)).as_deref(),
            Ok("sub")
        );
        assert_eq!(
            resolve_require_target(
                &format!("{}/lua/input.lua", root.display()),
                Some(&root),
                Some(home),
            )
            .as_deref(),
            Ok("input")
        );

        let fake_cfg = home.join(".config/koma");
        assert_eq!(
            resolve_require_target("~/.config/koma/lua/input", Some(&fake_cfg), Some(home))
                .as_deref(),
            Ok("input")
        );

        for bad in [
            "../etc/passwd",
            "./../etc/passwd",
            "./../lua/input",
            "/etc/passwd",
            "~/secret.lua",
            "~",
            "./lua",
            "./lua/init.lua",
            "./init.lua",
            "./secret",
            "outside/mod",
        ] {
            let err = resolve_require_target(bad, Some(&root), Some(home))
                .expect_err(&format!("`{bad}` must be rejected"));
            assert!(
                err.contains("access violation") && err.contains("escapes jailed lua directory"),
                "unexpected error for `{bad}`: {err}"
            );
        }

        assert!(resolve_require_target("./input", None, Some(home)).is_err());

        let _ = fs::remove_dir_all(&root);
    }
}
