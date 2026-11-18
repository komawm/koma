//! Supervision of the `koma-overview` daemon from the compositor.
//!
//! `Mod + Tab` depends on a separate process: `koma-overview --daemon` owns the layer surfaces and
//! the control socket. When that process died, every later press spawned `koma-overview toggle`,
//! which failed with `Connection refused (os error 111)` and logged nothing useful
//! (komawm/koma#75). The compositor now owns the lifecycle of the daemon it starts:
//!
//! * the toggle is sent over the control socket directly, so a healthy press spawns no process,
//! * an unreachable socket is reported **once** with the socket path and the recorded daemon pid,
//! * a replacement is started through [`crate::child_reaper::spawn_detached`], so the child is
//!   reaped and its exit status is visible ([`crate::child_reaper::take_exit_status`]),
//! * starts are single-flight ([`koma_proto::overview::SpawnGuard`]) and bounded by the shared
//!   [`koma_proto::overview::RespawnBudget`], with exponential backoff from
//!   [`koma_proto::overview::backoff_delay`]; giving up logs exactly one `error!`,
//! * once the compositor owns a daemon it probes it every [`HEALTH_INTERVAL`] and restarts it, so
//!   the overview heals without waiting for the next press.
//!
//! Nothing here blocks the compositor thread: waiting for a freshly spawned daemon to bind is done
//! with one-shot `calloop` timers, so a press never stalls input or rendering.

use std::{io, os::unix::net::UnixStream, process::Command, time::Duration};

use calloop::{RegistrationToken, timer::Timer};
use koma_proto::overview::{
    MAX_RESPAWNS, OverviewPaths, RespawnAttempt, RespawnBudget, SpawnGuard,
    is_unreachable_daemon_error, probe_overview_daemon, read_pid_file, unix_now,
};
use tracing::{debug, error, info, warn};

use crate::state::KomaState;

/// Interval between readiness probes while a spawned daemon binds its socket.
pub(crate) const READY_POLL: Duration = Duration::from_millis(100);

/// How many readiness probes one start attempt gets (`2 s` at [`READY_POLL`]).
pub(crate) const READY_POLLS: u32 = 20;

/// How often the daemon this compositor owns is re-probed once it answered.
pub(crate) const HEALTH_INTERVAL: Duration = Duration::from_secs(5);

/// Binary the supervisor starts; resolved through `PATH` like the old keybind spawn.
const OVERVIEW_PROGRAM: &str = "koma-overview";

/// What the one-shot supervisor timer should do when it fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimerKind {
    /// Compose and spawn the replacement daemon (after the backoff delay).
    Spawn { attempt: u32 },
    /// Probe a daemon that was just spawned (or a peer's daemon).
    Ready,
    /// Re-enter the start scheduler after a failed attempt or a health check.
    Retry,
    /// Periodic health check for the daemon this compositor owns.
    Health,
}

/// Lifecycle state of the overview daemon, owned by [`KomaState`].
#[derive(Debug, Default)]
pub(crate) struct OverviewSupervisor {
    /// Pid of the daemon this compositor started, when known.
    spawned_pid: Option<u32>,
    /// A `toggle` arrived while the daemon was down and is delivered once it answers.
    pending_toggle: bool,
    /// Readiness probes spent on the current start attempt.
    ready_polls: u32,
    /// A start attempt (backoff, spawn or readiness probes) is already in flight.
    starting: bool,
    /// Consecutive waits for a peer process that held the spawn lock.
    peer_waits: u32,
    /// Single-flight start lock, held from "we decided to spawn" until the daemon answers.
    spawn_guard: Option<SpawnGuard>,
    /// Pending one-shot `calloop` timer.
    timer: Option<RegistrationToken>,
    /// The single unreachable-target warning was already emitted for this outage.
    warned_unreachable: bool,
    /// The shared respawn budget is spent; cleared by the next successful delivery.
    gave_up: bool,
}

impl OverviewSupervisor {
    /// Clears the per-outage state after a command reached a live daemon.
    fn on_delivered(&mut self, paths: &OverviewPaths, answering_pid: Option<u32>) {
        if self.warned_unreachable {
            info!(
                socket = %paths.socket.display(),
                daemon_pid = answering_pid,
                "koma-overview daemon answers again"
            );
        }
        self.pending_toggle = false;
        self.ready_polls = 0;
        self.starting = false;
        self.peer_waits = 0;
        self.warned_unreachable = false;
        self.gave_up = false;
        self.spawn_guard = None;
        if answering_pid.is_some() {
            // Adopt whoever answers, so health checks follow the daemon that is actually serving.
            self.spawned_pid = answering_pid;
        }
        RespawnBudget::default().record_success(&paths.spawn_state);
    }

    /// Releases the single-flight start lock.
    fn release_guard(&mut self) {
        self.spawn_guard = None;
    }
}

/// Whether `polls` readiness probes already exhausted one start attempt.
#[must_use]
pub(crate) fn ready_polls_exhausted(polls: u32) -> bool {
    polls >= READY_POLLS
}

/// Handles `Mod + Tab` / `koma.act.overview.toggle()`.
///
/// A dead daemon is not an error the user has to see twice: the first press warns once and starts
/// the replacement, and the toggle is delivered as soon as the replacement binds its socket.
pub(crate) fn request_toggle(state: &mut KomaState) {
    let Some(paths) = overview_paths(state) else {
        // No instance runtime dir (unusual: only before IPC starts): keep the pre-#75 behaviour.
        debug!(
            "no Koma instance runtime directory; falling back to spawning `koma-overview toggle`"
        );
        spawn_toggle_cli(state);
        return;
    };

    let mut supervisor = std::mem::take(&mut state.overview_supervisor);
    deliver_toggle(state, &mut supervisor, &paths);
    state.overview_supervisor = supervisor;
}

/// Sends the toggle, or starts a daemon and remembers the toggle for later delivery.
fn deliver_toggle(
    state: &mut KomaState,
    supervisor: &mut OverviewSupervisor,
    paths: &OverviewPaths,
) {
    match send_control(&paths.socket, "toggle") {
        Ok(()) => {
            // Read the pid file instead of probing again: the daemon already answered.
            supervisor.on_delivered(paths, read_pid_file(&paths.pid_file));
            supervise_answering_daemon(state, supervisor, paths);
            return;
        }
        Err(err) if !is_unreachable_daemon_error(&err) => {
            warn!(
                socket = %paths.socket.display(),
                %err,
                "failed sending `toggle` to the koma-overview daemon"
            );
            return;
        }
        Err(err) => debug!(
            socket = %paths.socket.display(),
            %err,
            "koma-overview control socket is unreachable"
        ),
    }

    let probe = probe_overview_daemon(paths);
    if probe.is_live() {
        // A peer (or our own just-started daemon) bound the socket between the write and the probe.
        if let Err(err) = send_control(&paths.socket, "toggle") {
            warn!(socket = %paths.socket.display(), %err, "failed delivering `toggle` to the koma-overview daemon");
        }
        supervisor.on_delivered(paths, probe.named_pid());
        supervise_answering_daemon(state, supervisor, paths);
        return;
    }

    if !supervisor.warned_unreachable {
        supervisor.warned_unreachable = true;
        warn!(
            socket = %paths.socket.display(),
            daemon_pid = probe.named_pid(),
            state = %probe.describe(&paths.socket),
            "koma-overview toggle target is unreachable; starting a replacement daemon"
        );
    }

    supervisor.pending_toggle = true;
    schedule_start(state, supervisor, paths);
}

/// Keeps polling the daemon that answered, so a later death is noticed without waiting for a press.
fn supervise_answering_daemon(
    state: &mut KomaState,
    supervisor: &mut OverviewSupervisor,
    paths: &OverviewPaths,
) {
    if supervisor.spawned_pid.is_none() {
        debug!(
            socket = %paths.socket.display(),
            "overview daemon pid is unknown; supervision starts after the next restart"
        );
        return;
    }
    if supervisor.timer.is_some() {
        // A pending start/probe timer is already in flight; it arms the health tick on success.
        return;
    }
    arm_timer(state, supervisor, HEALTH_INTERVAL, TimerKind::Health);
}

/// Books one start attempt in the shared budget and arms the backoff timer.
fn schedule_start(
    state: &mut KomaState,
    supervisor: &mut OverviewSupervisor,
    paths: &OverviewPaths,
) {
    if supervisor.gave_up {
        debug!(
            socket = %paths.socket.display(),
            "koma-overview respawn budget already exhausted for this window; not retrying"
        );
        return;
    }
    if supervisor.starting {
        // A press during an in-flight start only queues the toggle; the running chain delivers it
        // and no second daemon is spawned (`Mod + Tab` must not fork-bomb on key repeat).
        debug!(
            socket = %paths.socket.display(),
            "a koma-overview start is already in flight; not starting another one"
        );
        return;
    }
    if supervisor.spawn_guard.is_none() {
        match SpawnGuard::try_acquire(paths) {
            Ok(Some(guard)) => supervisor.spawn_guard = Some(guard),
            Ok(None) => {
                // Another process (the CLI, or a bar script) is starting it; probe shortly.
                supervisor.peer_waits += 1;
                if supervisor.peer_waits > MAX_RESPAWNS {
                    supervisor.gave_up = true;
                    supervisor.starting = false;
                    error!(
                        socket = %paths.socket.display(),
                        peer_waits = supervisor.peer_waits,
                        "another process keeps holding the koma-overview spawn lock without                          starting a daemon; not retrying this window"
                    );
                    return;
                }
                debug!(
                    socket = %paths.socket.display(),
                    peer_wait = supervisor.peer_waits,
                    "another process is starting the koma-overview daemon"
                );
                supervisor.starting = true;
                arm_timer(state, supervisor, READY_POLL, TimerKind::Ready);
                return;
            }
            Err(err) => {
                warn!(
                    socket = %paths.socket.display(),
                    %err,
                    "failed taking the koma-overview spawn lock"
                );
                return;
            }
        }
    }

    let now = unix_now();
    let mut budget = RespawnBudget::load(&paths.spawn_state, now);
    match budget.next_attempt(&paths.spawn_state, now) {
        RespawnAttempt::Spawn { attempt, delay } => {
            debug!(
                socket = %paths.socket.display(),
                attempt,
                backoff_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
                "scheduling a koma-overview replacement"
            );
            supervisor.ready_polls = 0;
            supervisor.starting = true;
            arm_timer(state, supervisor, delay, TimerKind::Spawn { attempt });
        }
        RespawnAttempt::GaveUp { attempts, window } => {
            supervisor.gave_up = true;
            supervisor.starting = false;
            supervisor.release_guard();
            error!(
                socket = %paths.socket.display(),
                attempts,
                window_secs = window.as_secs(),
                "giving up restarting the koma-overview daemon; run `komactl doctor` and check its \
                 journal lines"
            );
        }
        RespawnAttempt::Exhausted => {
            supervisor.gave_up = true;
            supervisor.starting = false;
            supervisor.release_guard();
            debug!(
                socket = %paths.socket.display(),
                "koma-overview respawn budget is exhausted for this window"
            );
        }
    }
}

/// Spawns the replacement daemon through the reaping supervisor.
fn spawn_daemon(
    state: &mut KomaState,
    supervisor: &mut OverviewSupervisor,
    paths: &OverviewPaths,
    attempt: u32,
) {
    let mut cmd = Command::new(OVERVIEW_PROGRAM);
    cmd.arg("--daemon");
    state.configure_child_env(&mut cmd, None);
    match crate::child_reaper::spawn_detached(&mut cmd, "overview.supervisor") {
        Ok(pid) => {
            info!(
                socket = %paths.socket.display(),
                pid,
                attempt,
                "restarted dead koma-overview daemon"
            );
            supervisor.spawned_pid = Some(pid);
            supervisor.ready_polls = 0;
            arm_timer(state, supervisor, READY_POLL, TimerKind::Ready);
        }
        Err(_) => {
            // `spawn_detached` already reported the `execve` failure with the program name.
            supervisor.release_guard();
            supervisor.starting = false;
            arm_timer(state, supervisor, READY_POLL, TimerKind::Retry);
        }
    }
}

/// Readiness probe for a daemon that was started moments ago.
fn poll_ready(state: &mut KomaState, supervisor: &mut OverviewSupervisor, paths: &OverviewPaths) {
    let probe = probe_overview_daemon(paths);
    if probe.is_live() {
        let answering_pid = probe.named_pid();
        info!(
            socket = %paths.socket.display(),
            daemon_pid = answering_pid,
            "koma-overview daemon is up"
        );
        if supervisor.pending_toggle {
            match send_control(&paths.socket, "toggle") {
                Ok(()) => debug!(
                    socket = %paths.socket.display(),
                    "delivered the queued overview toggle"
                ),
                Err(err) => warn!(
                    socket = %paths.socket.display(),
                    %err,
                    "failed delivering the queued overview toggle"
                ),
            }
        }
        supervisor.on_delivered(paths, answering_pid);
        // Supervise only the daemon this compositor started; a foreign one is not ours to restart
        // on a timer (the next press still recovers it through the same path).
        if supervisor.spawned_pid.is_some() {
            arm_timer(state, supervisor, HEALTH_INTERVAL, TimerKind::Health);
        }
        return;
    }

    supervisor.ready_polls += 1;
    if !ready_polls_exhausted(supervisor.ready_polls) {
        arm_timer(state, supervisor, READY_POLL, TimerKind::Ready);
        return;
    }

    log_daemon_loss(supervisor, paths, &probe);
    supervisor.ready_polls = 0;
    supervisor.starting = false;
    supervisor.release_guard();
    // The attempt is already booked in the budget; the next one waits for its backoff.
    schedule_start(state, supervisor, paths);
}

/// Periodic health check for the daemon this compositor owns.
fn health_tick(state: &mut KomaState, supervisor: &mut OverviewSupervisor, paths: &OverviewPaths) {
    let probe = probe_overview_daemon(paths);
    if probe.is_live() {
        arm_timer(state, supervisor, HEALTH_INTERVAL, TimerKind::Health);
        return;
    }
    log_daemon_loss(supervisor, paths, &probe);
    supervisor.pending_toggle = false;
    supervisor.warned_unreachable = false;
    supervisor.ready_polls = 0;
    supervisor.starting = false;
    schedule_start(state, supervisor, paths);
}

/// Logs why the supervised daemon is not answering, including its exit status when it exited.
fn log_daemon_loss(
    supervisor: &mut OverviewSupervisor,
    paths: &OverviewPaths,
    probe: &koma_proto::overview::OverviewProbe,
) {
    let pid = supervisor.spawned_pid.take();
    let status = pid.and_then(crate::child_reaper::take_exit_status);
    let running = pid.is_some_and(crate::child_reaper::is_running);
    warn!(
        socket = %paths.socket.display(),
        daemon_pid = pid,
        reason = %daemon_loss_reason(pid, status, running, probe, paths),
        "supervised koma-overview daemon is not answering; restarting it with backoff"
    );
}

/// Renders the daemon loss for the journal: an exit status, a wedged process, or a vanished one.
fn daemon_loss_reason(
    pid: Option<u32>,
    status: Option<std::process::ExitStatus>,
    running: bool,
    probe: &koma_proto::overview::OverviewProbe,
    paths: &OverviewPaths,
) -> String {
    match (pid, status) {
        (Some(pid), Some(status)) => format!("pid {pid} exited ({status})"),
        (Some(pid), None) if running => {
            format!(
                "pid {pid} is still running but {}",
                probe.describe(&paths.socket)
            )
        }
        (Some(pid), None) => format!("pid {pid} is gone and {}", probe.describe(&paths.socket)),
        (None, _) => probe.describe(&paths.socket),
    }
}

/// Arms (and replaces) the single supervisor timer.
fn arm_timer(
    state: &mut KomaState,
    supervisor: &mut OverviewSupervisor,
    delay: Duration,
    kind: TimerKind,
) {
    let Some(handle) = state.loop_handle.clone() else {
        debug!("no calloop handle; koma-overview supervision timer not armed");
        return;
    };
    if let Some(token) = supervisor.timer.take() {
        handle.remove(token);
    }
    let timer = Timer::from_duration(delay);
    match handle.insert_source(timer, move |_, _, state| {
        let mut supervisor = std::mem::take(&mut state.overview_supervisor);
        supervisor.timer = None;
        let Some(paths) = overview_paths(state) else {
            state.overview_supervisor = supervisor;
            return calloop::timer::TimeoutAction::Drop;
        };
        match kind {
            TimerKind::Spawn { attempt } => spawn_daemon(state, &mut supervisor, &paths, attempt),
            TimerKind::Ready => poll_ready(state, &mut supervisor, &paths),
            TimerKind::Retry => schedule_start(state, &mut supervisor, &paths),
            TimerKind::Health => health_tick(state, &mut supervisor, &paths),
        }
        state.overview_supervisor = supervisor;
        calloop::timer::TimeoutAction::Drop
    }) {
        Ok(token) => supervisor.timer = Some(token),
        Err(err) => debug!(%err, "failed arming the koma-overview supervision timer"),
    }
}

/// Resolves the daemon lifecycle paths for this session instance.
fn overview_paths(state: &KomaState) -> Option<OverviewPaths> {
    state
        .instance_runtime_dir
        .as_ref()
        .map(|dir| OverviewPaths::in_runtime_dir(dir))
}

/// Pre-#75 fallback: let the CLI process handle the toggle (and its own self-healing).
fn spawn_toggle_cli(state: &mut KomaState) {
    let mut cmd = Command::new(OVERVIEW_PROGRAM);
    cmd.arg("toggle");
    state.configure_child_env(&mut cmd, None);
    let _ = crate::child_reaper::spawn_detached(&mut cmd, "keybind.overview");
}

/// Writes one newline-terminated control line to the daemon socket.
fn send_control(socket: &std::path::Path, line: &str) -> io::Result<()> {
    use std::io::Write as _;

    let mut stream = UnixStream::connect(socket)?;
    stream.write_all(line.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::Read as _,
        os::unix::net::UnixListener,
        path::PathBuf,
        sync::atomic::{AtomicU32, Ordering},
    };

    use super::*;

    static NEXT_ID: AtomicU32 = AtomicU32::new(0);

    fn temp_dir(label: &str) -> PathBuf {
        let base = std::path::Path::new("/tmp/koma");
        fs::create_dir_all(base).expect("create /tmp/koma");
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let dir = base.join(format!(
            "koma-supervisor-{label}-{}-{id}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create fixture dir");
        dir
    }

    #[test]
    fn readiness_probes_are_bounded() {
        assert!(!ready_polls_exhausted(0));
        assert!(!ready_polls_exhausted(READY_POLLS - 1));
        assert!(ready_polls_exhausted(READY_POLLS));
        assert!(ready_polls_exhausted(READY_POLLS + 5));
        assert!(READY_POLL * READY_POLLS <= Duration::from_secs(2));
    }

    #[test]
    fn send_control_writes_a_single_newline_terminated_command() {
        let dir = temp_dir("send");
        let socket = dir.join("koma-overview.ctl.sock");
        let listener = UnixListener::bind(&socket).expect("bind control socket");
        send_control(&socket, "toggle").expect("write the control line");

        let (mut stream, _) = listener.accept().expect("accept the control client");
        let mut line = String::new();
        stream.read_to_string(&mut line).expect("read the command");
        assert_eq!(line, "toggle\n");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn send_control_reports_a_missing_socket() {
        let dir = temp_dir("missing");
        let socket = dir.join("koma-overview.ctl.sock");
        let err = send_control(&socket, "toggle").expect_err("no listener must fail");
        assert!(is_unreachable_daemon_error(&err), "{err}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn daemon_loss_reasons_distinguish_exit_wedge_and_vanish() {
        use koma_proto::overview::OverviewProbe;

        let dir = temp_dir("loss");
        let paths = OverviewPaths::in_runtime_dir(&dir);
        let probe = OverviewProbe::Stale {
            recorded_pid: Some(77),
            live_pid: None,
            detail: "Connection refused (os error 111)".to_owned(),
        };
        let exited = daemon_loss_reason(
            Some(77),
            Some(std::process::ExitStatus::default()),
            false,
            &probe,
            &paths,
        );
        assert!(exited.contains("pid 77 exited"), "{exited}");
        let wedged = daemon_loss_reason(Some(77), None, true, &probe, &paths);
        assert!(wedged.contains("still running"), "{wedged}");
        assert!(wedged.contains("nothing is listening"), "{wedged}");
        let vanished = daemon_loss_reason(Some(77), None, false, &probe, &paths);
        assert!(vanished.contains("is gone"), "{vanished}");
        let adopted = daemon_loss_reason(None, None, false, &probe, &paths);
        assert!(adopted.contains("koma-overview.ctl.sock"), "{adopted}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn paths_come_from_the_instance_runtime_dir() {
        let dir = temp_dir("paths");
        let paths = OverviewPaths::in_runtime_dir(&dir);
        assert_eq!(paths.socket.parent(), Some(dir.as_path()));
        assert_eq!(
            paths.socket.file_name().and_then(|name| name.to_str()),
            Some(koma_proto::overview::OVERVIEW_CONTROL_SOCK_NAME)
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
