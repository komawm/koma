//! Non-blocking Unix domain socket framing with `SCM_RIGHTS` FD passing, `SO_PEERCRED` UID
//! verification, and write-sealed `memfd` creation (`IPC_SPEC.md` §2.1, §2.2, §5.4).
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
};

use rustix::{
    cmsg_space,
    io::Errno,
    net::{
        RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
        SendAncillaryMessage, SendFlags, recvmsg, sendmsg,
    },
    process::getuid,
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

/// Verifies that a client connecting to `koma-admin.sock` runs under the exact same UID as `koma`.
#[cfg(target_os = "linux")]
pub fn verify_same_uid_peer(stream: &UnixStream) -> io::Result<PeerCreds> {
    let ucred = rustix::net::sockopt::socket_peercred(stream)
        .map_err(|e| io::Error::from_raw_os_error(e.raw_os_error()))?;
    let host_uid = getuid();
    if ucred.uid != host_uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "koma-admin.sock rejected foreign UID {} (compositor UID is {})",
                ucred.uid.as_raw(),
                host_uid.as_raw()
            ),
        ));
    }
    Ok(PeerCreds {
        uid: ucred.uid.as_raw(),
        gid: ucred.gid.as_raw(),
    })
}

#[cfg(not(target_os = "linux"))]
pub fn verify_same_uid_peer(_stream: &UnixStream) -> io::Result<PeerCreds> {
    Ok(PeerCreds {
        uid: getuid().as_raw(),
        gid: 0,
    })
}

/// Allocates an anonymous RAM-backed `memfd`, writes `payload`, rewinds to offset `0`, and applies
/// `F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_WRITE | F_SEAL_SEAL` so untrusted IPC peers can `mmap`
/// `PROT_READ` zero-copy without ever mutating or truncating the buffer (`IPC_SPEC.md` §5.4).
#[cfg(target_os = "linux")]
pub fn create_sealed_readonly_memfd(label: &str, payload: &[u8]) -> io::Result<OwnedFd> {
    use rustix::fs::{MemfdFlags, SealFlags, fcntl_add_seals, memfd_create};

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
    let cname = CString::new(if clean_name.is_empty() {
        "koma_frame".to_owned()
    } else {
        clean_name
    })
    .expect("sanitized ASCII CString");

    let fd = memfd_create(&cname, MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING)
        .map_err(|e| io::Error::from_raw_os_error(e.raw_os_error()))?;

    let mut file = File::from(fd);
    file.write_all(payload)?;
    file.seek(SeekFrom::Start(0))?;
    let fd = OwnedFd::from(file);

    fcntl_add_seals(
        &fd,
        SealFlags::SHRINK | SealFlags::GROW | SealFlags::WRITE | SealFlags::SEAL,
    )
    .map_err(|e| io::Error::from_raw_os_error(e.raw_os_error()))?;

    Ok(fd)
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

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::io::Read as _;

    use super::*;
    use crate::frame::{IpcRequest, IpcResponse};

    #[test]
    fn transfers_sealed_readonly_memfd_over_scm_rights() {
        let (server, client) = UnixStream::pair().unwrap();
        server.set_nonblocking(true).unwrap();

        let creds = verify_same_uid_peer(&server).unwrap();
        assert_eq!(creds.uid, rustix::process::getuid().as_raw());

        // Create sealed read-only memfd simulating a 1920x1080 RGBA frame
        let pixels = b"KOMA_ZERO_COPY_RGBA8_SCANLINE_DATA";
        let memfd = create_sealed_readonly_memfd("ws_01_cap", pixels).unwrap();

        let resp = IpcResponse::ok(
            1,
            serde_json::json!({ "fd_index": 0, "byte_len": pixels.len() }),
        );
        let mut outbox = FrameOutbox::default();
        outbox.enqueue(&resp, vec![memfd]).unwrap();
        outbox.try_flush(&server).unwrap();

        // Client receives JSON + sealed memfd via SCM_RIGHTS
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
        assert_eq!(read_back, pixels);

        // Verify write-seal blocks mutation by untrusted peer!
        let write_err = received_file.write_all(b"corrupt");
        assert!(
            write_err.is_err(),
            "F_SEAL_WRITE must block peer writes to shared frame memfd"
        );
        let _ = IpcRequest {
            id: 1,
            method: "ping".into(),
            params: serde_json::Value::Null,
        };
    }
}
