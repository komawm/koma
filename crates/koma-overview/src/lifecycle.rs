//! Self-healing `koma-overview toggle` for callers that are not the compositor.
//!
//! `Mod + Tab` asks the compositor for the toggle, but status bars (`Waybar` hover modules) and
//! `ctx:spawn({ "koma-overview", "toggle" })` gestures invoke this binary directly. Both paths must
//! recover from a daemon that died instead of reporting `Connection refused (os error 111)` forever
//! (komawm/koma#75).
//!
//! # Contract
//!
//! * A dead control socket triggers **one** replacement attempt per press, announced by a single
//!   `info!` naming the socket, the new pid and the attempt number.
//! * Attempts are shared through [`koma_proto::overview::RespawnBudget`], so this process, the
//!   compositor supervisor and a parallel bar script cannot fork-bomb a daemon that is broken at
//!   startup. After [`koma_proto::overview::MAX_RESPAWNS`] failures inside the window exactly one
//!   `error!` is emitted.
//! * Starts are serialised by [`koma_proto::overview::SpawnGuard`]: a second press that arrives
//!   while a peer is spawning waits for the peer's daemon instead of spawning another one.
//!
//! Spawning and sleeping are injected ([`DaemonSpawner`], [`Waiter`]) so unit tests never fork a
//! process or wait on a wall clock.

use std::{
    io,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

use koma_proto::overview::{
    OverviewPaths, RespawnAttempt, RespawnBudget, SpawnGuard, is_unreachable_daemon_error,
    probe_overview_daemon, unix_now,
};
use tracing::{debug, error, info, warn};

/// Delay between readiness probes while a replacement daemon binds its socket.
pub const READY_POLL: Duration = Duration::from_millis(25);

/// How long a freshly spawned daemon is given to accept the retried command.
pub const READY_TIMEOUT: Duration = Duration::from_millis(2_000);

/// How long a press waits for a *peer* process' replacement daemon before giving up.
pub const PEER_WAIT: Duration = Duration::from_millis(1_000);

/// Timing knobs of one recovery attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoveryPolicy {
    /// Total time a fresh daemon gets to answer the retried command.
    pub ready_timeout: Duration,
    /// Interval between readiness probes.
    pub ready_poll: Duration,
    /// Total time to wait for a peer's daemon when another process holds the spawn lock.
    pub peer_wait: Duration,
}

impl Default for RecoveryPolicy {
    fn default() -> Self {
        Self {
            ready_timeout: READY_TIMEOUT,
            ready_poll: READY_POLL,
            peer_wait: PEER_WAIT,
        }
    }
}

/// Spawns one `koma-overview --daemon`; returns its pid.
pub trait DaemonSpawner {
    /// Starts the daemon process. Errors are reported to the user, never swallowed.
    fn spawn_daemon(&mut self) -> io::Result<u32>;
}

/// Blocking sleep, injected so tests do not wait.
pub trait Waiter {
    /// Sleeps for `duration`.
    fn sleep(&mut self, duration: Duration);
}

/// Produces a real daemon process: the current executable, so the replacement is always the same
/// build the caller is running.
#[derive(Debug, Clone)]
pub struct CurrentExeSpawner {
    program: PathBuf,
}

impl CurrentExeSpawner {
    /// Resolves the program to re-exec, falling back to `koma-overview` on `PATH`.
    #[must_use]
    pub fn detect() -> Self {
        let program = std::env::current_exe()
            .ok()
            .filter(|path| path.is_file())
            .unwrap_or_else(|| PathBuf::from("koma-overview"));
        Self { program }
    }

    /// Program this spawner re-executes.
    #[must_use]
    pub fn program(&self) -> &Path {
        &self.program
    }
}

impl DaemonSpawner for CurrentExeSpawner {
    fn spawn_daemon(&mut self) -> io::Result<u32> {
        // stdin is detached; stderr stays inherited so the daemon's own exit reason reaches the
        // journal instead of disappearing with a pipe nobody reads.
        let child = Command::new(&self.program)
            .arg("--daemon")
            .stdin(Stdio::null())
            .spawn()?;
        Ok(child.id())
    }
}

/// Wall-clock sleeper.
#[derive(Debug, Clone, Copy, Default)]
pub struct ThreadWaiter;

impl Waiter for ThreadWaiter {
    fn sleep(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// Result of one self-healing command delivery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery {
    /// The daemon answered on the first attempt.
    Delivered,
    /// A replacement daemon became reachable and received the command.
    Recovered {
        /// Pid of the spawned daemon.
        pid: u32,
        /// Which attempt inside the respawn window this was (1-based).
        attempt: u32,
        /// Control socket that answered.
        socket: PathBuf,
    },
    /// A peer process started the daemon; the command reached it without us spawning anything.
    DeliveredByPeer {
        /// Control socket that answered.
        socket: PathBuf,
    },
    /// The respawn budget is exhausted: the daemon keeps failing to start.
    GaveUp {
        /// Replacements attempted inside the current window.
        attempts: u32,
    },
    /// Another process is spawning and its daemon did not answer in time.
    PeerRespawning {
        /// Control socket that is being replaced.
        socket: PathBuf,
    },
    /// The command could not be delivered and no replacement was started.
    Failed {
        /// Human-readable cause, printed by `main`.
        detail: String,
    },
}

impl Delivery {
    /// Whether the control command reached a live daemon.
    #[must_use]
    pub fn succeeded(&self) -> bool {
        matches!(
            self,
            Self::Delivered | Self::Recovered { .. } | Self::DeliveredByPeer { .. }
        )
    }

    /// Failure text for the CLI's stderr, or `None` on success.
    #[must_use]
    pub fn failure_message(&self) -> Option<String> {
        match self {
            Self::Delivered | Self::Recovered { .. } | Self::DeliveredByPeer { .. } => None,
            Self::GaveUp { attempts } => Some(format!(
                "koma-overview daemon is not running and {attempts} restart attempt(s) failed"
            )),
            Self::PeerRespawning { socket } => Some(format!(
                "another process is restarting the koma-overview daemon ({})",
                socket.display()
            )),
            Self::Failed { detail } => Some(detail.clone()),
        }
    }
}

/// Sends `command` to the daemon, restarting it once when the socket is dead.
///
/// `command` is the newline-terminated one-line control verb (`toggle`, `hover ...`, `leave`).
pub fn send_with_recovery(
    paths: &OverviewPaths,
    command: &str,
    spawner: &mut dyn DaemonSpawner,
    waiter: &mut dyn Waiter,
    policy: RecoveryPolicy,
) -> Delivery {
    match write_control(paths, command) {
        Ok(()) => {
            // A daemon answered: any earlier failure window is over.
            let mut budget = RespawnBudget::load(&paths.spawn_state, unix_now());
            if budget.failures > 0 || budget.gave_up {
                budget.record_success(&paths.spawn_state);
            }
            return Delivery::Delivered;
        }
        Err(err) if !is_unreachable_daemon_error(&err) => {
            return Delivery::Failed {
                detail: err.to_string(),
            };
        }
        Err(err) => debug!(
            socket = %paths.socket.display(),
            %err,
            "koma-overview control socket is unreachable; attempting recovery"
        ),
    }

    let probe = probe_overview_daemon(paths);
    if probe.is_live() {
        // The listener answered between the failed write and the probe (a peer just replaced it).
        return match write_control(paths, command) {
            Ok(()) => Delivery::Delivered,
            Err(err) => Delivery::Failed {
                detail: err.to_string(),
            },
        };
    }
    debug!(
        socket = %paths.socket.display(),
        state = %probe.describe(&paths.socket),
        "probed the koma-overview control socket"
    );

    let guard = match SpawnGuard::try_acquire(paths) {
        Ok(Some(guard)) => guard,
        Ok(None) => {
            debug!(
                socket = %paths.socket.display(),
                "another process is already restarting the koma-overview daemon"
            );
            return await_peer(paths, command, waiter, policy);
        }
        Err(err) => {
            return Delivery::Failed {
                detail: format!("failed taking the koma-overview spawn lock: {err}"),
            };
        }
    };

    let now = unix_now();
    let mut budget = RespawnBudget::load(&paths.spawn_state, now);
    match budget.next_attempt(&paths.spawn_state, now) {
        RespawnAttempt::Exhausted => {
            debug!(
                socket = %paths.socket.display(),
                attempts = budget.failures,
                "koma-overview respawn budget is exhausted for this window"
            );
            Delivery::GaveUp {
                attempts: budget.failures,
            }
        }
        RespawnAttempt::GaveUp { attempts, window } => {
            error!(
                socket = %paths.socket.display(),
                attempts,
                window_secs = window.as_secs(),
                "giving up restarting the koma-overview daemon; fix its startup error or restart \
                 the session"
            );
            Delivery::GaveUp { attempts }
        }
        RespawnAttempt::Spawn { attempt, delay } => {
            waiter.sleep(delay);
            match spawner.spawn_daemon() {
                Ok(pid) => {
                    info!(
                        socket = %paths.socket.display(),
                        pid,
                        attempt,
                        backoff_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
                        "restarting dead koma-overview daemon"
                    );
                    match await_ready(
                        paths,
                        command,
                        waiter,
                        policy.ready_timeout,
                        policy.ready_poll,
                    ) {
                        Ok(()) => {
                            budget.record_success(&paths.spawn_state);
                            drop(guard);
                            Delivery::Recovered {
                                pid,
                                attempt,
                                socket: paths.socket.clone(),
                            }
                        }
                        Err(err) => {
                            budget.record_failure(&paths.spawn_state);
                            warn!(
                                socket = %paths.socket.display(),
                                pid,
                                attempt,
                                %err,
                                "restarted koma-overview daemon never became reachable"
                            );
                            drop(guard);
                            Delivery::Failed {
                                detail: format!(
                                    "started koma-overview (pid {pid}) but {} never answered: {err}",
                                    paths.socket.display()
                                ),
                            }
                        }
                    }
                }
                Err(err) => {
                    budget.record_failure(&paths.spawn_state);
                    drop(guard);
                    Delivery::Failed {
                        detail: format!("failed starting koma-overview --daemon: {err}"),
                    }
                }
            }
        }
    }
}

/// Waits for a peer's replacement daemon and delivers the command to it.
fn await_peer(
    paths: &OverviewPaths,
    command: &str,
    waiter: &mut dyn Waiter,
    policy: RecoveryPolicy,
) -> Delivery {
    match await_ready(paths, command, waiter, policy.peer_wait, policy.ready_poll) {
        Ok(()) => Delivery::DeliveredByPeer {
            socket: paths.socket.clone(),
        },
        Err(_) => Delivery::PeerRespawning {
            socket: paths.socket.clone(),
        },
    }
}

/// Polls the control socket until `command` is accepted or `timeout` elapses.
///
/// The probe count is derived from `timeout / poll`, so the wait is deterministic and needs no
/// clock: `timeout == 0` performs exactly one delivery attempt.
fn await_ready(
    paths: &OverviewPaths,
    command: &str,
    waiter: &mut dyn Waiter,
    timeout: Duration,
    poll: Duration,
) -> io::Result<()> {
    let interval = poll.max(Duration::from_millis(1));
    let attempts = timeout.as_millis() / interval.as_millis();
    let mut last = io::Error::from(io::ErrorKind::ConnectionRefused);
    for _ in 0..=attempts {
        match write_control(paths, command) {
            Ok(()) => return Ok(()),
            Err(err) => last = err,
        }
        if attempts > 0 {
            waiter.sleep(interval);
        }
    }
    Err(last)
}

/// Writes one newline-terminated control line to the daemon socket.
fn write_control(paths: &OverviewPaths, command: &str) -> io::Result<()> {
    use std::io::Write as _;

    let mut stream = UnixStream::connect(&paths.socket)?;
    stream.write_all(command.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        os::unix::net::UnixListener,
        sync::{
            Arc, Barrier, Mutex,
            atomic::{AtomicU32, Ordering},
        },
    };

    use koma_proto::overview::{MAX_RESPAWNS, RESPAWN_WINDOW};

    static NEXT_ID: AtomicU32 = AtomicU32::new(0);

    fn temp_dir(label: &str) -> PathBuf {
        let base = Path::new("/tmp/koma");
        fs::create_dir_all(base).expect("create /tmp/koma");
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let dir = base.join(format!("overview-heal-{label}-{}-{id}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create fixture dir");
        dir
    }

    /// Instant policy: one delivery attempt, no backoff sleep.
    fn instant() -> RecoveryPolicy {
        RecoveryPolicy {
            ready_timeout: Duration::ZERO,
            ready_poll: Duration::ZERO,
            peer_wait: Duration::ZERO,
        }
    }

    #[derive(Default)]
    struct FakeSpawner {
        calls: Arc<AtomicU32>,
        barrier: Option<Arc<Barrier>>,
        fail: bool,
    }

    impl DaemonSpawner for FakeSpawner {
        fn spawn_daemon(&mut self) -> io::Result<u32> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(barrier) = &self.barrier {
                barrier.wait();
            }
            if self.fail {
                return Err(io::Error::other("execl failed"));
            }
            Ok(4242)
        }
    }

    struct NoWait;
    impl Waiter for NoWait {
        fn sleep(&mut self, _duration: Duration) {}
    }

    /// Captures every `tracing` line emitted on this thread, so log assertions do not depend on a
    /// global subscriber or on journald.
    fn capture_logs(body: impl FnOnce()) -> String {
        #[derive(Clone, Default)]
        struct Buffer(Arc<Mutex<Vec<u8>>>);
        impl io::Write for Buffer {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.0.lock().expect("log buffer").extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let buffer = Buffer::default();
        let writer = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, body);
        String::from_utf8(buffer.0.lock().expect("log buffer").clone()).expect("utf-8 logs")
    }

    /// A daemon double: binds `paths.socket` as soon as the spawner is called, like the real one.
    struct BindingSpawner {
        socket: PathBuf,
        calls: Arc<AtomicU32>,
        listener: Option<UnixListener>,
    }

    impl DaemonSpawner for BindingSpawner {
        fn spawn_daemon(&mut self) -> io::Result<u32> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.listener = Some(UnixListener::bind(&self.socket)?);
            Ok(9001)
        }
    }

    #[test]
    fn dead_socket_triggers_one_respawn_and_one_info_line() {
        let dir = temp_dir("respawn");
        let paths = OverviewPaths::in_runtime_dir(&dir);
        let calls = Arc::new(AtomicU32::new(0));
        let mut spawner = BindingSpawner {
            socket: paths.socket.clone(),
            calls: Arc::clone(&calls),
            listener: None,
        };
        let mut waiter = NoWait;

        let mut delivery = Delivery::Failed {
            detail: String::new(),
        };
        let logs = capture_logs(|| {
            delivery = send_with_recovery(&paths, "toggle", &mut spawner, &mut waiter, instant());
        });

        assert_eq!(calls.load(Ordering::SeqCst), 1, "exactly one spawn attempt");
        assert!(
            matches!(
                delivery,
                Delivery::Recovered {
                    pid: 9001,
                    attempt: 1,
                    ..
                }
            ),
            "a restarted daemon must deliver the command: {delivery:?}"
        );
        assert!(delivery.succeeded());
        assert_eq!(
            logs.matches("restarting dead koma-overview daemon").count(),
            1,
            "exactly one recovery line:\n{logs}"
        );
        assert!(
            !logs.contains("ERROR"),
            "a successful recovery must not log an error:\n{logs}"
        );
        assert!(
            !paths.spawn_state.exists(),
            "a healthy daemon clears the persisted budget"
        );

        // The command really reached the bound listener.
        spawner
            .listener
            .as_ref()
            .expect("listener")
            .set_nonblocking(true)
            .expect("non-blocking listener");
        let (mut stream, _) = spawner
            .listener
            .as_ref()
            .expect("listener")
            .accept()
            .expect("retried command was delivered");
        let mut line = String::new();
        std::io::Read::read_to_string(&mut stream, &mut line).expect("read command");
        assert_eq!(line, "toggle\n");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn live_daemon_is_used_without_spawning() {
        let dir = temp_dir("live");
        let paths = OverviewPaths::in_runtime_dir(&dir);
        let listener = UnixListener::bind(&paths.socket).expect("bind control socket");
        let calls = Arc::new(AtomicU32::new(0));
        let mut spawner = FakeSpawner {
            calls: Arc::clone(&calls),
            ..FakeSpawner::default()
        };
        let mut waiter = NoWait;

        let delivery = send_with_recovery(&paths, "toggle", &mut spawner, &mut waiter, instant());
        assert_eq!(delivery, Delivery::Delivered);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        drop(listener);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failing_daemon_is_restarted_at_most_max_respawns_times_then_one_error() {
        let dir = temp_dir("give-up");
        let paths = OverviewPaths::in_runtime_dir(&dir);
        let calls = Arc::new(AtomicU32::new(0));
        let mut waiter = NoWait;
        let mut logs = Vec::new();

        for press in 1..=MAX_RESPAWNS + 2 {
            let mut spawner = FakeSpawner {
                calls: Arc::clone(&calls),
                ..FakeSpawner::default()
            };
            let mut delivery = Delivery::Failed {
                detail: String::new(),
            };
            logs.push(capture_logs(|| {
                delivery =
                    send_with_recovery(&paths, "toggle", &mut spawner, &mut waiter, instant());
            }));
            if press <= MAX_RESPAWNS {
                assert!(
                    matches!(delivery, Delivery::Failed { .. }),
                    "press {press} must fail after the daemon never answers: {delivery:?}"
                );
            } else {
                assert_eq!(
                    delivery,
                    Delivery::GaveUp {
                        attempts: MAX_RESPAWNS
                    },
                    "press {press} must report the exhausted budget"
                );
            }
        }

        assert_eq!(
            calls.load(Ordering::SeqCst),
            MAX_RESPAWNS,
            "no spawn may happen after the budget is spent"
        );
        let errors: usize = logs
            .iter()
            .map(|log| {
                log.matches("giving up restarting the koma-overview daemon")
                    .count()
            })
            .sum();
        assert_eq!(errors, 1, "the give-up diagnostic is emitted exactly once");

        // A stale window re-arms the budget instead of disabling recovery for the session.
        let past = unix_now().saturating_sub(RESPAWN_WINDOW.as_secs());
        fs::write(&paths.spawn_state, format!("{MAX_RESPAWNS} {past} 1 1\n"))
            .expect("rewrite budget window");
        let mut spawner = FakeSpawner {
            calls: Arc::clone(&calls),
            ..FakeSpawner::default()
        };
        let delivery = send_with_recovery(&paths, "toggle", &mut spawner, &mut waiter, instant());
        assert!(matches!(delivery, Delivery::Failed { .. }));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            MAX_RESPAWNS + 1,
            "an expired window allows one more attempt"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn competing_press_does_not_spawn_a_second_daemon() {
        let dir = temp_dir("single-flight");
        let paths = OverviewPaths::in_runtime_dir(&dir);
        let calls = Arc::new(AtomicU32::new(0));
        let barrier = Arc::new(Barrier::new(2));

        // The first press wins the spawn lock and blocks inside `spawn_daemon`.
        let first_paths = paths.clone();
        let first_calls = Arc::clone(&calls);
        let first_barrier = Arc::clone(&barrier);
        let first = std::thread::spawn(move || {
            let mut spawner = FakeSpawner {
                calls: first_calls,
                barrier: Some(first_barrier),
                fail: false,
            };
            let mut waiter = NoWait;
            send_with_recovery(&first_paths, "toggle", &mut spawner, &mut waiter, instant())
        });

        // Wait until the first press is inside `spawn_daemon`, then press again.
        while calls.load(Ordering::SeqCst) == 0 {
            std::thread::sleep(Duration::from_millis(1));
        }
        let mut second_spawner = FakeSpawner {
            calls: Arc::clone(&calls),
            ..FakeSpawner::default()
        };
        let mut waiter = NoWait;
        let second = send_with_recovery(
            &paths,
            "toggle",
            &mut second_spawner,
            &mut waiter,
            instant(),
        );
        barrier.wait();
        let first_delivery = first.join().expect("first press finishes");

        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "a concurrent press must reuse the peer's start"
        );
        assert!(
            matches!(second, Delivery::PeerRespawning { .. }),
            "the second press waits instead of spawning: {second:?}"
        );
        assert!(
            matches!(first_delivery, Delivery::Failed { .. }),
            "the fake daemon never binds a socket, so the first press fails: {first_delivery:?}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn spawn_failure_is_reported_and_counted() {
        let dir = temp_dir("spawn-fail");
        let paths = OverviewPaths::in_runtime_dir(&dir);
        let calls = Arc::new(AtomicU32::new(0));
        let mut spawner = FakeSpawner {
            calls: Arc::clone(&calls),
            barrier: None,
            fail: true,
        };
        let mut waiter = NoWait;

        let delivery = send_with_recovery(&paths, "toggle", &mut spawner, &mut waiter, instant());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        match delivery {
            Delivery::Failed { detail } => assert!(detail.contains("failed starting"), "{detail}"),
            other => panic!("expected a spawn failure, got {other:?}"),
        }
        let budget = RespawnBudget::load(&paths.spawn_state, unix_now());
        assert_eq!(budget.failures, 1, "the failed attempt must be booked");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_peer_holding_the_spawn_lock_never_spawns_a_second_daemon() {
        let dir = temp_dir("peer");
        let paths = OverviewPaths::in_runtime_dir(&dir);
        // A live peer holds the spawn lock.
        let peer = SpawnGuard::try_acquire(&paths)
            .expect("acquire")
            .expect("peer wins the lock");
        // Bind the socket as the peer's daemon would.
        let listener = UnixListener::bind(&paths.socket).expect("bind control socket");

        let calls = Arc::new(AtomicU32::new(0));
        let mut spawner = FakeSpawner {
            calls: Arc::clone(&calls),
            ..FakeSpawner::default()
        };
        let mut waiter = NoWait;
        let delivery = send_with_recovery(&paths, "toggle", &mut spawner, &mut waiter, instant());
        assert!(
            delivery.succeeded(),
            "the peer's daemon must serve the command: {delivery:?}"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "a press must not spawn while a peer holds the spawn lock"
        );
        drop(listener);
        drop(peer);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn permission_errors_are_reported_without_respawning() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = temp_dir("hard-error");
        let paths = OverviewPaths::in_runtime_dir(&dir);
        // A listener this user cannot reach answers `EACCES`, which is not a dead daemon: spawning
        // a replacement would only replace one unreachable socket with another.
        let listener = UnixListener::bind(&paths.socket).expect("bind control socket");
        fs::set_permissions(&paths.socket, fs::Permissions::from_mode(0o000))
            .expect("make the socket unreachable");
        let calls = Arc::new(AtomicU32::new(0));
        let mut spawner = FakeSpawner {
            calls: Arc::clone(&calls),
            ..FakeSpawner::default()
        };
        let mut waiter = NoWait;

        let delivery = send_with_recovery(&paths, "toggle", &mut spawner, &mut waiter, instant());
        assert!(
            matches!(delivery, Delivery::Failed { .. }),
            "a permission error must be reported as a failure: {delivery:?}"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "a non-transport error must never spawn a daemon"
        );
        drop(listener);
        fs::set_permissions(&paths.socket, fs::Permissions::from_mode(0o600)).ok();
        let _ = fs::remove_dir_all(&dir);
    }
}
