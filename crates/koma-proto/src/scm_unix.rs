//! Non-blocking Unix domain socket framing with `SCM_RIGHTS` FD passing, kernel peer credential
//! verification (`SO_PEERCRED` / `LOCAL_PEERCRED` / `getpeereid`), and write-sealed `memfd` creation
//! (`IPC_SPEC.md` §2.1, §2.2, §5.4).
//!
//! Uses `rustix`'s safe `OwnedFd`/`BorrowedFd` abstractions with zero raw `cmsg` pointer arithmetic.

use std::{
    collections::VecDeque,
    ffi::CString,
    fs::File,
    io::{self, IoSlice, IoSliceMut, Seek as _, SeekFrom, Write as _},
    mem::MaybeUninit,
    os::{
        fd::{AsFd as _, BorrowedFd, OwnedFd},
        unix::net::UnixStream,
    },
    sync::atomic::{AtomicU64, Ordering},
};

use rustix::{
    cmsg_space,
    io::Errno,
    net::{
        RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
        SendAncillaryMessage, SendFlags, recvmsg, sendmsg,
    },
    process::getuid,
    shm,
};
use serde::{Serialize, de::DeserializeOwned};

use crate::frame::{
    FrameDecodeError, MAX_FRAME_BYTES, MAX_SCM_FDS, decode_frame_header, encode_frame,
};

pub const MAX_OUTBOX_FRAMES: usize = 64;
pub const MAX_OUTBOX_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerCreds {
    pub uid: u32,
    pub gid: u32,
}

/// Kernel-verified Unix socket peer identity (`pid` where reported by the kernel, plus `uid` and `gid`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SocketPeerCreds {
    pub pid: Option<i32>,
    pub uid: u32,
    pub gid: u32,
}

/// Queries kernel-verified peer credentials (`pid`, `uid`, `gid`) for a connected Unix stream socket.
///
/// Uses `SO_PEERCRED` on Linux/Android/OpenBSD, `LOCAL_PEERCRED` (`xucred`) on FreeBSD, and
/// `getpeereid` on other BSD/Darwin/Unix platforms. Fails closed (`Unsupported`) if an OS has no
/// verified probe.
pub fn query_socket_peer_creds(stream: &UnixStream) -> io::Result<SocketPeerCreds> {
    query_socket_peer_creds_impl(stream)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn query_socket_peer_creds_impl(stream: &UnixStream) -> io::Result<SocketPeerCreds> {
    let ucred = rustix::net::sockopt::socket_peercred(stream)
        .map_err(|e| io::Error::from_raw_os_error(e.raw_os_error()))?;
    Ok(SocketPeerCreds {
        pid: Some(ucred.pid.as_raw_nonzero().get()),
        uid: ucred.uid.as_raw(),
        gid: ucred.gid.as_raw(),
    })
}

#[cfg(target_os = "freebsd")]
#[allow(unsafe_code)]
fn query_socket_peer_creds_impl(stream: &UnixStream) -> io::Result<SocketPeerCreds> {
    use std::os::fd::AsRawFd as _;

    let mut cred = MaybeUninit::<libc::xucred>::zeroed();
    let mut len = std::mem::size_of::<libc::xucred>() as libc::socklen_t;
    // SAFETY: `stream` is a live Unix stream FD and `cred` points to writable `xucred` storage.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_LOCAL,
            libc::LOCAL_PEERCRED,
            cred.as_mut_ptr().cast::<libc::c_void>(),
            &raw mut len,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    if (len as usize) < std::mem::size_of::<libc::xucred>() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "truncated FreeBSD LOCAL_PEERCRED xucred payload",
        ));
    }
    // SAFETY: `getsockopt` succeeded and populated the entire `libc::xucred` struct.
    let cred = unsafe { cred.assume_init() };
    if cred.cr_version != libc::XUCRED_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported FreeBSD xucred version {}", cred.cr_version),
        ));
    }
    let gid = if cred.cr_ngroups > 0 {
        cred.cr_groups[0]
    } else {
        0
    };
    // SAFETY: On FreeBSD 13+, the kernel writes `cr_pid` into this union; earlier kernels leave
    // the pointer slot zeroed, so non-positive values map to `None`.
    let raw_pid = unsafe { cred.cr_pid__c_anonymous_union.cr_pid };
    Ok(SocketPeerCreds {
        pid: (raw_pid > 0).then_some(raw_pid),
        uid: cred.cr_uid,
        gid,
    })
}

#[cfg(target_os = "openbsd")]
#[allow(unsafe_code)]
fn query_socket_peer_creds_impl(stream: &UnixStream) -> io::Result<SocketPeerCreds> {
    use std::os::fd::AsRawFd as _;

    let mut cred = MaybeUninit::<libc::sockpeercred>::zeroed();
    let mut len = std::mem::size_of::<libc::sockpeercred>() as libc::socklen_t;
    // SAFETY: `stream` is a live Unix stream FD and `cred` points to writable `sockpeercred` storage.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            cred.as_mut_ptr().cast::<libc::c_void>(),
            &raw mut len,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    if (len as usize) < std::mem::size_of::<libc::sockpeercred>() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "truncated OpenBSD SO_PEERCRED sockpeercred payload",
        ));
    }
    // SAFETY: `getsockopt` succeeded and populated the entire `libc::sockpeercred` struct.
    let cred = unsafe { cred.assume_init() };
    Ok(SocketPeerCreds {
        pid: (cred.pid > 0).then_some(cred.pid),
        uid: cred.uid,
        gid: cred.gid,
    })
}

#[cfg(any(
    target_os = "dragonfly",
    target_os = "netbsd",
    target_os = "macos",
    target_os = "ios",
    target_os = "aix",
    target_os = "cygwin",
    target_os = "nto"
))]
#[allow(unsafe_code)]
fn query_socket_peer_creds_impl(stream: &UnixStream) -> io::Result<SocketPeerCreds> {
    use std::os::fd::AsRawFd as _;

    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    // SAFETY: `stream` is a live Unix stream FD and `uid`/`gid` are valid out-pointers.
    let rc = unsafe { libc::getpeereid(stream.as_raw_fd(), &raw mut uid, &raw mut gid) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(SocketPeerCreds {
        pid: None,
        uid: uid as u32,
        gid: gid as u32,
    })
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "macos",
    target_os = "ios",
    target_os = "aix",
    target_os = "cygwin",
    target_os = "nto"
)))]
fn query_socket_peer_creds_impl(_stream: &UnixStream) -> io::Result<SocketPeerCreds> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "kernel Unix socket peer credential verification is unsupported on this OS",
    ))
}

fn ensure_same_uid(peer: SocketPeerCreds, host_uid: u32) -> io::Result<PeerCreds> {
    if peer.uid != host_uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "koma-admin.sock rejected foreign UID {} (compositor UID is {})",
                peer.uid, host_uid
            ),
        ));
    }
    Ok(PeerCreds {
        uid: peer.uid,
        gid: peer.gid,
    })
}

/// Verifies that a client connecting to `koma-admin.sock` runs under the exact same UID as `koma`.
pub fn verify_same_uid_peer(stream: &UnixStream) -> io::Result<PeerCreds> {
    let creds = query_socket_peer_creds(stream)?;
    ensure_same_uid(creds, getuid().as_raw())
}

static SHM_SEQ: AtomicU64 = AtomicU64::new(0);
const POSIX_SHM_MAX_ATTEMPTS: usize = 8;
// OpenBSD and macOS cap POSIX SHM names at 31 bytes (`PSHMNAMLEN` / `SHM_NAME_MAX`), leaving 30 non-NUL bytes.
const POSIX_SHM_MAX_NAME_LEN: usize = 30;

#[cfg_attr(any(target_os = "linux", target_os = "freebsd"), allow(dead_code))]
struct ShmUnlinkGuard(Option<CString>);

impl ShmUnlinkGuard {
    #[cfg_attr(any(target_os = "linux", target_os = "freebsd"), allow(dead_code))]
    fn name(&self) -> &CString {
        self.0.as_ref().expect("ShmUnlinkGuard path still present")
    }

    #[cfg_attr(any(target_os = "linux", target_os = "freebsd"), allow(dead_code))]
    fn unlink(mut self) -> io::Result<()> {
        if let Some(cname) = self.0.take() {
            shm::unlink(&cname).map_err(errno_to_io_error)?;
        }
        Ok(())
    }
}

impl Drop for ShmUnlinkGuard {
    fn drop(&mut self) {
        if let Some(cname) = self.0.take() {
            let _ = shm::unlink(&cname);
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn sanitize_memfd_cname(label: &str) -> CString {
    let clean_name: String = label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(48)
        .collect();
    CString::new(if clean_name.is_empty() {
        "koma_frame".to_owned()
    } else {
        clean_name
    })
    .expect("sanitized ASCII CString")
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn create_memfd_with_flags(
    label: &str,
    payload: &[u8],
    flags: rustix::fs::MemfdFlags,
) -> io::Result<OwnedFd> {
    let cname = sanitize_memfd_cname(label);
    let fd = rustix::fs::memfd_create(&cname, flags)
        .map_err(|e| io::Error::from_raw_os_error(e.raw_os_error()))?;

    let mut file = File::from(fd);
    file.write_all(payload)?;
    file.seek(SeekFrom::Start(0))?;
    Ok(OwnedFd::from(file))
}

#[cfg_attr(any(target_os = "linux", target_os = "freebsd"), allow(dead_code))]
fn open_populated_posix_shm_rw(payload: &[u8]) -> io::Result<(File, ShmUnlinkGuard)> {
    let pid = std::process::id();
    for attempt in 0..POSIX_SHM_MAX_ATTEMPTS {
        let seq = SHM_SEQ.fetch_add(1, Ordering::Relaxed);
        let name = format!("/km_{pid:x}_{seq:x}");
        debug_assert!(
            name.len() <= POSIX_SHM_MAX_NAME_LEN,
            "POSIX SHM name exceeded {POSIX_SHM_MAX_NAME_LEN} bytes: {name}"
        );
        let cname =
            CString::new(name).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

        let fd = match shm::open(
            &cname,
            shm::OFlags::RDWR | shm::OFlags::CREATE | shm::OFlags::EXCL,
            shm::Mode::RUSR | shm::Mode::WUSR,
        ) {
            Ok(fd) => fd,
            Err(Errno::EXIST) if attempt + 1 < POSIX_SHM_MAX_ATTEMPTS => continue,
            Err(err) => return Err(errno_to_io_error(err)),
        };

        let guard = ShmUnlinkGuard(Some(cname));
        let mut rw_file = File::from(fd);
        let len_u64 = u64::try_from(payload.len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "POSIX SHM payload exceeds u64 range",
            )
        })?;
        // POSIX `shm_open` objects start at 0 bytes; FreeBSD/POSIX do not extend them via `write()`.
        rw_file.set_len(len_u64)?;
        if !payload.is_empty() {
            rw_file.write_all(payload)?;
            rw_file.seek(SeekFrom::Start(0))?;
        }
        return Ok((rw_file, guard));
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "exhausted unique POSIX SHM name retries",
    ))
}

#[cfg_attr(any(target_os = "linux", target_os = "freebsd"), allow(dead_code))]
fn create_shm_readonly_fd(_label: &str, payload: &[u8]) -> io::Result<OwnedFd> {
    let (rw_file, guard) = open_populated_posix_shm_rw(payload)?;

    // Keep `rw_file` alive across the `O_RDONLY` reopen so the kernel retains the object before unlink.
    let ro_fd = shm::open(guard.name(), shm::OFlags::RDONLY, shm::Mode::empty())
        .map_err(errno_to_io_error)?;
    let mut ro_file = File::from(ro_fd);
    guard.unlink()?;
    drop(rw_file);
    ro_file.seek(SeekFrom::Start(0))?;
    Ok(OwnedFd::from(ro_file))
}

/// Allocates an anonymous RAM-backed buffer, writes `payload`, rewinds to offset `0`, and returns
/// a read-only descriptor so untrusted IPC peers can `mmap` `PROT_READ` zero-copy without ever
/// mutating or truncating the buffer (`IPC_SPEC.md` §5.4).
///
/// Uses `memfd_create` + `F_SEAL_*` on Linux and FreeBSD, and an unlinked `O_RDONLY` POSIX SHM
/// descriptor on other Unix targets (such as OpenBSD, NetBSD, and macOS).
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub fn create_sealed_readonly_memfd(label: &str, payload: &[u8]) -> io::Result<OwnedFd> {
    use rustix::fs::{MemfdFlags, SealFlags, fcntl_add_seals};

    let fd = create_memfd_with_flags(
        label,
        payload,
        MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
    )?;
    fcntl_add_seals(
        &fd,
        SealFlags::SHRINK | SealFlags::GROW | SealFlags::WRITE | SealFlags::SEAL,
    )
    .map_err(|e| io::Error::from_raw_os_error(e.raw_os_error()))?;

    Ok(fd)
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "freebsd"))))]
pub fn create_sealed_readonly_memfd(label: &str, payload: &[u8]) -> io::Result<OwnedFd> {
    create_shm_readonly_fd(label, payload)
}

/// Allocates an anonymous RAM-backed shared memory descriptor, writes `payload`, and rewinds to
/// offset `0` for `wl_shm` clients (`memfd_create` on Linux/FreeBSD, unlinked `shm_open` elsewhere).
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub fn create_writable_shm_fd(label: &str, payload: &[u8]) -> io::Result<OwnedFd> {
    create_memfd_with_flags(label, payload, rustix::fs::MemfdFlags::CLOEXEC)
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "freebsd"))))]
pub fn create_writable_shm_fd(_label: &str, payload: &[u8]) -> io::Result<OwnedFd> {
    let (rw_file, guard) = open_populated_posix_shm_rw(payload)?;
    guard.unlink()?;
    Ok(OwnedFd::from(rw_file))
}

/// Sends a byte slice over a `UnixStream`, attaching up to `MAX_SCM_FDS` (`4`) descriptors via `SCM_RIGHTS`.
pub fn send_chunk_with_scm_rights(
    stream: &UnixStream,
    bytes: &[u8],
    fds: &[BorrowedFd<'_>],
) -> io::Result<usize> {
    if fds.len() > MAX_SCM_FDS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "cannot attach {} FDs (MAX_SCM_FDS = {MAX_SCM_FDS})",
                fds.len()
            ),
        ));
    }

    let iov = [IoSlice::new(bytes)];
    let res = if fds.is_empty() {
        let mut empty = SendAncillaryBuffer::default();
        sendmsg(stream, &iov, &mut empty, SendFlags::NOSIGNAL)
    } else {
        let mut space = [MaybeUninit::uninit(); cmsg_space!(ScmRights(MAX_SCM_FDS))];
        let mut cmsg_buf = SendAncillaryBuffer::new(&mut space);
        let pushed = cmsg_buf.push(SendAncillaryMessage::ScmRights(fds));
        if !pushed {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "insufficient ancillary cmsg space for ScmRights",
            ));
        }
        sendmsg(stream, &iov, &mut cmsg_buf, SendFlags::NOSIGNAL)
    };

    res.map_err(errno_to_io_error)
}

/// Receives bytes and any attached `SCM_RIGHTS` file descriptors (atomically marked `FD_CLOEXEC`).
pub fn recv_chunk_with_scm_rights(
    stream: &UnixStream,
    buf: &mut [u8],
) -> io::Result<(usize, Vec<OwnedFd>)> {
    let mut iov = [IoSliceMut::new(buf)];
    let mut space = [MaybeUninit::uninit(); cmsg_space!(ScmRights(MAX_SCM_FDS))];
    let mut cmsg_buf = RecvAncillaryBuffer::new(&mut space);

    let outcome = recvmsg(stream, &mut iov, &mut cmsg_buf, RecvFlags::CMSG_CLOEXEC)
        .map_err(errno_to_io_error)?;

    let mut received_fds = Vec::new();
    for msg in cmsg_buf.drain() {
        if let RecvAncillaryMessage::ScmRights(fds) = msg {
            received_fds.extend(fds);
        }
    }

    Ok((outcome.bytes, received_fds))
}

fn errno_to_io_error(err: Errno) -> io::Error {
    if err == Errno::AGAIN || err == Errno::WOULDBLOCK {
        io::Error::from(io::ErrorKind::WouldBlock)
    } else {
        io::Error::from_raw_os_error(err.raw_os_error())
    }
}

/// Outbound frame queued on a non-blocking client connection.
#[derive(Debug)]
pub struct OutboundFrame {
    bytes: Vec<u8>,
    written: usize,
    /// `SCM_RIGHTS` FDs attached exclusively on the first `sendmsg` (`written == 0`) and cleared
    /// immediately once `n > 0` bytes transfer so partial writes never re-send duplicate FDs!
    fds: Vec<OwnedFd>,
}

/// Bounded non-blocking writer queue protecting `Koma`'s `240 Hz` VSync loop from wedged clients.
#[derive(Debug, Default)]
pub struct FrameOutbox {
    queue: VecDeque<OutboundFrame>,
    queued_bytes: usize,
}

impl FrameOutbox {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Enqueues a serialized wire frame + optional ancillary FDs.
    /// Returns `Err` if the client has exceeded watermark limits (caller drops wedged socket).
    pub fn enqueue<T: Serialize>(&mut self, msg: &T, fds: Vec<OwnedFd>) -> Result<(), String> {
        if self.queue.len() >= MAX_OUTBOX_FRAMES {
            return Err("client outbox frame watermark exceeded".to_owned());
        }
        let bytes = encode_frame(msg).map_err(|e| e.to_string())?;
        if self.queued_bytes.saturating_add(bytes.len()) > MAX_OUTBOX_BYTES {
            return Err("client outbox byte watermark exceeded".to_owned());
        }
        self.queued_bytes += bytes.len();
        self.queue.push_back(OutboundFrame {
            bytes,
            written: 0,
            fds,
        });
        Ok(())
    }

    /// Flushes as many queued bytes/FDs as possible without blocking (`WouldBlock` returns `Ok(())`).
    pub fn try_flush(&mut self, stream: &UnixStream) -> io::Result<()> {
        while let Some(front) = self.queue.front_mut() {
            let remaining = &front.bytes[front.written..];
            let sent = if front.written == 0 && !front.fds.is_empty() {
                let borrowed: Vec<BorrowedFd<'_>> = front.fds.iter().map(|f| f.as_fd()).collect();
                send_chunk_with_scm_rights(stream, remaining, &borrowed)?
            } else {
                send_chunk_with_scm_rights(stream, remaining, &[])?
            };

            if sent == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "peer closed Unix socket during write",
                ));
            }

            if front.written == 0 {
                // Ancillary FDs were delivered with the initial byte of this frame; drop our copies.
                front.fds.clear();
            }
            front.written += sent;
            self.queued_bytes = self.queued_bytes.saturating_sub(sent);

            if front.written >= front.bytes.len() {
                self.queue.pop_front();
            }
        }
        Ok(())
    }
}

#[derive(Debug, Default)]
pub struct FrameInbox {
    buf: Vec<u8>,
    pending_fds: VecDeque<OwnedFd>,
}

#[derive(Debug)]
pub enum InboxPoll {
    Frame {
        json_bytes: Vec<u8>,
        fds: Vec<OwnedFd>,
    },
    NeedMoreData,
    PeerClosed,
}

impl FrameInbox {
    /// Reads available non-blocking bytes + `SCM_RIGHTS` FDs from `stream` into the internal buffer.
    pub fn read_available(&mut self, stream: &UnixStream) -> io::Result<bool> {
        let mut scratch = [0_u8; 16 * 1024];
        let mut eof = false;
        loop {
            match recv_chunk_with_scm_rights(stream, &mut scratch) {
                Ok((0, _)) => {
                    eof = true;
                    break;
                }
                Ok((n, fds)) => {
                    self.buf.extend_from_slice(&scratch[..n]);
                    self.pending_fds.extend(fds);
                    if self.buf.len() > MAX_FRAME_BYTES + 4 {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "inbound buffer exceeded 16 MiB frame ceiling",
                        ));
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }
        Ok(eof)
    }

    /// Extracts the next complete length-prefixed JSON frame if all bytes have arrived.
    pub fn next_frame(&mut self, saw_eof: bool) -> Result<InboxPoll, FrameDecodeError> {
        if self.buf.len() < 4 {
            return if saw_eof && self.buf.is_empty() {
                Ok(InboxPoll::PeerClosed)
            } else if saw_eof {
                Err(FrameDecodeError::Json(
                    "truncated 4-byte frame header on EOF".into(),
                ))
            } else {
                Ok(InboxPoll::NeedMoreData)
            };
        }

        let header = [self.buf[0], self.buf[1], self.buf[2], self.buf[3]];
        let body_len = decode_frame_header(header)?;
        if self.buf.len() < 4 + body_len {
            return if saw_eof {
                Err(FrameDecodeError::Json(
                    "truncated JSON payload on EOF".into(),
                ))
            } else {
                Ok(InboxPoll::NeedMoreData)
            };
        }

        let json_bytes = self.buf[4..4 + body_len].to_vec();
        self.buf.drain(..4 + body_len);
        let fds: Vec<OwnedFd> = self.pending_fds.drain(..).collect();
        Ok(InboxPoll::Frame { json_bytes, fds })
    }
}

/// Blocking convenience helper used by stateless CLI tools (`komactl`).
pub fn sync_roundtrip<Req: Serialize, Resp: DeserializeOwned>(
    stream: &UnixStream,
    req: &Req,
    req_fds: Vec<OwnedFd>,
) -> io::Result<(Resp, Vec<OwnedFd>)> {
    let mut outbox = FrameOutbox::default();
    outbox
        .enqueue(req, req_fds)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    while !outbox.is_empty() {
        outbox.try_flush(stream)?;
    }

    let mut inbox = FrameInbox::default();
    let mut scratch = [0_u8; 16 * 1024];
    loop {
        match inbox
            .next_frame(false)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?
        {
            InboxPoll::Frame { json_bytes, fds } => {
                let parsed: Resp = serde_json::from_slice(&json_bytes)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
                return Ok((parsed, fds));
            }
            InboxPoll::NeedMoreData => {
                let (n, fds) = recv_chunk_with_scm_rights(stream, &mut scratch)?;
                if n == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "compositor closed IPC socket before reply",
                    ));
                }
                inbox.buf.extend_from_slice(&scratch[..n]);
                inbox.pending_fds.extend(fds);
            }
            InboxPoll::PeerClosed => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "compositor closed IPC socket",
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_same_uid_rejects_foreign_uid_fail_closed() {
        let foreign = SocketPeerCreds {
            pid: Some(4242),
            uid: 1001,
            gid: 1001,
        };
        let err = ensure_same_uid(foreign, 1000).expect_err("foreign UID must be rejected");
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);

        let same = SocketPeerCreds {
            pid: None,
            uid: 1000,
            gid: 100,
        };
        assert_eq!(
            ensure_same_uid(same, 1000).unwrap(),
            PeerCreds {
                uid: 1000,
                gid: 100
            }
        );
    }

    #[cfg(unix)]
    fn assert_readonly_fd_transfers_over_scm_rights(fd: OwnedFd, expected_payload: &[u8]) -> File {
        use std::io::Read as _;

        use crate::frame::{IpcRequest, IpcResponse};

        let (server, client) = UnixStream::pair().unwrap();
        server.set_nonblocking(true).unwrap();

        let creds = verify_same_uid_peer(&server).unwrap();
        assert_eq!(creds.uid, rustix::process::getuid().as_raw());

        let resp = IpcResponse::ok(
            1,
            serde_json::json!({ "fd_index": 0, "byte_len": expected_payload.len() }),
        );
        let mut outbox = FrameOutbox::default();
        outbox.enqueue(&resp, vec![fd]).unwrap();
        outbox.try_flush(&server).unwrap();

        // Client receives JSON + read-only FD via SCM_RIGHTS
        client.set_nonblocking(true).unwrap();
        let mut inbox = FrameInbox::default();
        let eof = inbox.read_available(&client).unwrap();
        let InboxPoll::Frame {
            json_bytes,
            mut fds,
        } = inbox.next_frame(eof).unwrap()
        else {
            panic!("expected complete frame");
        };
        let decoded: IpcResponse = serde_json::from_slice(&json_bytes).unwrap();
        assert_eq!(decoded.id, 1);
        assert_eq!(fds.len(), 1);

        let mut received_file = File::from(fds.pop().unwrap());
        let mut read_back = Vec::new();
        received_file.read_to_end(&mut read_back).unwrap();
        assert_eq!(read_back, expected_payload);

        // Verify read-only sealing or O_RDONLY permissions block peer mutation!
        let write_err = received_file.write_all(b"corrupt");
        assert!(
            write_err.is_err(),
            "read-only IPC FD must block peer writes to shared frame memory"
        );
        let _ = IpcRequest {
            id: 1,
            method: "ping".into(),
            params: serde_json::Value::Null,
        };
        received_file
    }

    #[cfg(unix)]
    #[test]
    fn transfers_sealed_readonly_memfd_over_scm_rights() {
        let pixels = b"KOMA_ZERO_COPY_RGBA8_SCANLINE_DATA";
        let memfd = create_sealed_readonly_memfd("ws_01_cap", pixels).unwrap();
        let _received = assert_readonly_fd_transfers_over_scm_rights(memfd, pixels);
    }

    #[cfg(unix)]
    #[test]
    fn shm_readonly_fallback_blocks_writes_and_prot_write_mmap() {
        let pixels = b"KOMA_ZERO_COPY_POSIX_SHM_RGBA8_DATA";
        let shm_fd = create_shm_readonly_fd("ws_01_bsd", pixels).unwrap();
        let mut received_file = assert_readonly_fd_transfers_over_scm_rights(shm_fd, pixels);

        let err = received_file
            .write(b"mutated")
            .expect_err("POSIX SHM O_RDONLY fallback must reject writes");
        assert!(
            err.raw_os_error().is_some(),
            "expected OS error on O_RDONLY write, got: {err:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn writable_shm_fd_preserves_payload_at_offset_zero() {
        use std::io::Read as _;

        let pixels = b"KOMA_OVERVIEW_SHM_PIXELS";
        let fd = create_writable_shm_fd("koma_overview_shm", pixels).unwrap();
        let mut file = File::from(fd);
        let mut read_back = Vec::new();
        file.read_to_end(&mut read_back).unwrap();
        assert_eq!(read_back, pixels);
    }
}
