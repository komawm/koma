//! In-compositor stall watchdog.
//!
//! The compositor event loop runs on a single thread. When it wedges on a non-reentrant mutex
//! the process looks *healthy* from the outside: load average is low, memory is free, the GPU is
//! idle, and no kernel oops is logged. The only external evidence is `wchan`, and the one datum
//! that identifies the offending mutex - the futex address passed to `futex(2)` - cannot be read
//! from outside because `kernel.yama.ptrace_scope=1` refuses `syscall` reads on another process.
//!
//! This module samples that evidence from inside the process:
//!
//! * [`StallWatchdog::start`] spawns one side thread before the event loop runs.
//! * [`StallWatchdog::tick`] is called once per event-loop iteration and stores a monotonic
//!   microsecond heartbeat into an atomic.
//! * When the heartbeat is older than the configured threshold the side thread writes one
//!   directory of evidence and logs a single classification line.
//!
//! Two properties are load-bearing:
//!
//! * **Nothing is written while healthy.** The thread only allocates, formats or touches the
//!   filesystem once a stall is detected.
//! * **The dump cannot block on the allocator.** The frozen event-loop thread may be the thread
//!   that holds the malloc arena lock, so the dump path uses pre-opened directory descriptors,
//!   stack buffers, and `rustix` syscalls only - no `Vec`, `String`, `format!`, `std::fs`, or
//!   `println!`. Every step checks a hard wall-clock budget and gives up rather than overrunning
//!   it.
//!
//! The classifier is a pure function over parsed [`ThreadSnapshot`] values so it can be unit
//! tested without reproducing a freeze.

use std::{
    fs::File,
    io,
    os::fd::{AsFd, OwnedFd},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use rustix::{
    fd::BorrowedFd,
    fs::{Mode, OFlags},
    io::Errno,
};
use tracing::{error, warn};

/// Environment variable holding the stall threshold in seconds (`0` disables the watchdog).
pub const STALL_SECS_ENV: &str = "KOMA_STALL_WATCHDOG_SECS";
/// Environment variable holding the dump wall-clock budget in milliseconds.
pub const STALL_BUDGET_ENV: &str = "KOMA_STALL_WATCHDOG_BUDGET_MS";

/// Default threshold: five seconds without an event-loop tick.
pub const DEFAULT_STALL_SECS: u64 = 5;
/// Default dump budget: 750 ms.
pub const DEFAULT_BUDGET_MS: u64 = 750;
/// How often the side thread re-reads the heartbeat.
const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Upper bound on captured threads.
const MAX_THREADS: usize = 256;
/// `comm` is at most 15 bytes plus NUL.
const COMM_CAP: usize = 16;
/// `wchan` symbol names are short.
const WCHAN_CAP: usize = 48;
/// `/proc/<pid>/syscall` is `nr` plus six arguments plus stack pointer and program counter.
const SYSCALL_CAP: usize = 192;

/// Futex syscall number for aarch64 and riscv64.
#[cfg(any(target_arch = "aarch64", target_arch = "riscv64"))]
const SYS_FUTEX_NR: i64 = 98;
/// Futex syscall number for x86_64.
#[cfg(target_arch = "x86_64")]
const SYS_FUTEX_NR: i64 = 202;
/// Futex syscall number for i686.
#[cfg(target_arch = "x86")]
const SYS_FUTEX_NR: i64 = 240;
/// Architectures without a known constant fall back to `wchan`-only detection.
#[cfg(not(any(
    target_arch = "aarch64",
    target_arch = "riscv64",
    target_arch = "x86_64",
    target_arch = "x86"
)))]
const SYS_FUTEX_NR: i64 = -1;

/// One thread's `/proc/self/task/<tid>/*` state, captured without heap allocation.
#[derive(Clone, Copy)]
pub struct ThreadSnapshot {
    pub tid: i32,
    comm: [u8; COMM_CAP],
    comm_len: usize,
    pub state: u8,
    wchan: [u8; WCHAN_CAP],
    wchan_len: usize,
    /// `(syscall number, first argument)` from `/proc/self/task/<tid>/syscall`.
    pub syscall: Option<(i64, u64)>,
    /// Verbatim `syscall` line, so the futex op/val/timeout arguments survive the capture.
    syscall_raw: [u8; SYSCALL_CAP],
    syscall_raw_len: usize,
    pub run_ns: u64,
    pub wait_ns: u64,
}

impl Default for ThreadSnapshot {
    fn default() -> Self {
        Self {
            tid: 0,
            comm: [0; COMM_CAP],
            comm_len: 0,
            state: b'?',
            wchan: [0; WCHAN_CAP],
            wchan_len: 0,
            syscall: None,
            syscall_raw: [0; SYSCALL_CAP],
            syscall_raw_len: 0,
            run_ns: 0,
            wait_ns: 0,
        }
    }
}

impl ThreadSnapshot {
    #[must_use]
    pub fn comm(&self) -> &str {
        std::str::from_utf8(&self.comm[..self.comm_len]).unwrap_or("?")
    }

    #[must_use]
    pub fn wchan(&self) -> &str {
        std::str::from_utf8(&self.wchan[..self.wchan_len]).unwrap_or("")
    }

    /// True when this thread is parked inside a futex wait.
    #[must_use]
    pub fn is_futex_wait(&self) -> bool {
        self.wchan().contains("futex")
    }

    /// Address the thread is waiting on, when it is a plain `futex(2)` wait.
    #[must_use]
    pub fn futex_address(&self) -> Option<u64> {
        match self.syscall {
            Some((nr, address)) if nr == SYS_FUTEX_NR => Some(address),
            _ => None,
        }
    }

    #[must_use]
    pub fn is_kernel_blocked(&self) -> bool {
        self.state == b'D'
    }
}

/// Kind of stall the evidence points at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StallKind {
    /// The event-loop thread alone waits on one futex: a self-reentrant mutex.
    SelfReentrant,
    /// Two or more threads wait on the same futex address: a cycle between threads.
    ThreadCycle,
    /// Exactly one futex waiter, but it is not the event-loop thread.
    FutexWaitOther,
    /// Several futex waiters on different addresses; no single cycle is proven.
    ScatteredFutex,
    /// No futex waiter, but at least one thread is in uninterruptible sleep.
    KernelBlocked,
    /// The evidence does not match a known pattern.
    Unexplained,
}

impl StallKind {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::SelfReentrant => "self-reentrant-mutex",
            Self::ThreadCycle => "cross-thread-futex-cycle",
            Self::FutexWaitOther => "futex-wait-non-loop-thread",
            Self::ScatteredFutex => "scattered-futex-waits",
            Self::KernelBlocked => "kernel-blocked",
            Self::Unexplained => "unexplained",
        }
    }
}

/// Classification result over a captured thread set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Verdict {
    pub kind: StallKind,
    pub address: Option<u64>,
    tids: [i32; MAX_THREADS],
    tid_count: usize,
    pub futex_waiters: usize,
    pub kernel_blocked: usize,
}

impl Verdict {
    /// Threads that participate in the classified wait (at most four are reported).
    #[must_use]
    pub fn tids(&self) -> &[i32] {
        &self.tids[..self.tid_count]
    }

    /// One-line human summary written next to the dump and into the log.
    #[must_use]
    pub fn one_line(&self) -> String {
        let threads = self
            .tids()
            .iter()
            .map(i32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        match self.kind {
            StallKind::SelfReentrant => format!(
                "{}: thread {} is the lone futex waiter on 0x{:x}",
                self.kind.label(),
                threads,
                self.address.unwrap_or(0)
            ),
            StallKind::ThreadCycle => format!(
                "{}: threads {} wait on the same futex 0x{:x}",
                self.kind.label(),
                threads,
                self.address.unwrap_or(0)
            ),
            StallKind::FutexWaitOther => format!(
                "{}: thread {} waits on futex 0x{:x} while the event-loop thread is elsewhere",
                self.kind.label(),
                threads,
                self.address.unwrap_or(0)
            ),
            StallKind::ScatteredFutex => format!(
                "{}: {} futex waiters on distinct addresses",
                self.kind.label(),
                self.futex_waiters
            ),
            StallKind::KernelBlocked => format!(
                "{}: {} thread(s) in uninterruptible sleep",
                self.kind.label(),
                self.kernel_blocked
            ),
            StallKind::Unexplained => format!(
                "{}: {} futex waiter(s), {} kernel-blocked thread(s)",
                self.kind.label(),
                self.futex_waiters,
                self.kernel_blocked
            ),
        }
    }
}

/// Classifies a captured thread set.
///
/// `event_loop_tid` is the thread id that produced the heartbeat. Keeping this function pure
/// (no `/proc`, no clock, no allocation) is what makes the classifier unit-testable.
#[must_use]
pub fn classify(threads: &[ThreadSnapshot], event_loop_tid: i32) -> Verdict {
    let mut verdict = Verdict {
        kind: StallKind::Unexplained,
        address: None,
        tids: [0; MAX_THREADS],
        tid_count: 0,
        futex_waiters: 0,
        kernel_blocked: 0,
    };

    let mut waiters: [(i32, u64); MAX_THREADS] = [(0, 0); MAX_THREADS];
    let mut waiter_count = 0_usize;
    let mut d_state: [i32; MAX_THREADS] = [0; MAX_THREADS];
    let mut d_count = 0_usize;

    for thread in threads {
        if thread.is_kernel_blocked() && d_count < MAX_THREADS {
            d_state[d_count] = thread.tid;
            d_count += 1;
        }
        if thread.is_futex_wait()
            && let Some(address) = thread.futex_address()
            && waiter_count < MAX_THREADS
        {
            waiters[waiter_count] = (thread.tid, address);
            waiter_count += 1;
        }
    }

    verdict.futex_waiters = waiter_count;
    verdict.kernel_blocked = d_count;

    if waiter_count == 0 {
        if d_count > 0 {
            verdict.kind = StallKind::KernelBlocked;
            for tid in &d_state[..d_count.min(4)] {
                verdict.tids[verdict.tid_count] = *tid;
                verdict.tid_count += 1;
            }
        }
        return verdict;
    }

    // Group waiters by futex address, preferring the group the event-loop thread belongs to.
    let mut best_address = waiters[0].1;
    let mut best_count = 0_usize;
    let mut event_loop_group = None;

    for candidate in 0..waiter_count {
        let address = waiters[candidate].1;
        let mut count = 0_usize;
        let mut contains_loop = false;
        for waiter in &waiters[..waiter_count] {
            if waiter.1 == address {
                count += 1;
                contains_loop |= waiter.0 == event_loop_tid;
            }
        }
        if contains_loop {
            event_loop_group = Some((address, count));
            break;
        }
        if count > best_count {
            best_address = address;
            best_count = count;
        }
    }

    let (address, group_count) = event_loop_group.unwrap_or((best_address, best_count.max(1)));
    verdict.address = Some(address);

    for waiter in &waiters[..waiter_count] {
        if waiter.1 == address && verdict.tid_count < MAX_THREADS {
            verdict.tids[verdict.tid_count] = waiter.0;
            verdict.tid_count += 1;
        }
    }

    verdict.kind = if group_count >= 2 {
        StallKind::ThreadCycle
    } else if verdict.tids[..verdict.tid_count].contains(&event_loop_tid) {
        StallKind::SelfReentrant
    } else if waiter_count == 1 {
        StallKind::FutexWaitOther
    } else {
        StallKind::ScatteredFutex
    };

    // Report at most four participants in the log line.
    verdict.tid_count = verdict.tid_count.min(4);
    verdict
}

/// Fixed-capacity, allocation-free text buffer used by the dump path.
struct Line {
    buf: [u8; 4096],
    len: usize,
}

impl Line {
    fn new() -> Self {
        Self {
            buf: [0; 4096],
            len: 0,
        }
    }

    fn clear(&mut self) {
        self.len = 0;
    }

    fn push_bytes(&mut self, bytes: &[u8]) {
        let room = self.buf.len() - self.len;
        let take = bytes.len().min(room);
        self.buf[self.len..self.len + take].copy_from_slice(&bytes[..take]);
        self.len += take;
    }

    fn push_str(&mut self, text: &str) {
        self.push_bytes(text.as_bytes());
    }

    fn push_u64(&mut self, value: u64) {
        let mut digits = [0_u8; 20];
        let mut index = digits.len();
        let mut rest = value;
        loop {
            index -= 1;
            digits[index] = b'0' + (rest % 10) as u8;
            rest /= 10;
            if rest == 0 {
                break;
            }
        }
        self.push_bytes(&digits[index..]);
    }

    fn push_i64(&mut self, value: i64) {
        if value < 0 {
            self.push_str("-");
            self.push_u64(value.unsigned_abs());
        } else {
            self.push_u64(value.unsigned_abs());
        }
    }

    fn push_hex(&mut self, value: u64) {
        let mut digits = [0_u8; 16];
        let mut index = digits.len();
        let mut rest = value;
        loop {
            index -= 1;
            let nibble = (rest & 0xf) as u8;
            digits[index] = if nibble < 10 {
                b'0' + nibble
            } else {
                b'a' + (nibble - 10)
            };
            rest >>= 4;
            if rest == 0 {
                break;
            }
        }
        self.push_str("0x");
        self.push_bytes(&digits[index..]);
    }

    fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

fn write_all(fd: BorrowedFd<'_>, mut bytes: &[u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        match rustix::io::write(fd, bytes) {
            Ok(0) => return Err(io::Error::from(Errno::IO)),
            Ok(written) => bytes = &bytes[written..],
            Err(Errno::INTR) => {}
            Err(err) => return Err(io::Error::from(err)),
        }
    }
    Ok(())
}

/// Reads a procfs file into `buf`, returning the trimmed slice length.
fn read_small(fd: BorrowedFd<'_>, buf: &mut [u8]) -> io::Result<usize> {
    let mut total = 0_usize;
    loop {
        if total == buf.len() {
            break;
        }
        match rustix::io::read(fd, &mut buf[total..]) {
            Ok(0) => break,
            Ok(read) => total += read,
            Err(Errno::INTR) => {}
            Err(err) => return Err(io::Error::from(err)),
        }
    }
    while total > 0 && matches!(buf[total - 1], b'\n' | b'\r' | b' ' | 0) {
        total -= 1;
    }
    Ok(total)
}

fn parse_u64_dec(token: &[u8]) -> Option<u64> {
    if token.is_empty() {
        return None;
    }
    let mut value = 0_u64;
    for byte in token {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add(u64::from(byte - b'0'))?;
    }
    Some(value)
}

fn parse_i64_dec(token: &[u8]) -> Option<i64> {
    match token.split_first() {
        Some((b'-', rest)) => parse_u64_dec(rest)
            .and_then(|value| i64::try_from(value).ok())
            .map(|value| -value),
        _ => i64::try_from(parse_u64_dec(token)?).ok(),
    }
}

fn parse_u64_hex(token: &[u8]) -> Option<u64> {
    let digits = token
        .strip_prefix(b"0x")
        .or_else(|| token.strip_prefix(b"0X"))
        .unwrap_or(token);
    if digits.is_empty() {
        return None;
    }
    let mut value = 0_u64;
    for byte in digits {
        let nibble = match byte {
            b'0'..=b'9' => u64::from(byte - b'0'),
            b'a'..=b'f' => u64::from(byte - b'a') + 10,
            b'A'..=b'F' => u64::from(byte - b'A') + 10,
            _ => return None,
        };
        value = value.checked_mul(16)?.checked_add(nibble)?;
    }
    Some(value)
}

/// `/proc/<pid>/stat`: the state character follows the last `)` of the command name.
fn parse_stat_state(bytes: &[u8]) -> Option<u8> {
    let close = bytes.iter().rposition(|byte| *byte == b')')?;
    bytes[close + 1..]
        .iter()
        .copied()
        .find(u8::is_ascii_alphabetic)
}

/// `/proc/<pid>/syscall`: `nr arg0 .. arg5 sp pc`, or `running`.
fn parse_syscall(bytes: &[u8]) -> Option<(i64, u64)> {
    let mut tokens = bytes
        .split(|byte| *byte == b' ')
        .filter(|token| !token.is_empty());
    let nr = parse_i64_dec(tokens.next()?)?;
    let arg0 = tokens.next().and_then(parse_u64_hex).unwrap_or(0);
    Some((nr, arg0))
}

/// `/proc/<pid>/schedstat`: `run_ns wait_ns timeslices`.
fn parse_schedstat(bytes: &[u8]) -> Option<(u64, u64)> {
    let mut tokens = bytes
        .split(|byte| *byte == b' ')
        .filter(|token| !token.is_empty());
    let run = parse_u64_dec(tokens.next()?)?;
    let wait = parse_u64_dec(tokens.next()?)?;
    Some((run, wait))
}

fn copy_proc_file(
    src_dir: BorrowedFd<'_>,
    name: &str,
    dst_dir: BorrowedFd<'_>,
    dst_name: &str,
    sanitize_nul: bool,
    deadline: Instant,
) {
    let Ok(source) = rustix::fs::openat(
        src_dir,
        name,
        OFlags::RDONLY | OFlags::CLOEXEC,
        Mode::empty(),
    ) else {
        return;
    };
    let Ok(target) = rustix::fs::openat(
        dst_dir,
        dst_name,
        OFlags::CREATE | OFlags::WRONLY | OFlags::TRUNC | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    ) else {
        return;
    };

    let mut chunk = [0_u8; 8192];
    while Instant::now() < deadline {
        match rustix::io::read(&source, &mut chunk) {
            Ok(0) => break,
            Ok(read) => {
                let slice = &mut chunk[..read];
                if sanitize_nul {
                    for byte in slice.iter_mut() {
                        if *byte == 0 {
                            *byte = b' ';
                        }
                    }
                }
                if write_all(target.as_fd(), slice).is_err() {
                    break;
                }
            }
            Err(Errno::INTR) => {}
            Err(_) => break,
        }
    }
}

/// Pre-opened filesystem state so the dump path never allocates.
struct Paths {
    root: OwnedFd,
    root_display: String,
    proc_self: OwnedFd,
    event_loop_tid: i32,
}

impl Paths {
    fn prepare() -> Option<Self> {
        let state_home = std::env::var_os("XDG_STATE_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| {
                    let mut path = std::path::PathBuf::from(home);
                    path.push(".local/state");
                    path
                })
            })?;
        let root = state_home.join("koma").join("stalls");
        if let Err(err) = std::fs::create_dir_all(&root) {
            warn!(
                path = %root.display(),
                "stall watchdog disabled: cannot create evidence directory: {err}"
            );
            return None;
        }
        let root_file = File::open(&root).ok()?;
        let proc_self = File::open("/proc/self").ok()?;

        Some(Self {
            root: OwnedFd::from(root_file),
            root_display: root.display().to_string(),
            proc_self: OwnedFd::from(proc_self),
            event_loop_tid: current_tid(),
        })
    }
}

fn current_tid() -> i32 {
    #[allow(unsafe_code)]
    // SAFETY: `gettid` is a side-effect-free syscall wrapper in libc that cannot fail.
    unsafe {
        libc::gettid()
    }
}

/// Collects every thread of this process into `out`, returning the captured count.
fn read_task_file(
    task_dir: BorrowedFd<'_>,
    path: &mut Line,
    prefix_len: usize,
    name: &str,
    buf: &mut [u8],
) -> Option<usize> {
    path.len = prefix_len;
    path.push_str(name);
    let path = std::str::from_utf8(path.as_bytes()).ok()?;
    let fd = rustix::fs::openat(
        task_dir,
        path,
        OFlags::RDONLY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .ok()?;
    read_small(fd.as_fd(), buf).ok()
}

/// Collects every thread of this process into `out`, returning the captured count.
fn capture_threads(
    paths: &Paths,
    out: &mut [ThreadSnapshot; MAX_THREADS],
    deadline: Instant,
) -> usize {
    let Ok(task_dir) = rustix::fs::openat(
        &paths.proc_self,
        "task",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    ) else {
        return 0;
    };

    let mut dir_buffer = [std::mem::MaybeUninit::uninit(); 4096];
    let mut captured = 0_usize;
    let mut raw = rustix::fs::RawDir::new(&task_dir, &mut dir_buffer);

    while let Some(entry) = raw.next() {
        if Instant::now() >= deadline || captured == MAX_THREADS {
            break;
        }
        let Ok(entry) = entry else { break };
        let name = entry.file_name();
        let Some(name) = name.to_str().ok() else {
            continue;
        };
        let Some(tid) = i32::try_from(parse_u64_dec(name.as_bytes()).unwrap_or(0)).ok() else {
            continue;
        };
        if tid <= 0 {
            continue;
        }

        let mut path = Line::new();
        path.push_str(name);
        path.push_str("/");
        let prefix_len = path.len;
        let task_dir = task_dir.as_fd();

        let mut snapshot = ThreadSnapshot {
            tid,
            ..ThreadSnapshot::default()
        };

        let mut comm = [0_u8; COMM_CAP];
        if let Some(len) = read_task_file(task_dir, &mut path, prefix_len, "comm", &mut comm) {
            snapshot.comm_len = len.min(COMM_CAP);
            snapshot.comm[..snapshot.comm_len].copy_from_slice(&comm[..snapshot.comm_len]);
        }

        let mut stat = [0_u8; 512];
        if let Some(len) = read_task_file(task_dir, &mut path, prefix_len, "stat", &mut stat) {
            snapshot.state = parse_stat_state(&stat[..len]).unwrap_or(b'?');
        }

        let mut wchan = [0_u8; WCHAN_CAP];
        if let Some(len) = read_task_file(task_dir, &mut path, prefix_len, "wchan", &mut wchan) {
            snapshot.wchan_len = len.min(WCHAN_CAP);
            snapshot.wchan[..snapshot.wchan_len].copy_from_slice(&wchan[..snapshot.wchan_len]);
        }

        let mut syscall = [0_u8; SYSCALL_CAP];
        if let Some(len) = read_task_file(task_dir, &mut path, prefix_len, "syscall", &mut syscall)
        {
            snapshot.syscall = parse_syscall(&syscall[..len]);
            snapshot.syscall_raw_len = len;
            snapshot.syscall_raw[..len].copy_from_slice(&syscall[..len]);
        }

        let mut schedstat = [0_u8; 96];
        if let Some(len) =
            read_task_file(task_dir, &mut path, prefix_len, "schedstat", &mut schedstat)
            && let Some((run, wait)) = parse_schedstat(&schedstat[..len])
        {
            snapshot.run_ns = run;
            snapshot.wait_ns = wait;
        }

        out[captured] = snapshot;
        captured += 1;
    }

    captured
}

/// Writes evidence with stack buffers and pre-opened descriptors only.
fn write_evidence(paths: &Paths, stalled_micros: u64, budget: Duration) -> Option<Verdict> {
    let deadline = Instant::now() + budget;

    // Timestamp directory name, formatted without allocation.
    let mut stamp = Line::new();
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| u64::try_from(since.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0);
    stamp.push_u64(millis);
    let stamp = std::str::from_utf8(stamp.as_bytes()).ok()?;

    let _ = rustix::fs::mkdirat(&paths.root, stamp, Mode::RWXU);
    let dump_dir = rustix::fs::openat(
        &paths.root,
        stamp,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .ok()?;

    let mut threads = [ThreadSnapshot::default(); MAX_THREADS];
    let captured = capture_threads(paths, &mut threads, deadline);
    let verdict = classify(&threads[..captured], paths.event_loop_tid);

    if let Ok(fd) = rustix::fs::openat(
        &dump_dir,
        "threads.txt",
        OFlags::CREATE | OFlags::WRONLY | OFlags::TRUNC | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    ) {
        let mut line = Line::new();
        for thread in &threads[..captured] {
            line.clear();
            line.push_str("tid=");
            line.push_i64(i64::from(thread.tid));
            line.push_str(" comm=");
            line.push_str(thread.comm());
            line.push_str(" state=");
            line.push_bytes(&[thread.state]);
            line.push_str(" wchan=");
            line.push_str(thread.wchan());
            line.push_str(" schedstat(run_ns=");
            line.push_u64(thread.run_ns);
            line.push_str(" wait_ns=");
            line.push_u64(thread.wait_ns);
            line.push_str(") syscall=");
            if thread.syscall_raw_len > 0 {
                line.push_bytes(&thread.syscall_raw[..thread.syscall_raw_len]);
            } else {
                line.push_str("unavailable");
            }
            if let Some((nr, arg0)) = thread.syscall {
                line.push_str(" syscall_nr=");
                line.push_i64(nr);
                if nr == SYS_FUTEX_NR {
                    line.push_str(" futex_uaddr=");
                    line.push_hex(arg0);
                }
            }
            line.push_str("\n");
            if write_all(fd.as_fd(), line.as_bytes()).is_err() {
                break;
            }
        }
    }

    copy_proc_file(
        paths.proc_self.as_fd(),
        "maps",
        dump_dir.as_fd(),
        "maps.txt",
        false,
        deadline,
    );
    copy_proc_file(
        paths.proc_self.as_fd(),
        "cmdline",
        dump_dir.as_fd(),
        "cmdline.txt",
        true,
        deadline,
    );

    if let Ok(fd) = rustix::fs::openat(
        &dump_dir,
        "summary.txt",
        OFlags::CREATE | OFlags::WRONLY | OFlags::TRUNC | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    ) {
        let mut line = Line::new();
        line.push_str("stalled_ms=");
        line.push_u64(stalled_micros / 1000);
        line.push_str("\nevent_loop_tid=");
        line.push_i64(i64::from(paths.event_loop_tid));
        line.push_str("\nthreads_captured=");
        line.push_u64(captured as u64);
        line.push_str("\nfutex_waiters=");
        line.push_u64(verdict.futex_waiters as u64);
        line.push_str("\nkernel_blocked=");
        line.push_u64(verdict.kernel_blocked as u64);
        line.push_str("\nclassification=");
        line.push_str(verdict.kind.label());
        line.push_str("\nfutex_address=");
        match verdict.address {
            Some(address) => line.push_hex(address),
            None => line.push_str("none"),
        }
        line.push_str("\n");
        let _ = write_all(fd.as_fd(), line.as_bytes());
    }

    // Only now, with the evidence safely on disk, may we format and log.
    error!(
        dir = %format!("{}/{}", paths.root_display, stamp),
        stalled_ms = stalled_micros / 1000,
        classification = verdict.kind.label(),
        summary = %verdict.one_line(),
        "compositor event loop stalled; writable self-diagnostic captured under the state directory"
    );

    Some(verdict)
}

/// Heartbeat plus side-thread lifecycle.
pub struct StallWatchdog {
    started: Instant,
    heartbeat: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl StallWatchdog {
    /// Starts the watchdog unless it is disabled or the evidence directory is unusable.
    ///
    /// Must be called from the event-loop thread: that thread id is recorded so the classifier
    /// can distinguish a self-reentrant wait from any other thread's wait.
    #[must_use]
    pub fn start() -> Option<Self> {
        let stall_secs = std::env::var(STALL_SECS_ENV)
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or(DEFAULT_STALL_SECS);
        if stall_secs == 0 {
            return None;
        }
        let budget_ms = std::env::var(STALL_BUDGET_ENV)
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or(DEFAULT_BUDGET_MS)
            .clamp(50, 10_000);

        let Some(paths) = Paths::prepare() else {
            warn!("stall watchdog disabled: no usable XDG state directory for evidence dumps");
            return None;
        };
        let started = Instant::now();
        let heartbeat = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));

        let thread_heartbeat = Arc::clone(&heartbeat);
        let thread_stop = Arc::clone(&stop);
        let thread_started = started;
        let stall_micros = stall_secs.saturating_mul(1_000_000);
        let budget = Duration::from_millis(budget_ms);
        let root_display = paths.root_display.clone();

        let thread = thread::Builder::new()
            .name("koma-stall".into())
            .spawn(move || {
                let mut reported_heartbeat: Option<u64> = None;
                loop {
                    thread::sleep(POLL_INTERVAL);
                    if thread_stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let beat = thread_heartbeat.load(Ordering::Relaxed);
                    let elapsed = thread_started
                        .elapsed()
                        .as_micros()
                        .min(u128::from(u64::MAX)) as u64;
                    let stalled_for = elapsed.saturating_sub(beat);
                    if stalled_for >= stall_micros {
                        // One dump per stall episode: the same heartbeat means the loop is still
                        // wedged, so re-dumping would only burn the budget.
                        if reported_heartbeat != Some(beat) {
                            reported_heartbeat = Some(beat);
                            let _ = write_evidence(&paths, stalled_for, budget);
                        }
                    } else {
                        reported_heartbeat = None;
                    }
                }
            })
            .ok()?;

        tracing::info!(
            stall_seconds = stall_secs,
            evidence_root = %root_display,
            "stall watchdog armed (writes only when the event loop stops ticking)"
        );

        Some(Self {
            started,
            heartbeat,
            stop,
            thread: Some(thread),
        })
    }

    /// Records one event-loop iteration. One relaxed store, no syscalls beyond the clock read.
    pub fn tick(&self) {
        let micros = self.started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
        self.heartbeat.store(micros, Ordering::Relaxed);
    }
}

impl Drop for StallWatchdog {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thread(tid: i32, state: u8, wchan: &str, syscall: Option<(i64, u64)>) -> ThreadSnapshot {
        let mut snapshot = ThreadSnapshot {
            tid,
            state,
            syscall,
            ..ThreadSnapshot::default()
        };
        let bytes = wchan.as_bytes();
        snapshot.wchan_len = bytes.len().min(WCHAN_CAP);
        snapshot.wchan[..snapshot.wchan_len].copy_from_slice(&bytes[..snapshot.wchan_len]);
        let comm = b"koma";
        snapshot.comm_len = comm.len();
        snapshot.comm[..comm.len()].copy_from_slice(comm);
        snapshot
    }

    fn futex_wait(tid: i32, address: u64) -> ThreadSnapshot {
        thread(
            tid,
            b'S',
            "futex_wait_queue_me",
            Some((SYS_FUTEX_NR, address)),
        )
    }

    #[test]
    fn lone_event_loop_waiter_is_a_self_reentrant_mutex() {
        let threads = [
            futex_wait(100, 0x7f00_dead_0000),
            thread(101, b'R', "", None),
            thread(102, b'S', "epoll_wait", Some((232, 0))),
        ];
        let verdict = classify(&threads, 100);
        assert_eq!(verdict.kind, StallKind::SelfReentrant);
        assert_eq!(verdict.address, Some(0x7f00_dead_0000));
        assert_eq!(verdict.tids(), &[100]);
        assert_eq!(verdict.futex_waiters, 1);
        assert!(verdict.one_line().contains("self-reentrant-mutex"));
    }

    #[test]
    fn two_waiters_on_one_address_are_a_thread_cycle() {
        let threads = [
            futex_wait(200, 0x55aa_0000),
            futex_wait(201, 0x55aa_0000),
            thread(202, b'S', "futex_wait", Some((SYS_FUTEX_NR, 0x99))),
        ];
        let verdict = classify(&threads, 200);
        assert_eq!(verdict.kind, StallKind::ThreadCycle);
        assert_eq!(verdict.address, Some(0x55aa_0000));
        assert_eq!(verdict.tids(), &[200, 201]);
    }

    #[test]
    fn a_cycle_not_involving_the_event_loop_thread_is_still_reported() {
        let threads = [futex_wait(300, 0xabc0), futex_wait(301, 0xabc0)];
        let verdict = classify(&threads, 999);
        assert_eq!(verdict.kind, StallKind::ThreadCycle);
        assert_eq!(verdict.tids(), &[300, 301]);
    }

    #[test]
    fn uninterruptible_sleep_without_futex_is_kernel_blocked() {
        let threads = [
            thread(400, b'D', "io_schedule", Some((0, 0))),
            thread(401, b'R', "", None),
        ];
        let verdict = classify(&threads, 401);
        assert_eq!(verdict.kind, StallKind::KernelBlocked);
        assert_eq!(verdict.tids(), &[400]);
        assert_eq!(verdict.kernel_blocked, 1);
        assert_eq!(verdict.address, None);
    }

    #[test]
    fn lone_non_loop_futex_waiter_is_not_blamed_on_the_loop() {
        let threads = [futex_wait(500, 0x1234), thread(501, b'R', "", None)];
        let verdict = classify(&threads, 501);
        assert_eq!(verdict.kind, StallKind::FutexWaitOther);
        assert_eq!(verdict.tids(), &[500]);
    }

    #[test]
    fn distinct_futex_addresses_are_reported_as_scattered() {
        let threads = [
            futex_wait(600, 0x10),
            futex_wait(601, 0x20),
            futex_wait(602, 0x30),
        ];
        let verdict = classify(&threads, 603);
        assert_eq!(verdict.kind, StallKind::ScatteredFutex);
        assert_eq!(verdict.futex_waiters, 3);
    }

    #[test]
    fn empty_capture_is_unexplained() {
        let verdict = classify(&[], 1);
        assert_eq!(verdict.kind, StallKind::Unexplained);
        assert!(verdict.tids().is_empty());
    }

    #[test]
    fn futex_address_requires_the_futex_syscall() {
        let snapshot = thread(700, b'S', "futex_wait", Some((35, 0xfeed)));
        assert!(snapshot.is_futex_wait());
        assert_eq!(snapshot.futex_address(), None);
    }

    #[test]
    fn proc_parsers_handle_kernel_formats() {
        assert_eq!(parse_stat_state(b"4242 (koma worker) S 1 4242"), Some(b'S'));
        assert_eq!(
            parse_syscall(b"202 0x7f8c12340000 0x81 0x0 0x0 0x0 0x0 0x7ffd 0x55aa"),
            Some((202, 0x7f8c_1234_0000))
        );
        assert_eq!(parse_syscall(b"running"), None);
        assert_eq!(parse_schedstat(b"100 200 3"), Some((100, 200)));
        assert_eq!(parse_i64_dec(b"-1"), Some(-1));
        assert_eq!(parse_u64_hex(b"0xdeadbeef"), Some(0xdead_beef));
    }
}
