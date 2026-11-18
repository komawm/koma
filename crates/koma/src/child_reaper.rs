//! Reaps the detached children Koma spawns (`koma.act.spawn`, `ctx:spawn` macros,
//! `--winit/--tty --spawn`, and `notify-send` notices).
//!
//! `Command::spawn` returns a `Child`, but dropping that handle detaches the process instead of
//! waiting on it: nothing ever calls `waitpid`, so every child that exits stays `<defunct>` with
//! `PPID = koma`. A few minutes of ordinary use leaked hundreds of zombies (komawm/koma#38), and
//! because `pkill X` matches the zombie and exits 0, the documented Hyprland migration idiom
//! `pkill X || X` becomes a silent no-op after its first press — a migrated `Mod + F1` toggle
//! simply stops working.
//!
//! Spawn failures were swallowed too, so a missing binary looked exactly like a bind that never
//! fired. Both are fixed here: every detached child is registered on spawn and reaped with
//! `try_wait` by a background thread, and every `execve` error is logged once with the program
//! name and errno.
//!
//! # Reaping discipline
//!
//! The reaper only ever calls `Child::try_wait` on handles it owns, which is a `waitpid(pid,
//! WNOHANG)` on a specific child. A global `waitpid(-1)` is never used: `xwayland_satellite`
//! owns its own `Child` and waits on it, and stealing that exit status would break its
//! supervisor loop.
//!
//! # Registry size
//!
//! Entries are dropped as soon as the child exits, so the registry is bounded by the number of
//! *concurrently live* children. A terminal that lives for hours keeps exactly one entry for
//! hours, which is intentional — it is the only handle able to reap that process. A 500 ms poll
//! over a few hundred entries costs a few hundred `waitpid(WNOHANG)` syscalls per interval,
//! i.e. no meaningful CPU; there is deliberately no cap or eviction path, because dropping a live
//! entry would reintroduce the leak this module exists to prevent.

use std::{
    collections::VecDeque,
    io,
    process::{Child, Command, ExitStatus},
    sync::{Mutex, MutexGuard, OnceLock},
    thread,
    time::Duration,
};

use tracing::{debug, error, warn};

/// How often the reaper thread polls registered children.
const REAP_INTERVAL: Duration = Duration::from_millis(500);

/// How many reaped exit statuses are kept for supervisors that poll them after the fact.
const RECENT_EXITS_CAPACITY: usize = 32;

/// Children spawned by Koma that still need a `waitpid` call.
static CHILDREN: OnceLock<Mutex<Vec<TrackedChild>>> = OnceLock::new();

/// Exit statuses of recently reaped children, keyed by pid.
///
/// A supervisor (`koma-overview`, `xwayland-satellite`) needs to explain *why* the child it owns
/// disappeared; the reaper is the only code that performs the `waitpid`, so it publishes the status
/// here instead of dropping it into a `debug!` line. Bounded to the last
/// [`RECENT_EXITS_CAPACITY`] exits: a caller that never asks cannot grow the map.
static RECENT_EXITS: OnceLock<Mutex<VecDeque<(u32, ExitStatus)>>> = OnceLock::new();

/// One registered child, tagged with its call site so a leak can be traced back.
struct TrackedChild {
    child: Child,
    origin: &'static str,
}

fn registry() -> &'static Mutex<Vec<TrackedChild>> {
    CHILDREN.get_or_init(|| Mutex::new(Vec::new()))
}

/// Locks the registry, recovering from poisoning: a panicked reaper tick must not disable reaping.
fn registry_guard() -> MutexGuard<'static, Vec<TrackedChild>> {
    registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn recent_exits() -> &'static Mutex<VecDeque<(u32, ExitStatus)>> {
    RECENT_EXITS.get_or_init(|| Mutex::new(VecDeque::new()))
}

fn publish_exit(pid: u32, status: ExitStatus) {
    let mut exits = recent_exits()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if exits.len() == RECENT_EXITS_CAPACITY {
        exits.pop_front();
    }
    exits.push_back((pid, status));
}

/// Taken exit status of a child spawned through [`spawn_detached`], if it already exited.
///
/// The status is removed on the first call, so exactly one supervisor observes it; a child that is
/// still running (or was never spawned through this module) returns `None`.
pub(crate) fn take_exit_status(pid: u32) -> Option<ExitStatus> {
    let mut exits = recent_exits()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let index = exits.iter().position(|(exited, _)| *exited == pid)?;
    exits.remove(index).map(|(_, status)| status)
}

/// Whether `pid` is still registered as a running child of this compositor.
pub(crate) fn is_running(pid: u32) -> bool {
    registry_guard()
        .iter()
        .any(|tracked| tracked.child.id() == pid)
}

fn ensure_reaper_thread() {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        if let Err(err) = thread::Builder::new()
            .name("koma-child-reaper".into())
            .spawn(reaper_loop)
        {
            // Children still run without the thread, they are just never reaped; keep the
            // compositor alive and say so once (`reap_exited_children` may still be called).
            warn!(%err, "failed to start the child reaper thread");
        }
    });
}

fn reaper_loop() {
    loop {
        thread::sleep(REAP_INTERVAL);
        reap_exited_children();
    }
}

/// Drops every registered child that has already exited, reaping it on the way out.
///
/// `Child::try_wait` performs the `waitpid` that removes the `<defunct>` entry, so removing the
/// entry from the registry *is* the reap. A child that is still running stays registered.
pub(crate) fn reap_exited_children() {
    let mut children = registry_guard();
    children.retain_mut(|tracked| match tracked.child.try_wait() {
        Ok(Some(status)) => {
            publish_exit(tracked.child.id(), status);
            debug!(
                pid = tracked.child.id(),
                origin = tracked.origin,
                ?status,
                "reaped detached child"
            );
            false
        }
        Ok(None) => true,
        Err(err) => {
            // `ECHILD` means the exit status was already collected elsewhere (`Child::wait` on
            // a duplicate handle); either way the process is gone and must leave the registry.
            debug!(
                pid = tracked.child.id(),
                origin = tracked.origin,
                %err,
                "dropping untracked child"
            );
            false
        }
    });
}

/// Spawns `cmd` and registers the child so it is reaped when it exits.
///
/// Returns the child pid. Spawn failures are logged with the program name and errno before being
/// returned, so a bind whose binary is missing is diagnosable instead of indistinguishable from
/// a bind that never fired. Exactly one `error!` is emitted per failed spawn: failures only occur
/// on user action, so there is no flood risk, and silencing them is the bug being fixed.
pub(crate) fn spawn_detached(cmd: &mut Command, origin: &'static str) -> io::Result<u32> {
    match cmd.spawn() {
        Ok(child) => {
            let pid = child.id();
            ensure_reaper_thread();
            registry_guard().push(TrackedChild { child, origin });
            debug!(pid, origin, program = ?cmd.get_program(), "spawned detached child");
            Ok(pid)
        }
        Err(err) => {
            error!(
                origin,
                program = ?cmd.get_program(),
                %err,
                "failed to spawn child process"
            );
            Err(err)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// Pids currently held by the registry (other tests may run concurrently in this process, so
    /// assertions are made per-pid instead of on the global length).
    fn registered_pids() -> Vec<u32> {
        registry_guard()
            .iter()
            .map(|tracked| tracked.child.id())
            .collect()
    }

    /// Waits until none of `pids` is registered any more, driving the reaper by hand so the test
    /// does not depend on the background thread's 500 ms tick.
    fn wait_until_unregistered(pids: &[u32], timeout: Duration) -> Vec<u32> {
        let deadline = Instant::now() + timeout;
        loop {
            reap_exited_children();
            let registered = registered_pids();
            let leaked: Vec<u32> = pids
                .iter()
                .copied()
                .filter(|pid| registered.contains(pid))
                .collect();
            if leaked.is_empty() || Instant::now() >= deadline {
                return leaked;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// `/proc/<pid>/stat` process state (`R`, `S`, `Z`, ...), or `None` when the pid is gone.
    #[cfg(target_os = "linux")]
    fn proc_stat_state(pid: u32) -> Option<char> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // `1234 (comm with spaces) S 1 ...`: skip past the parenthesised comm.
        let after_comm = stat.rfind(')')?;
        stat[after_comm..].split_whitespace().nth(1)?.chars().next()
    }

    #[cfg(target_os = "linux")]
    fn proc_entry_exists(pid: u32) -> bool {
        std::path::Path::new(&format!("/proc/{pid}")).exists()
    }

    fn spawn_exiting_child() -> u32 {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "exit 0"]);
        spawn_detached(&mut cmd, "test.exiting").expect("spawn `sh -c 'exit 0'`")
    }

    /// The bug's premise: a spawned child with no waiter stays `<defunct>` until *somebody*
    /// calls `waitpid`. Proves the `/proc` probe below actually detects zombies.
    #[cfg(target_os = "linux")]
    #[test]
    fn unwatched_child_is_observable_as_a_zombie() {
        let mut child = Command::new("sh")
            .args(["-c", "exit 0"])
            .spawn()
            .expect("spawn `sh -c 'exit 0'`");
        let pid = child.id();

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut state = proc_stat_state(pid);
        while state != Some('Z') && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
            state = proc_stat_state(pid);
        }
        assert_eq!(
            state,
            Some('Z'),
            "an unwaited child must be observable as <defunct>"
        );

        // Clean up under our own handle; this is the exact `waitpid` the reaper performs.
        child.wait().expect("reap the probe child");
    }

    /// Acceptance criterion of komawm/koma#38: a spawn loop leaves zero zombies behind.
    #[test]
    fn spawn_loop_leaves_zero_zombies() {
        const ROUNDS: usize = 20;
        let pids: Vec<u32> = (0..ROUNDS).map(|_| spawn_exiting_child()).collect();

        let leaked = wait_until_unregistered(&pids, Duration::from_secs(10));
        assert!(
            leaked.is_empty(),
            "detached children were never reaped: {leaked:?}"
        );

        #[cfg(target_os = "linux")]
        {
            let zombies: Vec<u32> = pids
                .iter()
                .copied()
                .filter(|pid| proc_stat_state(*pid) == Some('Z'))
                .collect();
            assert!(
                zombies.is_empty(),
                "children left as <defunct>: {zombies:?}"
            );
            let still_present: Vec<u32> = pids
                .iter()
                .copied()
                .filter(|pid| proc_entry_exists(*pid))
                .collect();
            assert!(
                still_present.is_empty(),
                "reaped children must not keep a /proc entry: {still_present:?}"
            );
        }
    }

    /// Long-lived children (a terminal lives for hours) must stay registered until they exit
    /// instead of being dropped by the first sweep.
    #[test]
    fn running_child_stays_registered_until_it_exits() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 0.5"]);
        let pid = spawn_detached(&mut cmd, "test.long_lived").expect("spawn `sleep`");

        reap_exited_children();
        assert!(
            registered_pids().contains(&pid),
            "a running child must not be dropped from the registry"
        );

        let leaked = wait_until_unregistered(&[pid], Duration::from_secs(10));
        assert!(leaked.is_empty(), "child {pid} was never reaped");
    }

    /// A supervisor must be able to explain a child's disappearance, not just notice it.
    #[test]
    fn reaped_exit_status_is_published_once_for_supervisors() {
        let pid = spawn_exiting_child();
        let leaked = wait_until_unregistered(&[pid], Duration::from_secs(10));
        assert!(leaked.is_empty(), "child {pid} was never reaped");

        let status = take_exit_status(pid).expect("the exit status must be published");
        assert!(
            status.success(),
            "`sh -c 'exit 0'` exits successfully: {status:?}"
        );
        assert!(
            take_exit_status(pid).is_none(),
            "the status is consumed by the supervisor that asks for it"
        );
        assert!(
            !is_running(pid),
            "a reaped pid must not be reported as running"
        );
    }

    #[test]
    fn running_child_is_reported_as_running() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 0.2"]);
        let pid = spawn_detached(&mut cmd, "test.running_probe").expect("spawn `sleep`");
        assert!(is_running(pid));
        assert!(take_exit_status(pid).is_none());
        let leaked = wait_until_unregistered(&[pid], Duration::from_secs(10));
        assert!(leaked.is_empty(), "child {pid} was never reaped");
    }

    #[test]
    fn missing_program_reports_an_error_instead_of_being_swallowed() {
        let mut cmd = Command::new("/nonexistent/koma-child-reaper-probe");
        let err = spawn_detached(&mut cmd, "test.missing").expect_err("missing binary must fail");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }
}
