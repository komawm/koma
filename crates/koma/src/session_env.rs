//! `koma init <shell>` environment rendering and live-instance discovery.
//!
//! A running Koma instance owns `$XDG_RUNTIME_DIR/koma/<pid>_<wayland-socket>/`
//! with `koma-admin.sock` and `koma-agent.sock` inside. `koma init` inspects
//! those directories read-only and probes `koma-admin.sock` liveness with a bare
//! connect (no RPC), then emits shell-native assignments so
//! `koma init fish | source` works from an external terminal. `--unset` emits
//! the inverse. Capability tokens are never read or printed.
//!
//! The X11 `DISPLAY` value is derived from `/tmp/.X<n>-lock`: Koma's XWayland
//! satellite writes its own PID into the lock file, so the compositor PID from
//! the runtime-directory name identifies the correct display without guessing.

use std::{
    env, fmt, fs, io,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
};

use koma_proto::ADMIN_SOCKET_NAME;

/// Environment variables written by `koma init` and removed by `koma init --unset`.
pub const SESSION_ENV_VARS: [&str; 6] = [
    "WAYLAND_DISPLAY",
    "KOMA_INSTANCE_SIGNATURE",
    "XDG_RUNTIME_DIR",
    "DISPLAY",
    "XDG_SESSION_TYPE",
    "XDG_CURRENT_DESKTOP",
];

/// Shells accepted by `koma init <shell>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
    Nushell,
}

impl Shell {
    /// Parses a shell name; returns `None` for unknown shells.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "bash" => Some(Self::Bash),
            "zsh" => Some(Self::Zsh),
            "fish" => Some(Self::Fish),
            "nushell" | "nu" => Some(Self::Nushell),
            _ => None,
        }
    }

    /// Canonical name used in help text and shell comments.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Bash => "bash",
            Self::Zsh => "zsh",
            Self::Fish => "fish",
            Self::Nushell => "nushell",
        }
    }

    /// How to load the rendered output for this shell.
    #[must_use]
    pub const fn load_hint(self) -> &'static str {
        match self {
            Self::Bash => "eval \"$(koma init bash)\"",
            Self::Zsh => "eval \"$(koma init zsh)\"",
            Self::Fish => "koma init fish | source",
            Self::Nushell => {
                "koma init nushell | save -f ~/.config/koma/env.nu; source ~/.config/koma/env.nu"
            }
        }
    }
}

impl fmt::Display for Shell {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Resolved environment of one live Koma instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveSessionEnv {
    /// `KOMA_INSTANCE_SIGNATURE` (`<pid>_<wayland-socket>`).
    pub signature: String,
    /// Wayland socket name advertised by the compositor (`wayland-1`).
    pub wayland_display: String,
    /// Base runtime directory that contains `koma/<signature>/`.
    pub runtime_dir: PathBuf,
    /// Primary XWayland display (`:0`), present only while the satellite is up.
    pub x11_display: Option<String>,
}

/// Discovery failure with actionable messages.
#[derive(Debug)]
pub enum SessionEnvError {
    /// `$XDG_RUNTIME_DIR` is unset or empty.
    RuntimeDirUnset,
    /// No `koma/<signature>/koma-admin.sock` exists below the runtime directory.
    NoInstances { koma_root: PathBuf },
    /// Several instances are live; the user must choose one.
    MultipleInstances {
        koma_root: PathBuf,
        signatures: Vec<String>,
    },
    /// `KOMA_INSTANCE_SIGNATURE` names an instance that is not live.
    UnknownSignature {
        signature: String,
        koma_root: PathBuf,
        signatures: Vec<String>,
    },
    /// A runtime directory name does not match `<pid>_<wayland-socket>`.
    InvalidSignature { signature: String },
    /// Runtime directory scan failed.
    Io { path: PathBuf, source: io::Error },
}

impl fmt::Display for SessionEnvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RuntimeDirUnset => write!(
                f,
                "XDG_RUNTIME_DIR is not set; run `koma init` from a login session or set it \
                 to the directory that contains `koma/`"
            ),
            Self::NoInstances { koma_root } => write!(
                f,
                "no live Koma instance found under `{}`; start Koma first, or point \
                 XDG_RUNTIME_DIR at the session runtime directory",
                koma_root.display()
            ),
            Self::MultipleInstances {
                koma_root,
                signatures,
            } => write!(
                f,
                "several live Koma instances found under `{}`: {}; set \
                 KOMA_INSTANCE_SIGNATURE to the one you want",
                koma_root.display(),
                signatures.join(", ")
            ),
            Self::UnknownSignature {
                signature,
                koma_root,
                signatures,
            } => {
                write!(
                    f,
                    "KOMA_INSTANCE_SIGNATURE=`{signature}` is not live under `{}`",
                    koma_root.display()
                )?;
                if signatures.is_empty() {
                    write!(f, " (no live instances found)")
                } else {
                    write!(f, " (live: {})", signatures.join(", "))
                }
            }
            Self::InvalidSignature { signature } => write!(
                f,
                "runtime directory `{signature}` does not follow the \
                 `<pid>_<wayland-socket>` naming scheme"
            ),
            Self::Io { path, source } => {
                write!(f, "failed reading `{}`: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for SessionEnvError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Returns `$XDG_RUNTIME_DIR` or a clear error.
pub fn runtime_base() -> Result<PathBuf, SessionEnvError> {
    env::var_os("XDG_RUNTIME_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or(SessionEnvError::RuntimeDirUnset)
}

/// Discovers the live instance below `runtime_base/koma/`.
///
/// `preferred` is `$KOMA_INSTANCE_SIGNATURE`; when set it must name a live
/// directory. `x11_lock_dir` is normally `/tmp` and is injectable for tests.
///
/// Time O(number of runtime entries + X11 lock entries), space O(entries).
pub fn discover(
    runtime_base: &Path,
    x11_lock_dir: &Path,
    preferred: Option<&str>,
) -> Result<LiveSessionEnv, SessionEnvError> {
    let koma_root = runtime_base.join("koma");
    let entries = match fs::read_dir(&koma_root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Err(SessionEnvError::NoInstances { koma_root });
        }
        Err(source) => {
            return Err(SessionEnvError::Io {
                path: koma_root,
                source,
            });
        }
    };

    let mut signatures: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|entry| {
            let admin_socket = entry.path().join(ADMIN_SOCKET_NAME);
            admin_socket.exists() && UnixStream::connect(&admin_socket).is_ok()
        })
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    signatures.sort();

    let signature = if let Some(preferred) = preferred {
        if !signatures.iter().any(|sig| sig == preferred) {
            return Err(SessionEnvError::UnknownSignature {
                signature: preferred.to_owned(),
                koma_root,
                signatures,
            });
        }
        preferred.to_owned()
    } else {
        match signatures.as_slice() {
            [] => return Err(SessionEnvError::NoInstances { koma_root }),
            [only] => only.clone(),
            _ => {
                return Err(SessionEnvError::MultipleInstances {
                    koma_root,
                    signatures,
                });
            }
        }
    };

    let (pid_raw, wayland_display) = signature
        .split_once('_')
        .filter(|(pid, display)| {
            !pid.is_empty() && pid.bytes().all(|byte| byte.is_ascii_digit()) && !display.is_empty()
        })
        .map(|(pid, display)| (pid.to_owned(), display.to_owned()))
        .ok_or_else(|| SessionEnvError::InvalidSignature {
            signature: signature.clone(),
        })?;
    let pid: u32 = pid_raw
        .parse()
        .map_err(|_| SessionEnvError::InvalidSignature {
            signature: signature.clone(),
        })?;

    Ok(LiveSessionEnv {
        signature,
        wayland_display,
        runtime_dir: runtime_base.to_path_buf(),
        x11_display: find_x11_display(x11_lock_dir, pid),
    })
}

/// Finds the `/tmp/.X<n>-lock` file owned by `pid` with a live pathname socket.
fn find_x11_display(lock_dir: &Path, pid: u32) -> Option<String> {
    let mut numbers: Vec<u32> = fs::read_dir(lock_dir)
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            let number = name.strip_prefix(".X")?.strip_suffix("-lock")?;
            if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            let owner = fs::read_to_string(entry.path()).ok()?;
            if owner.trim().parse::<u32>().ok()? != pid {
                return None;
            }
            let number = number.parse::<u32>().ok()?;
            lock_dir
                .join(".X11-unix")
                .join(format!("X{number}"))
                .exists()
                .then_some(number)
        })
        .collect();
    numbers.sort_unstable();
    numbers.first().map(|number| format!(":{number}"))
}

/// Renders shell-native assignments for `env`, or removals when `unset`.
///
/// Time O(number of variables), space O(output).
#[must_use]
pub fn render(shell: Shell, unset: bool, env: &LiveSessionEnv) -> String {
    let mut out = format!(
        "# Koma session environment (generated by `koma init {}`).\n",
        shell.name()
    );
    if unset {
        out.push_str(&format!(
            "# Unset mode: removes the Koma session variables (including a stale DISPLAY).\n# Load it with: {}\n",
            shell.load_hint()
        ));
    } else {
        out.push_str(&format!("# Load it with: {}\n", shell.load_hint()));
    }

    let xdg_runtime = env.runtime_dir.display().to_string();

    for name in SESSION_ENV_VARS {
        let value = match name {
            "WAYLAND_DISPLAY" => Some(env.wayland_display.as_str()),
            "KOMA_INSTANCE_SIGNATURE" => Some(env.signature.as_str()),
            "XDG_RUNTIME_DIR" => Some(xdg_runtime.as_str()),
            "DISPLAY" => env.x11_display.as_deref(),
            "XDG_SESSION_TYPE" => Some("wayland"),
            "XDG_CURRENT_DESKTOP" => Some("koma"),
            other => unreachable!("unhandled session environment variable `{other}`"),
        };
        if unset {
            out.push_str(&unset_line(shell, name));
        } else if let Some(value) = value {
            out.push_str(&set_line(shell, name, value));
        }
        out.push('\n');
    }
    out
}

fn set_line(shell: Shell, name: &str, value: &str) -> String {
    match shell {
        Shell::Bash | Shell::Zsh => format!("export {name}={}", posix_single_quote(value)),
        Shell::Fish => format!("set -gx {name} {}", fish_single_quote(value)),
        Shell::Nushell => format!("$env.{name} = {}", nushell_double_quote(value)),
    }
}

fn unset_line(shell: Shell, name: &str) -> String {
    match shell {
        Shell::Bash | Shell::Zsh => format!("unset {name}"),
        Shell::Fish => format!("set -eg {name}"),
        Shell::Nushell => format!("$env.{name} = null"),
    }
}

fn posix_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

fn fish_single_quote(value: &str) -> String {
    let mut quoted = String::from("'");
    for ch in value.chars() {
        match ch {
            '\\' => quoted.push_str("\\\\"),
            '\'' => quoted.push_str("\\'"),
            other => quoted.push(other),
        }
    }
    quoted.push('\'');
    quoted
}

fn nushell_double_quote(value: &str) -> String {
    let mut quoted = String::from("\"");
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

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        process::Command,
        sync::atomic::{AtomicU32, Ordering},
    };

    use super::*;

    static NEXT_ID: AtomicU32 = AtomicU32::new(0);

    fn temp_dir(label: &str) -> PathBuf {
        let base = Path::new("/tmp/koma");
        fs::create_dir_all(base).expect("create /tmp/koma");
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let dir = base.join(format!("env-{label}-{}-{id}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn fake_instance(
        runtime_base: &Path,
        signature: &str,
    ) -> (
        std::os::unix::net::UnixListener,
        std::os::unix::net::UnixListener,
    ) {
        let dir = runtime_base.join("koma").join(signature);
        fs::create_dir_all(&dir).expect("create instance dir");
        let admin = std::os::unix::net::UnixListener::bind(dir.join(ADMIN_SOCKET_NAME))
            .expect("bind admin listener");
        let agent = std::os::unix::net::UnixListener::bind(dir.join(koma_proto::AGENT_SOCKET_NAME))
            .expect("bind agent listener");
        (admin, agent)
    }

    fn sample_env(runtime_base: &Path) -> LiveSessionEnv {
        LiveSessionEnv {
            signature: "4242_wayland-7".to_owned(),
            wayland_display: "wayland-7".to_owned(),
            runtime_dir: runtime_base.to_path_buf(),
            x11_display: Some(":3".to_owned()),
        }
    }

    #[test]
    fn discovers_a_single_instance_and_matching_x11_display() {
        let root = temp_dir("single");
        let lock_dir = root.join("tmp");
        fs::create_dir_all(lock_dir.join(".X11-unix")).expect("create x11 dir");
        let _instance = fake_instance(&root, "4242_wayland-7");
        fs::write(lock_dir.join(".X3-lock"), "      4242\n").expect("lock pid");
        fs::write(lock_dir.join(".X9-lock"), "      9999\n").expect("foreign lock");
        fs::write(lock_dir.join(".X11-unix/X3"), b"").expect("x11 socket");
        fs::write(lock_dir.join(".X11-unix/X9"), b"").expect("foreign socket");

        let env = discover(&root, &lock_dir, None).expect("discover");
        assert_eq!(env.signature, "4242_wayland-7");
        assert_eq!(env.wayland_display, "wayland-7");
        assert_eq!(env.x11_display.as_deref(), Some(":3"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn omits_display_when_no_xwayland_lock_matches() {
        let root = temp_dir("no-x11");
        let _instance = fake_instance(&root, "4242_wayland-7");
        let env = discover(&root, &root, None).expect("discover");
        assert_eq!(env.x11_display, None);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn zero_instances_fail_with_a_clear_message() {
        let root = temp_dir("empty");
        let err = discover(&root, &root, None).expect_err("must fail");
        assert!(matches!(err, SessionEnvError::NoInstances { .. }));
        assert!(err.to_string().contains("no live Koma instance"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn several_instances_fail_and_list_the_signatures() {
        let root = temp_dir("many");
        let _instance_a = fake_instance(&root, "111_wayland-1");
        let _instance_b = fake_instance(&root, "222_wayland-2");
        let err = discover(&root, &root, None).expect_err("must fail");
        let message = err.to_string();
        assert!(matches!(err, SessionEnvError::MultipleInstances { .. }));
        assert!(message.contains("111_wayland-1"));
        assert!(message.contains("222_wayland-2"));
        assert!(message.contains("KOMA_INSTANCE_SIGNATURE"));

        let selected = discover(&root, &root, Some("222_wayland-2")).expect("choose instance");
        assert_eq!(selected.signature, "222_wayland-2");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn stale_socket_directories_are_not_counted_as_live_instances() {
        let root = temp_dir("stale");
        let stale = root.join("koma").join("999_wayland-9");
        fs::create_dir_all(&stale).expect("create stale dir");
        fs::write(stale.join(ADMIN_SOCKET_NAME), b"").expect("stale socket placeholder");

        let err = discover(&root, &root, None).expect_err("stale socket must not count");
        assert!(matches!(err, SessionEnvError::NoInstances { .. }));

        let _instance = fake_instance(&root, "4242_wayland-7");
        let env = discover(&root, &root, None).expect("only the live instance is selected");
        assert_eq!(env.signature, "4242_wayland-7");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn preferred_signature_must_be_live_and_well_formed() {
        let root = temp_dir("preferred");
        let _instance = fake_instance(&root, "111_wayland-1");
        let err = discover(&root, &root, Some("nope")).expect_err("must fail");
        assert!(matches!(err, SessionEnvError::UnknownSignature { .. }));

        let root2 = temp_dir("malformed");
        let _instance2 = fake_instance(&root2, "wayland-only");
        let err = discover(&root2, &root2, None).expect_err("must fail");
        assert!(matches!(err, SessionEnvError::InvalidSignature { .. }));
        assert!(err.to_string().contains("pid"));
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&root2);
    }

    #[test]
    fn bash_and_zsh_rendering_sets_and_unsets_every_variable() {
        let env = sample_env(Path::new("/run/user/1000"));
        for shell in [Shell::Bash, Shell::Zsh] {
            let set = render(shell, false, &env);
            for name in SESSION_ENV_VARS {
                assert!(set.contains(&format!("export {name}=")), "{shell}: {set}");
                assert!(render(shell, true, &env).contains(&format!("unset {name}")));
            }
            assert!(set.contains("export WAYLAND_DISPLAY='wayland-7'"));
            assert!(!set.contains("koma_cap_"));
        }
    }

    #[test]
    fn fish_and_nushell_use_native_syntax() {
        let env = sample_env(Path::new("/run/user/1000"));
        let fish = render(Shell::Fish, false, &env);
        assert!(fish.contains("set -gx WAYLAND_DISPLAY 'wayland-7'"));
        assert!(render(Shell::Fish, true, &env).contains("set -eg DISPLAY"));
        let nu = render(Shell::Nushell, false, &env);
        assert!(nu.contains("$env.WAYLAND_DISPLAY = \"wayland-7\""));
        assert!(render(Shell::Nushell, true, &env).contains("$env.DISPLAY = null"));
        assert!(!nu.contains("export "));
    }

    #[test]
    fn rendered_scripts_source_cleanly_in_available_shells() {
        let root = temp_dir("source");
        let env = sample_env(Path::new("/run/user/1000"));
        let expected = "wayland-7|4242_wayland-7|/run/user/1000|:3|wayland|koma";

        for (shell, program) in [
            (Shell::Bash, "bash"),
            (Shell::Fish, "fish"),
            (Shell::Nushell, "nu"),
        ] {
            if Command::new(program).arg("--version").output().is_err() {
                continue;
            }
            let script = root.join(format!("{}.{}", shell.name(), extension(shell)));
            fs::write(&script, render(shell, false, &env)).expect("write script");
            let output = Command::new(program)
                .args(source_args(shell, &script))
                .output()
                .expect("run shell");
            assert!(
                output.status.success(),
                "{shell} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                stdout.trim().ends_with(expected),
                "{shell} did not set every variable: {stdout}"
            );

            fs::write(&script, render(shell, true, &env)).expect("write unset script");
            let output = Command::new(program)
                .args(unset_args(shell, &script))
                .output()
                .expect("run shell");
            assert!(
                output.status.success(),
                "{shell} unset failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                stdout.contains("all-unset"),
                "{shell} did not unset every variable: {stdout}"
            );
        }
        let _ = fs::remove_dir_all(&root);
    }

    fn extension(shell: Shell) -> &'static str {
        match shell {
            Shell::Bash | Shell::Zsh => "sh",
            Shell::Fish => "fish",
            Shell::Nushell => "nu",
        }
    }

    fn source_args(shell: Shell, script: &Path) -> Vec<String> {
        let path = script.display();
        match shell {
            Shell::Bash | Shell::Zsh => vec![
                "-c".to_owned(),
                format!(
                    "source '{path}'; printf '%s|%s|%s|%s|%s|%s' \
                     \"$WAYLAND_DISPLAY\" \"$KOMA_INSTANCE_SIGNATURE\" \"$XDG_RUNTIME_DIR\" \
                     \"$DISPLAY\" \"$XDG_SESSION_TYPE\" \"$XDG_CURRENT_DESKTOP\""
                ),
            ],
            Shell::Fish => vec![
                "-c".to_owned(),
                format!(
                    "source '{path}'; printf '%s|%s|%s|%s|%s|%s' \
                     $WAYLAND_DISPLAY $KOMA_INSTANCE_SIGNATURE $XDG_RUNTIME_DIR \
                     $DISPLAY $XDG_SESSION_TYPE $XDG_CURRENT_DESKTOP"
                ),
            ],
            Shell::Nushell => vec![
                "-c".to_owned(),
                format!(
                    "source '{path}'; print $\"($env.WAYLAND_DISPLAY)|($env.KOMA_INSTANCE_SIGNATURE)|\
                     ($env.XDG_RUNTIME_DIR)|($env.DISPLAY)|($env.XDG_SESSION_TYPE)|\
                     ($env.XDG_CURRENT_DESKTOP)\""
                ),
            ],
        }
    }

    fn unset_args(shell: Shell, script: &Path) -> Vec<String> {
        let path = script.display();
        match shell {
            Shell::Bash | Shell::Zsh => vec![
                "-c".to_owned(),
                format!(
                    "export WAYLAND_DISPLAY=x KOMA_INSTANCE_SIGNATURE=x XDG_RUNTIME_DIR=x \
                     DISPLAY=x XDG_SESSION_TYPE=x XDG_CURRENT_DESKTOP=x; source '{path}'; \
                     if [ -z \"$WAYLAND_DISPLAY$KOMA_INSTANCE_SIGNATURE$XDG_RUNTIME_DIR$DISPLAY\
                     $XDG_SESSION_TYPE$XDG_CURRENT_DESKTOP\" ]; then echo all-unset; fi"
                ),
            ],
            Shell::Fish => vec![
                "-c".to_owned(),
                format!(
                    "set -gx WAYLAND_DISPLAY x; set -gx KOMA_INSTANCE_SIGNATURE x; \
                     set -gx XDG_RUNTIME_DIR x; set -gx DISPLAY x; set -gx XDG_SESSION_TYPE x; \
                     set -gx XDG_CURRENT_DESKTOP x; source '{path}'; \
                     if test -z \"$WAYLAND_DISPLAY$KOMA_INSTANCE_SIGNATURE$XDG_RUNTIME_DIR\
                     $DISPLAY$XDG_SESSION_TYPE$XDG_CURRENT_DESKTOP\"; echo all-unset; end"
                ),
            ],
            Shell::Nushell => vec![
                "-c".to_owned(),
                format!(
                    "$env.WAYLAND_DISPLAY = 'x'; $env.KOMA_INSTANCE_SIGNATURE = 'x'; \
                     $env.XDG_RUNTIME_DIR = 'x'; $env.DISPLAY = 'x'; \
                     $env.XDG_SESSION_TYPE = 'x'; $env.XDG_CURRENT_DESKTOP = 'x'; \
                     source '{path}'; if ($env.WAYLAND_DISPLAY == null) and \
                     ($env.KOMA_INSTANCE_SIGNATURE == null) and ($env.XDG_RUNTIME_DIR == null) and \
                     ($env.DISPLAY == null) and ($env.XDG_SESSION_TYPE == null) and \
                     ($env.XDG_CURRENT_DESKTOP == null) {{ print all-unset }}"
                ),
            ],
        }
    }
}
