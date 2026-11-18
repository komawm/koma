//! `koma-overview` daemon runtime plumbing shared by the compositor, `koma-overview` and `komactl`.
//!
//! The overview daemon is a separate process that owns one `wlr-layer-shell` client connection and
//! one `UnixListener` control socket. Three actors need to agree on where those live and on how a
//! dead daemon may be replaced:
//!
//! * the compositor, which toggles it (`Mod + Tab`) and supervises the daemon it spawns,
//! * the `koma-overview toggle|hover|leave` CLI, which is also invoked directly by status bars,
//! * `komactl doctor`, which reports whether the socket answers.
//!
//! Everything here is dependency-free (`std` + `rustix`) so it can be unit tested and cannot leak
//! protocol concerns into the callers. Nothing in this module logs: callers own the wording of the
//! one `info!` recovery line, the single `warn!` for an unreachable socket and the single `error!`
//! emitted when the respawn budget is exhausted.
//!
//! # Socket ownership
//!
//! A daemon only binds `<instance>/koma-overview.ctl.sock` when nothing answers there yet, so a
//! duplicate spawn cannot unlink a healthy daemon's socket and leave it unreachable. A leftover
//! file from a crashed daemon is reclaimed, which is why the *file* existing says nothing about the
//! *listener* being alive — [`probe_overview_daemon`] distinguishes the two.

use std::{
    fs::{self, OpenOptions},
    io::{self, Write as _},
    os::unix::{fs::OpenOptionsExt as _, net::UnixStream},
    path::{Path, PathBuf},
    time::Duration,
};

use rustix::process::{Pid, test_kill_process};

/// Control socket the daemon listens on (`$XDG_RUNTIME_DIR/koma/<signature>/koma-overview.ctl.sock`).
pub const OVERVIEW_CONTROL_SOCK_NAME: &str = "koma-overview.ctl.sock";

/// Pid of the running daemon, written by the daemon after it binds its control socket.
pub const OVERVIEW_PID_FILE_NAME: &str = "koma-overview.pid";

/// Single-flight lock taken by whichever actor is currently starting a replacement daemon.
pub const OVERVIEW_SPAWN_LOCK_NAME: &str = "koma-overview.spawn.lock";

/// Persisted respawn budget (`failures window_start gave_up reported`), shared by all actors.
pub const OVERVIEW_SPAWN_STATE_NAME: &str = "koma-overview.spawn-state";

/// Replacements allowed inside [`RESPAWN_WINDOW`] before the supervisor gives up.
pub const MAX_RESPAWNS: u32 = 3;

/// Sliding window the respawn budget is measured over.
pub const RESPAWN_WINDOW: Duration = Duration::from_secs(300);

/// First backoff delay between a failed start and the next attempt.
pub const BACKOFF_BASE: Duration = Duration::from_millis(200);

/// Ceiling of the exponential backoff between start attempts.
pub const BACKOFF_MAX: Duration = Duration::from_secs(4);

/// Everything the daemon lifecycle needs, resolved inside one instance runtime directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverviewPaths {
    /// Control socket (`koma-overview.ctl.sock`).
    pub socket: PathBuf,
    /// Pid file (`koma-overview.pid`).
    pub pid_file: PathBuf,
    /// Single-flight spawn lock (`koma-overview.spawn.lock`).
    pub spawn_lock: PathBuf,
    /// Persisted respawn budget (`koma-overview.spawn-state`).
    pub spawn_state: PathBuf,
}

impl OverviewPaths {
    /// Resolves every lifecycle path inside `runtime_dir` (`.../koma/<instance_signature>`).
    #[must_use]
    pub fn in_runtime_dir(runtime_dir: &Path) -> Self {
        Self {
            socket: runtime_dir.join(OVERVIEW_CONTROL_SOCK_NAME),
            pid_file: runtime_dir.join(OVERVIEW_PID_FILE_NAME),
            spawn_lock: runtime_dir.join(OVERVIEW_SPAWN_LOCK_NAME),
            spawn_state: runtime_dir.join(OVERVIEW_SPAWN_STATE_NAME),
        }
    }

    /// Directory holding the socket, pid file and spawn bookkeeping.
    #[must_use]
    pub fn runtime_dir(&self) -> &Path {
        self.socket.parent().unwrap_or_else(|| Path::new("."))
    }
}

/// What [`probe_overview_daemon`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverviewProbe {
    /// A listener accepted the probe connection: the daemon is alive and usable.
    Live {
        /// Pid reported by the daemon's pid file, when present and parseable.
        pid: Option<u32>,
    },
    /// A control socket file exists but nothing accepts connections on it.
    Stale {
        /// Pid recorded by the daemon, if it ever wrote one.
        recorded_pid: Option<u32>,
        /// `recorded_pid` only when that process is still running (a stale listener it no longer
        /// serves); `None` when the recorded process is gone.
        live_pid: Option<u32>,
        /// Operating-system error from the failed connect, for diagnostics.
        detail: String,
    },
    /// Neither a control socket nor a pid file exists: no overview daemon was ever started here.
    Absent,
}

impl OverviewProbe {
    /// `true` when a connection to the daemon succeeded.
    #[must_use]
    pub fn is_live(&self) -> bool {
        matches!(self, Self::Live { .. })
    }

    /// Pid worth naming in a diagnostic: the recorded daemon pid, or a pid file that outlived it.
    #[must_use]
    pub fn named_pid(&self) -> Option<u32> {
        match self {
            Self::Live { pid } => *pid,
            Self::Stale {
                recorded_pid,
                live_pid,
                ..
            } => live_pid.or(*recorded_pid),
            Self::Absent => None,
        }
    }

    /// One-line diagnostic naming the socket state without claiming to fix it.
    #[must_use]
    pub fn describe(&self, socket: &Path) -> String {
        match self {
            Self::Live { pid } => match pid {
                Some(pid) => format!("{} answers (daemon pid {pid})", socket.display()),
                None => format!("{} answers", socket.display()),
            },
            Self::Stale {
                recorded_pid: Some(pid),
                live_pid: None,
                detail,
            } => format!(
                "{} exists but nothing is listening ({detail}); daemon pid {pid} is gone",
                socket.display()
            ),
            Self::Stale {
                recorded_pid: Some(pid),
                live_pid: Some(_),
                detail,
            } => format!(
                "{} exists but nothing is listening ({detail}); daemon pid {pid} is still running \
                 without serving its socket",
                socket.display()
            ),
            Self::Stale {
                recorded_pid: None,
                detail,
                ..
            } => format!(
                "{} exists but nothing is listening ({detail})",
                socket.display()
            ),
            Self::Absent => format!(
                "{} does not exist; no koma-overview daemon was started for this instance",
                socket.display()
            ),
        }
    }
}

/// True for the errors that mean "the socket has no listener", i.e. worth a respawn attempt.
#[must_use]
pub fn is_unreachable_daemon_error(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::ConnectionRefused
            | io::ErrorKind::NotFound
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
    )
}

/// Probes the control socket without sending a command.
///
/// A successful `connect` (immediately closed) is the only signal that distinguishes "the daemon
/// listens" from "a socket file survived a crash". The daemon's accept loop treats the closed
/// connection as an empty command line, so the probe has no side effect.
#[must_use]
pub fn probe_overview_daemon(paths: &OverviewPaths) -> OverviewProbe {
    let recorded_pid = read_pid_file(&paths.pid_file);
    match UnixStream::connect(&paths.socket) {
        Ok(_) => OverviewProbe::Live { pid: recorded_pid },
        Err(err) => {
            if !paths.socket.exists() && recorded_pid.is_none() {
                return OverviewProbe::Absent;
            }
            let live_pid = recorded_pid.filter(|pid| process_is_alive(*pid));
            OverviewProbe::Stale {
                recorded_pid,
                live_pid,
                detail: err.to_string(),
            }
        }
    }
}

/// Reads a pid from `path`, ignoring absent, malformed or zero values.
#[must_use]
pub fn read_pid_file(path: &Path) -> Option<u32> {
    let raw = fs::read_to_string(path).ok()?;
    raw.trim().parse::<u32>().ok().filter(|pid| *pid > 0)
}

/// Records the current process as `path`'s owner (`0600`, atomic content replacement).
pub fn write_pid_file(path: &Path) -> io::Result<()> {
    let pid = std::process::id();
    write_private_file(path, format!("{pid}\n").as_bytes())
}

/// Writes `contents` to `path` with `0600` permissions, replacing the file atomically.
///
/// The temporary file lives next to the target (same filesystem) and is renamed into place, so a
/// concurrent reader never observes a half-written budget or pid file.
pub fn write_private_file(path: &Path, contents: &[u8]) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let temporary = path.with_extension(format!("tmp{}", std::process::id()));
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(contents)?;
        file.sync_all()?;
    }
    fs::rename(&temporary, path)
}

/// Removes `path` only when it still names `pid`, so a replacement daemon keeps its own files.
pub fn remove_owned_file(path: &Path, pid: u32) {
    if read_pid_file(path) == Some(pid) {
        let _ = fs::remove_file(path);
    }
}

/// Whether `pid` is a live process (`kill(pid, 0)`), used to label stale pid files.
#[must_use]
pub fn process_is_alive(pid: u32) -> bool {
    let Ok(raw) = i32::try_from(pid) else {
        return false;
    };
    let Some(pid) = Pid::from_raw(raw) else {
        return false;
    };
    match test_kill_process(pid) {
        Ok(()) => true,
        // `EPERM` still proves the pid exists; only `ESRCH` means it is gone.
        Err(errno) => errno == rustix::io::Errno::PERM,
    }
}

/// Held by the single actor currently allowed to start a replacement daemon.
///
/// Acquisition is `O_CREAT | O_EXCL` on `<instance>/koma-overview.spawn.lock` with the holder's pid
/// inside. A lock left behind by a crashed process is reclaimed after one liveness check, so a
/// stale lock cannot permanently disable recovery. Dropping the guard removes the file, which
/// releases every peer waiting in [`Self::try_acquire`].
#[derive(Debug)]
pub struct SpawnGuard {
    path: PathBuf,
}

impl SpawnGuard {
    /// Becomes the single respawner, or reports that a live peer already is one.
    ///
    /// Returns `Ok(None)` when another running process holds the lock.
    pub fn try_acquire(paths: &OverviewPaths) -> io::Result<Option<Self>> {
        match create_lock(&paths.spawn_lock) {
            Ok(Some(guard)) => Ok(Some(guard)),
            Ok(None) => Ok(None),
            // `create_lock` reports `AlreadyExists` only after proving the holder is gone; reclaim
            // it and retry once so a crashed respawner cannot disable recovery forever.
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                let _ = fs::remove_file(&paths.spawn_lock);
                create_lock(&paths.spawn_lock)
            }
            Err(err) => Err(err),
        }
    }

    /// Lock file path, for diagnostics.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for SpawnGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Creates the lock file exclusively; `Ok(None)` means a live peer owns it.
fn create_lock(path: &Path) -> io::Result<Option<SpawnGuard>> {
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(mut file) => {
            writeln!(file, "{}", std::process::id())?;
            Ok(Some(SpawnGuard {
                path: path.to_path_buf(),
            }))
        }
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            let holder = read_pid_file(path);
            if holder.is_some_and(process_is_alive) {
                Ok(None)
            } else {
                Err(err)
            }
        }
        Err(err) => Err(err),
    }
}

/// Decision returned by [`RespawnBudget::next_attempt`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RespawnAttempt {
    /// Start one daemon after sleeping `delay`; `attempt` is 1-based inside the current window.
    Spawn {
        /// Which attempt inside the window this is (`1..=MAX_RESPAWNS`).
        attempt: u32,
        /// Backoff to observe before spawning.
        delay: Duration,
    },
    /// The budget just ran out: callers emit their single `error!` now.
    GaveUp {
        /// Replacements already attempted inside the window.
        attempts: u32,
        /// Length of the window the attempts were counted in.
        window: Duration,
    },
    /// The budget ran out earlier and the single `error!` was already emitted.
    Exhausted,
}

/// Persisted, cross-actor respawn budget.
///
/// Every actor (compositor supervisor, `koma-overview toggle`) books its attempts here, so the
/// `N` bound is global: a broken daemon cannot be fork-bombed by a key repeat, a status-bar script
/// and the CLI at the same time. The window slides: once [`RESPAWN_WINDOW`] elapses without a new
/// attempt, the budget is usable again (a later press after the underlying defect is fixed still
/// recovers the daemon).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RespawnBudget {
    /// Replacements attempted inside the current window.
    pub failures: u32,
    /// Start of the current window (seconds since the Unix epoch).
    pub window_started_unix: u64,
    /// `true` once the window's attempts are spent.
    pub gave_up: bool,
    /// `true` once the single give-up diagnostic was emitted for this window.
    pub reported: bool,
}

impl RespawnBudget {
    /// Decides the next step, refreshing or persisting the window as needed.
    ///
    /// `now_unix` is passed in so the sliding window is testable without wall-clock sleep.
    pub fn next_attempt(&mut self, path: &Path, now_unix: u64) -> RespawnAttempt {
        self.refresh_window(now_unix);
        if self.gave_up {
            return if self.reported {
                RespawnAttempt::Exhausted
            } else {
                self.reported = true;
                self.persist(path);
                RespawnAttempt::GaveUp {
                    attempts: self.failures,
                    window: RESPAWN_WINDOW,
                }
            };
        }
        if self.failures >= MAX_RESPAWNS {
            self.gave_up = true;
            self.reported = true;
            self.persist(path);
            return RespawnAttempt::GaveUp {
                attempts: self.failures,
                window: RESPAWN_WINDOW,
            };
        }
        self.failures += 1;
        self.persist(path);
        RespawnAttempt::Spawn {
            attempt: self.failures,
            delay: backoff_delay(self.failures),
        }
    }

    /// Books a failed replacement (the daemon was spawned but never became reachable).
    ///
    /// The attempt itself is already counted by [`Self::next_attempt`]; this only keeps the
    /// window fresh so an actor that re-reads the file observes the failure immediately.
    pub fn record_failure(&mut self, path: &Path) {
        self.persist(path);
    }

    /// Clears the budget after a daemon answered; called with the same `now_unix` discipline.
    pub fn record_success(&mut self, path: &Path) {
        *self = Self::default();
        let _ = fs::remove_file(path);
    }

    /// Reads the persisted budget, expiring a window that has already slid past.
    #[must_use]
    pub fn load(path: &Path, now_unix: u64) -> Self {
        let mut budget = fs::read_to_string(path)
            .ok()
            .and_then(|raw| Self::parse(&raw))
            .unwrap_or_default();
        budget.refresh_window(now_unix);
        budget
    }

    fn parse(raw: &str) -> Option<Self> {
        let mut fields = raw.split_whitespace();
        let failures = fields.next()?.parse().ok()?;
        let window_started_unix = fields.next()?.parse().ok()?;
        let gave_up = fields.next()? == "1";
        let reported = fields.next().map(|value| value == "1").unwrap_or(false);
        Some(Self {
            failures,
            window_started_unix,
            gave_up,
            reported,
        })
    }

    fn serialize(self) -> String {
        format!(
            "{} {} {} {}\n",
            self.failures,
            self.window_started_unix,
            u8::from(self.gave_up),
            u8::from(self.reported)
        )
    }

    fn persist(self, path: &Path) {
        let _ = write_private_file(path, self.serialize().as_bytes());
    }

    /// Starts a fresh window once `RESPAWN_WINDOW` elapsed since the first attempt.
    fn refresh_window(&mut self, now_unix: u64) {
        if self.failures == 0 && !self.gave_up {
            self.window_started_unix = now_unix;
            return;
        }
        if now_unix.saturating_sub(self.window_started_unix) >= RESPAWN_WINDOW.as_secs() {
            *self = Self {
                window_started_unix: now_unix,
                ..Self::default()
            };
        }
    }
}

/// Exponential backoff for the `attempt`-th replacement inside one window.
///
/// `1 -> 200 ms`, `2 -> 400 ms`, `3 -> 800 ms`, capped at [`BACKOFF_MAX`].
#[must_use]
pub fn backoff_delay(attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(16);
    let millis = BACKOFF_BASE
        .as_millis()
        .saturating_mul(1_u128 << shift)
        .min(BACKOFF_MAX.as_millis());
    Duration::from_millis(u64::try_from(millis).unwrap_or(u64::MAX))
}

/// Seconds since the Unix epoch, saturating to `0` before 1970.
#[must_use]
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        os::unix::{fs::PermissionsExt as _, net::UnixListener},
        sync::atomic::{AtomicU32, Ordering},
    };

    static NEXT_ID: AtomicU32 = AtomicU32::new(0);

    fn temp_dir(label: &str) -> PathBuf {
        let base = Path::new("/tmp/koma");
        fs::create_dir_all(base).expect("create /tmp/koma");
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let dir = base.join(format!(
            "overview-proto-{label}-{}-{id}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create fixture dir");
        dir
    }

    #[test]
    fn paths_are_resolved_inside_the_instance_runtime_dir() {
        let paths = OverviewPaths::in_runtime_dir(Path::new("/run/user/1000/koma/1_wayland-1"));
        assert_eq!(
            paths.socket,
            PathBuf::from("/run/user/1000/koma/1_wayland-1/koma-overview.ctl.sock")
        );
        assert_eq!(
            paths.pid_file,
            PathBuf::from("/run/user/1000/koma/1_wayland-1/koma-overview.pid")
        );
        assert_eq!(
            paths.runtime_dir(),
            Path::new("/run/user/1000/koma/1_wayland-1")
        );
    }

    #[test]
    fn live_listener_is_reported_as_live_with_its_recorded_pid() {
        let dir = temp_dir("live");
        let paths = OverviewPaths::in_runtime_dir(&dir);
        let _listener = UnixListener::bind(&paths.socket).expect("bind control socket");
        fs::write(&paths.pid_file, format!("{}\n", std::process::id())).expect("write pid file");

        let probe = probe_overview_daemon(&paths);
        assert!(
            probe.is_live(),
            "a bound listener must probe as live: {probe:?}"
        );
        assert_eq!(probe.named_pid(), Some(std::process::id()));
        assert!(probe.describe(&paths.socket).contains("answers"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn socket_file_without_listener_is_stale_and_names_the_dead_pid() {
        let dir = temp_dir("stale");
        let paths = OverviewPaths::in_runtime_dir(&dir);
        // A crashed daemon leaves the socket inode and its pid file behind; nothing listens.
        let listener = UnixListener::bind(&paths.socket).expect("bind control socket");
        drop(listener);
        // `4_000_000` is above the default `pid_max` used by the kernel, so it cannot be alive.
        fs::write(&paths.pid_file, "4000000\n").expect("write pid file");

        let probe = probe_overview_daemon(&paths);
        match &probe {
            OverviewProbe::Stale {
                recorded_pid,
                live_pid,
                ..
            } => {
                assert_eq!(*recorded_pid, Some(4_000_000));
                assert_eq!(*live_pid, None, "a dead pid must not be reported as live");
            }
            other => panic!("expected a stale socket, got {other:?}"),
        }
        let described = probe.describe(&paths.socket);
        assert!(described.contains("nothing is listening"), "{described}");
        assert!(described.contains("4000000"), "{described}");
        assert!(described.contains("is gone"), "{described}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn absent_socket_and_pid_file_probe_as_absent() {
        let dir = temp_dir("absent");
        let paths = OverviewPaths::in_runtime_dir(&dir);
        let probe = probe_overview_daemon(&paths);
        assert_eq!(probe, OverviewProbe::Absent);
        assert!(probe.describe(&paths.socket).contains("does not exist"));
        assert!(!is_unreachable_daemon_error(&io::Error::from(
            io::ErrorKind::PermissionDenied
        )));
        assert!(is_unreachable_daemon_error(&io::Error::from(
            io::ErrorKind::ConnectionRefused
        )));
        assert!(is_unreachable_daemon_error(&io::Error::from(
            io::ErrorKind::NotFound
        )));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn spawn_guard_is_single_flight_and_released_on_drop() {
        let dir = temp_dir("guard");
        let paths = OverviewPaths::in_runtime_dir(&dir);
        let guard = SpawnGuard::try_acquire(&paths)
            .expect("acquire spawn lock")
            .expect("the first acquire must win");
        assert!(paths.spawn_lock.exists());
        assert!(
            SpawnGuard::try_acquire(&paths)
                .expect("second acquire is not an error")
                .is_none(),
            "a live holder must keep the lock single-flight"
        );
        assert_eq!(read_pid_file(&paths.spawn_lock), Some(std::process::id()));

        let lock_path = guard.path().to_path_buf();
        drop(guard);
        assert!(!lock_path.exists(), "dropping the guard releases the lock");
        assert!(
            SpawnGuard::try_acquire(&paths)
                .expect("re-acquire")
                .is_some(),
            "a released lock must be acquirable again"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn spawn_guard_reclaims_a_lock_left_by_a_dead_process() {
        let dir = temp_dir("guard-stale");
        let paths = OverviewPaths::in_runtime_dir(&dir);
        fs::write(&paths.spawn_lock, "4000000\n").expect("write stale lock");

        let guard = SpawnGuard::try_acquire(&paths)
            .expect("acquire over a stale lock")
            .expect("a stale holder must not block recovery");
        assert_eq!(read_pid_file(&paths.spawn_lock), Some(std::process::id()));
        drop(guard);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn budget_allows_max_respawns_then_reports_give_up_exactly_once() {
        let dir = temp_dir("budget");
        let state = dir.join(OVERVIEW_SPAWN_STATE_NAME);
        let mut budget = RespawnBudget::load(&state, 1_000);

        for attempt in 1..=MAX_RESPAWNS {
            match budget.next_attempt(&state, 1_000) {
                RespawnAttempt::Spawn {
                    attempt: got,
                    delay,
                } => {
                    assert_eq!(got, attempt);
                    assert_eq!(delay, backoff_delay(attempt));
                }
                other => panic!("attempt {attempt} should spawn, got {other:?}"),
            }
        }
        assert_eq!(
            budget.next_attempt(&state, 1_000),
            RespawnAttempt::GaveUp {
                attempts: MAX_RESPAWNS,
                window: RESPAWN_WINDOW
            }
        );
        assert_eq!(
            budget.next_attempt(&state, 1_000),
            RespawnAttempt::Exhausted,
            "the give-up diagnostic must be emitted exactly once"
        );

        // The budget survives a process boundary: a second actor reads the exhausted state.
        let mut reloaded = RespawnBudget::load(&state, 1_000);
        assert_eq!(
            reloaded.next_attempt(&state, 1_000),
            RespawnAttempt::Exhausted
        );

        // A new window re-arms the budget once the underlying defect may be fixed.
        let later = 1_000 + RESPAWN_WINDOW.as_secs();
        assert!(matches!(
            reloaded.next_attempt(&state, later),
            RespawnAttempt::Spawn { attempt: 1, .. }
        ));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn budget_success_clears_the_persisted_state() {
        let dir = temp_dir("budget-success");
        let state = dir.join(OVERVIEW_SPAWN_STATE_NAME);
        let mut budget = RespawnBudget::load(&state, 500);
        assert!(matches!(
            budget.next_attempt(&state, 500),
            RespawnAttempt::Spawn { .. }
        ));
        assert!(state.exists());
        budget.record_success(&state);
        assert!(!state.exists(), "a recovered daemon must clear the budget");
        assert_eq!(budget.failures, 0);
    }

    #[test]
    fn backoff_is_exponential_and_capped() {
        assert_eq!(backoff_delay(1), Duration::from_millis(200));
        assert_eq!(backoff_delay(2), Duration::from_millis(400));
        assert_eq!(backoff_delay(3), Duration::from_millis(800));
        assert_eq!(backoff_delay(9), BACKOFF_MAX);
        assert_eq!(backoff_delay(u32::MAX), BACKOFF_MAX);
    }

    #[test]
    fn pid_file_writes_are_private_and_owned_removal_is_safe() {
        let dir = temp_dir("pid");
        let pid_file = dir.join(OVERVIEW_PID_FILE_NAME);
        write_pid_file(&pid_file).expect("write pid file");
        assert_eq!(read_pid_file(&pid_file), Some(std::process::id()));
        let mode = fs::metadata(&pid_file)
            .expect("stat pid file")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "the pid file must not be world readable");

        remove_owned_file(&pid_file, std::process::id() + 1);
        assert!(pid_file.exists(), "a foreign pid must not delete the file");
        remove_owned_file(&pid_file, std::process::id());
        assert!(!pid_file.exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
