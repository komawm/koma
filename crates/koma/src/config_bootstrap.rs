//! First-run configuration scaffold for the compositor.
//!
//! When the user's `~/.config/koma/init.lua` does not exist yet, Koma can create
//! the commented template once if `komactl init` spawned it manually. Only the
//! regular XDG user path is scaffolded: an explicit `--config`/`KOMA_CONFIG`
//! path is never created behind the user's back.
//!
//! The auto-scaffold is disabled by `KOMA_NO_SCAFFOLD=1`, never runs for
//! `--headless-check`, and is compiled out of test builds so the test suite can
//! never write to `~/.config/koma`.

use std::path::PathBuf;

use koma_lua::{ScaffoldError, scaffold_config_tree};

use crate::{cli::LaunchMode, config_watcher::KomaConfigPath};

/// Environment variable that disables the first-run scaffold.
pub const NO_SCAFFOLD_ENV: &str = "KOMA_NO_SCAFFOLD";

/// Whether a launch mode may create a missing user config.
///
/// `--headless-check` must stay a pure version/state-machine check: it never
/// opens a device and never writes configuration.
#[must_use]
pub fn should_auto_scaffold(mode: LaunchMode, disabled: bool) -> bool {
    !disabled && !matches!(mode, LaunchMode::HeadlessCheck)
}

/// Returns the `init.lua` path to scaffold, if it is safe to do so.
///
/// Only `KomaConfigPath::Regular` with a missing user config and no active
/// system config qualifies; the system-wide `/etc/xdg/koma/init.lua` is never
/// created and is never shadowed by a generated user file.
#[must_use]
pub fn scaffold_target(config: &KomaConfigPath) -> Option<PathBuf> {
    match config {
        KomaConfigPath::Explicit(_) => None,
        KomaConfigPath::Regular {
            user_init,
            system_init,
        } if !user_init.exists() && !system_init.exists() => Some(user_init.clone()),
        KomaConfigPath::Regular { .. } => None,
    }
}

/// Creates the template when `config` points at a missing user config.
///
/// Returns the created `init.lua` path on success and `None` when no scaffold
/// was attempted. `disabled` is normally `KOMA_NO_SCAFFOLD=1` from the
/// environment.
pub fn maybe_auto_scaffold(
    config: &KomaConfigPath,
    disabled: bool,
) -> Option<Result<PathBuf, ScaffoldError>> {
    if disabled || cfg!(test) {
        return None;
    }
    let target = scaffold_target(config)?;
    let root = target.parent()?.to_path_buf();
    Some(scaffold_config_tree(&root, false).map(|_| target))
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use super::*;

    #[test]
    fn only_a_missing_user_config_is_scaffolded() {
        let regular = KomaConfigPath::Regular {
            user_init: PathBuf::from("/tmp/koma/does-not-exist/init.lua"),
            system_init: PathBuf::from("/tmp/koma/no-system-init.lua"),
        };
        assert_eq!(
            scaffold_target(&regular),
            Some(PathBuf::from("/tmp/koma/does-not-exist/init.lua"))
        );

        let explicit = KomaConfigPath::Explicit(PathBuf::from("/tmp/koma/custom.lua"));
        assert_eq!(scaffold_target(&explicit), None);
    }

    #[test]
    fn an_existing_user_config_is_left_untouched() {
        let dir = Path::new("/tmp/koma");
        fs::create_dir_all(dir).expect("create /tmp/koma");
        let user_init = dir.join(format!("bootstrap-existing-{}.lua", std::process::id()));
        fs::write(&user_init, "-- user config\n").expect("seed config");
        let regular = KomaConfigPath::Regular {
            user_init: user_init.clone(),
            system_init: PathBuf::from("/tmp/koma/no-system-init.lua"),
        };
        assert_eq!(scaffold_target(&regular), None);
        let _ = fs::remove_file(user_init);
    }

    #[test]
    fn a_system_config_is_never_shadowed_by_scaffolding() {
        let dir = Path::new("/tmp/koma");
        fs::create_dir_all(dir).expect("create /tmp/koma");
        let system_init = dir.join(format!("bootstrap-system-{}.lua", std::process::id()));
        fs::write(&system_init, "-- system config\n").expect("seed system config");
        let regular = KomaConfigPath::Regular {
            user_init: PathBuf::from("/tmp/koma/missing-user-init.lua"),
            system_init: system_init.clone(),
        };
        assert_eq!(scaffold_target(&regular), None);
        let _ = fs::remove_file(system_init);
    }

    #[test]
    fn auto_scaffold_never_runs_in_test_builds_or_when_disabled() {
        let cert = KomaConfigPath::Regular {
            user_init: PathBuf::from("/tmp/koma/auto-scaffold-never/init.lua"),
            system_init: PathBuf::from("/tmp/koma/no-system-init.lua"),
        };
        assert!(maybe_auto_scaffold(&cert, false).is_none());
        assert!(maybe_auto_scaffold(&cert, true).is_none());
        assert!(!Path::new("/tmp/koma/auto-scaffold-never/init.lua").exists());
    }

    #[test]
    fn headless_check_and_the_disable_switch_block_scaffolding() {
        assert!(!should_auto_scaffold(LaunchMode::HeadlessCheck, false));
        assert!(!should_auto_scaffold(LaunchMode::Tty, true));
        assert!(!should_auto_scaffold(LaunchMode::Winit, true));
        assert!(should_auto_scaffold(LaunchMode::Tty, false));
        assert!(should_auto_scaffold(LaunchMode::Winit, false));
    }
}
