//! Pure command-line parsing for the `koma` binary.
//!
//! Parsing never touches the environment or starts a backend, so `--help`, `--version` and
//! malformed invocations can be handled before any display, seat or DRM resource is opened.

use std::{fmt, path::PathBuf};

/// Backend selection requested on the command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchMode {
    /// Winit when `WAYLAND_DISPLAY`/`DISPLAY` is set, otherwise TTY.
    Auto,
    Winit,
    Tty,
    HeadlessCheck,
}

/// Options for starting the compositor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOptions {
    pub mode: LaunchMode,
    pub is_session: bool,
    pub spawn_cmd: Option<String>,
    pub config_override: Option<PathBuf>,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            mode: LaunchMode::Auto,
            is_session: false,
            spawn_cmd: None,
            config_override: None,
        }
    }
}

/// Action selected by the command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliAction {
    Run(RunOptions),
    Help,
    Version,
}

/// Invalid command line; the binary prints the message plus usage and exits with status 2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliError {
    UnknownArgument(String),
    MissingValue(&'static str),
    ConflictingModes(&'static str, &'static str),
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownArgument(arg) => write!(f, "unrecognized argument `{arg}`"),
            Self::MissingValue(flag) => write!(f, "option `{flag}` requires a value"),
            Self::ConflictingModes(a, b) => {
                write!(f, "options `{a}` and `{b}` select different backends")
            }
        }
    }
}

impl std::error::Error for CliError {}

/// Exit status for usage errors (matches common CLI conventions such as `clap`).
pub const USAGE_EXIT_CODE: i32 = 2;

/// Returns the `--help` text.
#[must_use]
pub fn usage() -> String {
    format!(
        "koma {version} - Wayland compositor

Usage: koma [OPTIONS]

Backends (default: --winit when WAYLAND_DISPLAY or DISPLAY is set, otherwise --tty):
      --tty               Run on a Linux VT through libseat (seatd or systemd-logind), udev and DRM/KMS
      --winit             Run nested in a window inside an existing Wayland/X11 session
      --headless-check    Print a version/state-machine check and exit without opening any device

Options:
      --session           Act as the login session (koma-session, greetd): export XDG_CURRENT_DESKTOP,
                          import the environment into systemd --user and D-Bus
      --spawn <COMMAND>   Spawn COMMAND once the compositor is ready
  -c, --config <PATH>     Use PATH instead of $XDG_CONFIG_HOME/koma/init.lua
  -h, --help              Print this help and exit
  -V, --version           Print the version and exit

Signals: SIGTERM, SIGINT and SIGHUP stop the compositor gracefully (exit status 0).
",
        version = env!("CARGO_PKG_VERSION")
    )
}

/// Returns the `--version` text.
#[must_use]
pub fn version() -> String {
    format!("koma {}", env!("CARGO_PKG_VERSION"))
}

fn mode_flag(mode: LaunchMode) -> &'static str {
    match mode {
        LaunchMode::Auto => "",
        LaunchMode::Winit => "--winit",
        LaunchMode::Tty => "--tty",
        LaunchMode::HeadlessCheck => "--headless-check",
    }
}

fn set_mode(opts: &mut RunOptions, mode: LaunchMode) -> Result<(), CliError> {
    if opts.mode != LaunchMode::Auto && opts.mode != mode {
        return Err(CliError::ConflictingModes(
            mode_flag(opts.mode),
            mode_flag(mode),
        ));
    }
    opts.mode = mode;
    Ok(())
}

/// Parses arguments (excluding `argv[0]`).
///
/// `--help` and `--version` win as soon as they are seen, matching GNU conventions, so
/// `koma --tty --help` prints help instead of starting a backend. Time O(n), space O(1)
/// beyond the parsed values.
pub fn parse_args<I, S>(args: I) -> Result<CliAction, CliError>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut opts = RunOptions::default();
    let mut args = args.into_iter().map(Into::into);

    while let Some(arg) = args.next() {
        // Support `--flag=value` for the value-taking long options.
        let (flag, inline_value) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f.to_owned(), Some(v.to_owned())),
            _ => (arg.clone(), None),
        };

        match flag.as_str() {
            "-h" | "--help" if inline_value.is_none() => return Ok(CliAction::Help),
            "-V" | "--version" if inline_value.is_none() => return Ok(CliAction::Version),
            "--session" if inline_value.is_none() => opts.is_session = true,
            "--tty" if inline_value.is_none() => set_mode(&mut opts, LaunchMode::Tty)?,
            "--winit" if inline_value.is_none() => set_mode(&mut opts, LaunchMode::Winit)?,
            "--headless-check" if inline_value.is_none() => {
                set_mode(&mut opts, LaunchMode::HeadlessCheck)?;
            }
            "--spawn" => {
                let value = inline_value
                    .or_else(|| args.next())
                    .filter(|v| !v.is_empty())
                    .ok_or(CliError::MissingValue("--spawn"))?;
                opts.spawn_cmd = Some(value);
            }
            "-c" | "--config" => {
                let value = inline_value
                    .or_else(|| args.next())
                    .filter(|v| !v.is_empty())
                    .ok_or(CliError::MissingValue("--config"))?;
                opts.config_override = Some(PathBuf::from(value));
            }
            _ => return Err(CliError::UnknownArgument(arg)),
        }
    }

    Ok(CliAction::Run(opts))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: &[&str]) -> RunOptions {
        match parse_args(args.iter().copied()) {
            Ok(CliAction::Run(opts)) => opts,
            other => panic!("expected Run for {args:?}, got {other:?}"),
        }
    }

    #[test]
    fn empty_args_select_auto_mode() {
        assert_eq!(run(&[]), RunOptions::default());
    }

    #[test]
    fn backend_flags_select_modes() {
        assert_eq!(run(&["--tty"]).mode, LaunchMode::Tty);
        assert_eq!(run(&["--winit"]).mode, LaunchMode::Winit);
        assert_eq!(run(&["--headless-check"]).mode, LaunchMode::HeadlessCheck);
        assert_eq!(run(&["--tty", "--tty"]).mode, LaunchMode::Tty);
    }

    #[test]
    fn help_and_version_short_circuit() {
        for args in [
            &["--help"][..],
            &["-h"],
            &["--tty", "--help"],
            &["--help", "--bogus"],
        ] {
            assert_eq!(parse_args(args.iter().copied()), Ok(CliAction::Help));
        }
        for args in [&["--version"][..], &["-V"], &["--winit", "-V"]] {
            assert_eq!(parse_args(args.iter().copied()), Ok(CliAction::Version));
        }
    }

    #[test]
    fn unknown_arguments_are_rejected() {
        assert_eq!(
            parse_args(["--bogus"]),
            Err(CliError::UnknownArgument("--bogus".to_owned()))
        );
        assert_eq!(
            parse_args(["--tty", "extra"]),
            Err(CliError::UnknownArgument("extra".to_owned()))
        );
        assert_eq!(
            parse_args(["--tty=1"]),
            Err(CliError::UnknownArgument("--tty=1".to_owned()))
        );
        assert_eq!(
            parse_args(["--help=x"]),
            Err(CliError::UnknownArgument("--help=x".to_owned()))
        );
    }

    #[test]
    fn value_options_parse_separate_and_inline_forms() {
        let opts = run(&["--session", "--spawn", "foot", "-c", "/tmp/init.lua"]);
        assert!(opts.is_session);
        assert_eq!(opts.spawn_cmd.as_deref(), Some("foot"));
        assert_eq!(opts.config_override, Some(PathBuf::from("/tmp/init.lua")));

        let opts = run(&["--spawn=ghostty", "--config=/etc/koma/init.lua"]);
        assert_eq!(opts.spawn_cmd.as_deref(), Some("ghostty"));
        assert_eq!(
            opts.config_override,
            Some(PathBuf::from("/etc/koma/init.lua"))
        );
    }

    #[test]
    fn value_options_require_values() {
        assert_eq!(
            parse_args(["--spawn"]),
            Err(CliError::MissingValue("--spawn"))
        );
        assert_eq!(parse_args(["-c"]), Err(CliError::MissingValue("--config")));
        assert_eq!(
            parse_args(["--config="]),
            Err(CliError::MissingValue("--config"))
        );
    }

    #[test]
    fn conflicting_backends_are_rejected() {
        assert_eq!(
            parse_args(["--tty", "--winit"]),
            Err(CliError::ConflictingModes("--tty", "--winit"))
        );
    }

    #[test]
    fn usage_mentions_every_flag() {
        let text = usage();
        for flag in [
            "--tty",
            "--winit",
            "--headless-check",
            "--session",
            "--spawn",
            "--config",
            "--help",
            "--version",
        ] {
            assert!(text.contains(flag), "usage is missing {flag}");
        }
        assert!(version().starts_with("koma "));
    }
}
