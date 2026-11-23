//! Serves a [`Disk`] as a network block device, on a Unix socket.
//!
//! Apple's Virtualization framework takes a disk image in a format of its own, or a network block
//! device: a server that answers its reads and writes. That is the only way a disk can be this
//! program's own. What comes in over the socket is a guest's doing, so every request is checked
//! before it reaches the disk.

use std::io::{self, IoSlice, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};

use crate::Disk;

/// The largest read or write that is answered. The framework's client asks for 8 MiB at most.
const LARGEST_REQUEST: u32 = 32 << 20;
/// Requests of one connection that are answered at the same time.
const WORKERS: usize = 8;
/// A path of a Unix socket has room for this many bytes on macOS, less than on Linux.
const LONGEST_PATH: usize = 103;

const OPTION_MAGIC: u64 = 0x4948_4156_454F_5054;
const OPTION_REPLY_MAGIC: u64 = 0x0003_E889_0455_65A9;
const REQUEST_MAGIC: u32 = 0x2560_9513;
const REPLY_MAGIC: u32 = 0x6744_6698;

const EPERM: u32 = 1;
const EIO: u32 = 5;
const EINVAL: u32 = 22;
const ENOSPC: u32 = 28;

/// A disk that a machine can connect to, until this is dropped or [`Server::stop`] is called.
pub struct Server {
    path: PathBuf,
    shared: Arc<Shared>,
    acceptor: Option<JoinHandle<()>>,
}

struct Shared {
    disk: Arc<dyn Disk>,
    stopped: AtomicBool,
    connections: Mutex<Vec<(UnixStream, JoinHandle<()>)>>,
}

impl Server {
    /// Listens on a new socket at `path`, which only this user can open.
    pub fn start(disk: Arc<dyn Disk>, path: &Path) -> io::Result<Self> {
        if path.as_os_str().len() > LONGEST_PATH {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the path of the disk's socket is too long",
            ));
        }
        _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        let shared = Arc::new(Shared {
            disk,
            stopped: AtomicBool::new(false),
            connections: Mutex::new(Vec::new()),
        });
        let acceptor = {
            let shared = shared.clone();
            thread::Builder::new()
                .name("nbd-accept".into())
                .spawn(move || accept(listener, shared))?
        };
        Ok(Self {
            path: path.to_owned(),
            shared,
            acceptor: Some(acceptor),
        })
    }

    /// What an engine is given to connect to: `nbd+unix:///disk?socket=<path>`.
    pub fn url(&self) -> String {
        let mut url = String::from("nbd+unix:///disk?socket=");
        for &byte in self.path.as_os_str().as_encoded_bytes() {
            match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                    url.push(byte as char)
                }
                _ => url.push_str(&format!("%{byte:02X}")),
            }
        }
        url
    }

    /// Stops listening and hangs up. Returns once no request is being answered any more, so that
    /// the disk can be closed.
    pub fn stop(&mut self) {
        if self.shared.stopped.swap(true, Ordering::SeqCst) {
            return;
        }
        // The thread that accepts waits for someone to connect. This is that someone.
        _ = UnixStream::connect(&self.path);
        if let Some(acceptor) = self.acceptor.take() {
            _ = acceptor.join();
        }
        let connections = std::mem::take(&mut *self.shared.connections.lock().unwrap());
        for (stream, _) in &connections {
            _ = stream.shutdown(std::net::Shutdown::Both);
        }
        for (_, thread) in connections {
            _ = thread.join();
        }
        _ = std::fs::remove_file(&self.path);
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
    }
}

fn accept(listener: UnixListener, shared: Arc<Shared>) {
    for stream in listener.incoming() {
        if shared.stopped.load(Ordering::SeqCst) {
            return;
        }
        let Ok(stream) = stream else {
            // Out of descriptors, for one: that passes, and the machine tries again by itself.
            thread::sleep(std::time::Duration::from_millis(100));
            continue;
        };
        // The system's few kilobytes would make every megabyte a hundred trips.
        for option in [libc::SO_SNDBUF, libc::SO_RCVBUF] {
            let size: libc::c_int = 4 << 20;
            // Safety: the descriptor is open, and the option takes an int.
            unsafe {
                libc::setsockopt(
                    stream.as_raw_fd(),
                    libc::SOL_SOCKET,
                    option,
                    (&raw const size).cast(),
                    size_of::<libc::c_int>() as libc::socklen_t,
                );
            }
        }
        let Ok(kept) = stream.try_clone() else { continue };
        let disk = shared.disk.clone();
        let mut connections = shared.connections.lock().unwrap();
        connections.retain(|(_, thread)| !thread.is_finished());
        if let Ok(thread) = thread::Builder::new()
            .name("nbd-serve".into())
            .spawn(move || _ = serve(stream, disk))
        {
            connections.push((kept, thread));
        }
    }
}

fn transmission_flags(disk: &dyn Disk) -> u16 {
    // Has flags, takes flushes, trims and requests to write zeros.
    1 << 0 | 1 << 2 | 1 << 5 | 1 << 6 | if disk.is_read_only() { 1 << 1 } else { 0 }
}

fn receive<const N: usize>(stream: &mut UnixStream) -> io::Result<[u8; N]> {
    let mut bytes = [0; N];
    stream.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_be_bytes(bytes[at..at + 2].try_into().unwrap())
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap())
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_be_bytes(bytes[at..at + 8].try_into().unwrap())
}

/// Agrees with the client on what is served. `false` when it asked for nothing.
fn handshake(stream: &mut UnixStream, disk: &dyn Disk) -> io::Result<bool> {
    // "NBDMAGIC", "IHAVEOPT", and that this server speaks the newer handshake without its padding.
    let mut greeting = Vec::with_capacity(18);
    greeting.extend(0x4E42_444D_4147_4943u64.to_be_bytes());
    greeting.extend(OPTION_MAGIC.to_be_bytes());
    greeting.extend(3u16.to_be_bytes());
    stream.write_all(&greeting)?;
    let flags = receive::<4>(stream)?;
    let skips_padding = flags[3] & 2 != 0;

    const ACKNOWLEDGED: u32 = 1;
    const INFORMATION: u32 = 3;
    const UNSUPPORTED: u32 = 0x8000_0001;
    let reply = |stream: &mut UnixStream, option: u32, kind: u32, data: &[u8]| {
        let mut answer = Vec::with_capacity(20 + data.len());
        answer.extend(OPTION_REPLY_MAGIC.to_be_bytes());
        answer.extend(option.to_be_bytes());
        answer.extend(kind.to_be_bytes());
        answer.extend((data.len() as u32).to_be_bytes());
        answer.extend(data);
        stream.write_all(&answer)
    };

    loop {
        let head = receive::<16>(stream)?;
        if u64_at(&head, 0) != OPTION_MAGIC {
            return Ok(false);
        }
        let option = u32_at(&head, 8);
        let length = u32_at(&head, 12) as usize;
        if length > 4096 {
            return Ok(false);
        }
        let mut data = vec![0; length];
        stream.read_exact(&mut data)?;

        match option {
            // The older way to ask for the disk: answered with its size alone.
            1 => {
                let mut answer = Vec::with_capacity(134);
                answer.extend(disk.capacity().to_be_bytes());
                answer.extend(transmission_flags(disk).to_be_bytes());
                if !skips_padding {
                    answer.extend([0; 124]);
                }
                stream.write_all(&answer)?;
                return Ok(true);
            }
            // Never mind.
            2 => {
                reply(stream, option, ACKNOWLEDGED, &[])?;
                return Ok(false);
            }
            // What is the disk like, and: let's go.
            6 | 7 => {
                // The name that is asked for, then what the client wants to know about it.
                let mut wants_block_size = false;
                if data.len() >= 6 {
                    let mut cursor = 4 + u32_at(&data, 0) as usize;
                    if cursor + 2 <= data.len() {
                        let count = u16_at(&data, cursor);
                        cursor += 2;
                        for _ in 0..count {
                            if cursor + 2 > data.len() {
                                break;
                            }
                            wants_block_size |= u16_at(&data, cursor) == 3;
                            cursor += 2;
                        }
                    }
                }
                let mut export = Vec::with_capacity(12);
                export.extend(0u16.to_be_bytes());
                export.extend(disk.capacity().to_be_bytes());
                export.extend(transmission_flags(disk).to_be_bytes());
                reply(stream, option, INFORMATION, &export)?;
                if wants_block_size {
                    let mut sizes = Vec::with_capacity(14);
                    sizes.extend(3u16.to_be_bytes());
                    sizes.extend(512u32.to_be_bytes());
                    sizes.extend(4096u32.to_be_bytes());
                    sizes.extend(LARGEST_REQUEST.to_be_bytes());
                    reply(stream, option, INFORMATION, &sizes)?;
                }
                reply(stream, option, ACKNOWLEDGED, &[])?;
                if option == 7 {
                    return Ok(true);
                }
            }
            _ => reply(stream, option, UNSUPPORTED, &[])?,
        }
    }
}

enum Work {
    Read,
    Write(Vec<u8>),
    Flush,
    Trim,
    Zero,
}

struct Request {
    cookie: u64,
    offset: u64,
    length: u32,
    work: Work,
}

fn code(error: &io::Error) -> u32 {
    match error.raw_os_error() {
        Some(libc::ENOSPC) => ENOSPC,
        Some(libc::EPERM | libc::EROFS) => EPERM,
        Some(libc::EINVAL) => EINVAL,
        _ => EIO,
    }
}

fn answer(replies: &Mutex<UnixStream>, cookie: u64, error: u32, payload: &[u8]) {
    let mut head = [0; 16];
    head[..4].copy_from_slice(&REPLY_MAGIC.to_be_bytes());
    head[4..8].copy_from_slice(&error.to_be_bytes());
    head[8..].copy_from_slice(&cookie.to_be_bytes());
    let payload = if error == 0 { payload } else { &[] };
    let mut stream = replies.lock().unwrap();
    // One call for both where the system takes them whole. A client that hung up is found out by
    // the thread that reads from it.
    let mut parts = [IoSlice::new(&head), IoSlice::new(payload)];
    let mut left = &mut parts[..];
    while !left.is_empty() {
        match stream.write_vectored(left) {
            Ok(0) => return,
            Ok(written) => IoSlice::advance_slices(&mut left, written),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

fn perform(disk: &dyn Disk, request: Request, replies: &Mutex<UnixStream>, buffer: &mut Vec<u8>) {
    let Request {
        cookie,
        offset,
        length,
        work,
    } = request;
    let end = offset.checked_add(length as u64);
    if end.is_none_or(|end| end > disk.capacity()) {
        return answer(replies, cookie, EINVAL, &[]);
    }
    let changes = !matches!(work, Work::Read | Work::Flush);
    if changes && disk.is_read_only() {
        return answer(replies, cookie, EPERM, &[]);
    }
    let result = match work {
        Work::Read => {
            buffer.resize(length as usize, 0);
            match disk.read(offset, buffer) {
                Ok(()) => return answer(replies, cookie, 0, buffer),
                Err(error) => Err(error),
            }
        }
        Work::Write(data) => disk.write(offset, &data),
        // A disk that is only read has nothing waiting to be written.
        Work::Flush if disk.is_read_only() => Ok(()),
        Work::Flush => disk.flush(),
        Work::Trim => disk.discard(offset, length as u64),
        Work::Zero => disk.zero(offset, length as u64),
    };
    answer(replies, cookie, result.as_ref().map_or_else(code, |()| 0), &[]);
}

fn serve(mut stream: UnixStream, disk: Arc<dyn Disk>) -> io::Result<()> {
    if !handshake(&mut stream, &*disk)? {
        return Ok(());
    }
    let replies = Arc::new(Mutex::new(stream.try_clone()?));
    // Bounded, so that a client that asks faster than the disk answers waits its turn.
    let (sender, receiver) = mpsc::sync_channel::<Request>(64);
    let receiver = Arc::new(Mutex::new(receiver));
    let workers: Vec<_> = (0..WORKERS)
        .map(|_| {
            let (disk, replies, receiver) = (disk.clone(), replies.clone(), receiver.clone());
            thread::spawn(move || {
                let mut buffer = Vec::new();
                loop {
                    let next = receiver.lock().unwrap().recv();
                    let Ok(request) = next else { return };
                    perform(&*disk, request, &replies, &mut buffer);
                }
            })
        })
        .collect();

    let result = (|| -> io::Result<()> {
        loop {
            let head = receive::<28>(&mut stream)?;
            if u32_at(&head, 0) != REQUEST_MAGIC {
                return Ok(());
            }
            let (command, cookie, offset, length) = (
                u16_at(&head, 6),
                u64_at(&head, 8),
                u64_at(&head, 16),
                u32_at(&head, 24),
            );
            let work = match command {
                0 if length <= LARGEST_REQUEST => Work::Read,
                1 => {
                    // What follows on the line is the data, which can't be told from the next
                    // request without reading it: a write that is too large ends the conversation.
                    if length > LARGEST_REQUEST {
                        return Ok(());
                    }
                    let mut data = vec![0; length as usize];
                    stream.read_exact(&mut data)?;
                    Work::Write(data)
                }
                2 => return Ok(()),
                3 => Work::Flush,
                4 => Work::Trim,
                6 => Work::Zero,
                _ => {
                    answer(&replies, cookie, EINVAL, &[]);
                    continue;
                }
            };
            if sender
                .send(Request {
                    cookie,
                    offset,
                    length,
                    work,
                })
                .is_err()
            {
                return Ok(());
            }
        }
    })();
    // The connection is closed only after the requests that are being answered on it.
    drop(sender);
    for worker in workers {
        _ = worker.join();
    }
    match result {
        Err(error) if error.kind() != io::ErrorKind::UnexpectedEof => Err(error),
        _ => Ok(()),
    }
}
