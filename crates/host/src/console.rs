//! A machine's serial console: a line to the guest that carries text both ways.
//!
//! What the guest prints goes to a log file, to a short transcript for whoever starts listening
//! later, and to everyone who is listening now. What any of them types goes to the guest.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{PipeReader, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::engine::ConsoleEnd;
use crate::{Context, Result};

/// How much of what the guest printed is kept for someone who starts listening later.
const TRANSCRIPT_LIMIT: usize = 256 << 10;
/// The log starts over when it reaches this size. The one before it is kept next to it.
const LOG_LIMIT: u64 = 4 << 20;
/// A listener that is this far behind what the guest printed is dropped. It has room for the
/// transcript, which every listener starts with.
const LISTENER_BACKLOG: usize = 4 * TRANSCRIPT_LIMIT;
/// A listener that takes nothing for this long is given up on.
const LISTENER_PATIENCE: Duration = Duration::from_secs(5);

pub(crate) struct Console {
    inner: Mutex<Inner>,
    /// What is typed goes to the guest through this. It never waits.
    to_guest: Mutex<File>,
    closing: AtomicBool,
    reader: Mutex<Option<JoinHandle<()>>>,
}

/// Someone who is listening. What the guest prints is put in a queue that a thread of the
/// listener's own writes out, so that one who reads slowly holds up neither the guest nor the
/// others.
struct Listener {
    id: u64,
    /// To hang up with.
    stream: UnixStream,
    queue: mpsc::Sender<Vec<u8>>,
    /// Bytes in the queue.
    waiting: Arc<AtomicUsize>,
}

impl Listener {
    /// Queues `bytes`. `false` for a listener that is too far behind, or gone.
    fn send(&self, bytes: &[u8]) -> bool {
        if bytes.is_empty() {
            return true;
        }
        let waiting = self.waiting.fetch_add(bytes.len(), Ordering::SeqCst) + bytes.len();
        waiting <= LISTENER_BACKLOG && self.queue.send(bytes.to_vec()).is_ok()
    }
}

struct Inner {
    transcript: VecDeque<u8>,
    listeners: Vec<Listener>,
    next_listener: u64,
    log: Option<File>,
    log_path: PathBuf,
    logged: u64,
}

fn previous(log: &Path) -> PathBuf {
    let mut name = log.as_os_str().to_owned();
    name.push(".1");
    name.into()
}

impl Inner {
    /// Starts the log over, keeping the one that was there.
    fn open_log(&mut self) {
        _ = std::fs::rename(&self.log_path, previous(&self.log_path));
        self.log = File::create(&self.log_path).ok();
        self.logged = 0;
    }

    fn take(&mut self, bytes: &[u8]) {
        if self.logged >= LOG_LIMIT {
            self.open_log();
        }
        // A log that can't be written doesn't keep the console from working.
        if let Some(log) = &mut self.log {
            _ = log.write_all(bytes);
            self.logged += bytes.len() as u64;
        }
        self.transcript.extend(bytes);
        let excess = self.transcript.len().saturating_sub(TRANSCRIPT_LIMIT);
        self.transcript.drain(..excess);
        self.listeners.retain(|listener| {
            let listening = listener.send(bytes);
            if !listening {
                // Wakes the threads that write to this listener and wait for what it types.
                _ = listener.stream.shutdown(std::net::Shutdown::Both);
            }
            listening
        });
    }
}

impl Console {
    /// Opens a console that logs to `log`, and returns it with the end that the engine gives to
    /// the guest.
    pub fn open(log: &Path) -> Result<(Arc<Self>, ConsoleEnd)> {
        let (input, to_guest) = std::io::pipe().context("create a pipe")?;
        let (from_guest, output) = std::io::pipe().context("create a pipe")?;
        let to_guest = File::from(OwnedFd::from(to_guest));
        // Typing to a guest that isn't reading must not hold anyone up.
        // Safety: the descriptor is open, and these calls only change its flags.
        unsafe {
            let flags = libc::fcntl(to_guest.as_raw_fd(), libc::F_GETFL);
            libc::fcntl(to_guest.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
        let mut inner = Inner {
            transcript: VecDeque::new(),
            listeners: Vec::new(),
            next_listener: 0,
            log: None,
            log_path: log.to_owned(),
            logged: 0,
        };
        inner.open_log();
        let console = Arc::new(Self {
            inner: Mutex::new(inner),
            to_guest: Mutex::new(to_guest),
            closing: AtomicBool::new(false),
            reader: Mutex::new(None),
        });
        let reader = {
            let console = console.clone();
            std::thread::Builder::new()
                .name("console".into())
                .spawn(move || console.read(from_guest))
                .context("start a thread")?
        };
        *console.reader.lock().unwrap() = Some(reader);
        Ok((
            console,
            ConsoleEnd {
                input: input.into(),
                output: output.into(),
            },
        ))
    }

    /// Passes on what the guest prints until the console is closed.
    fn read(&self, mut from_guest: PipeReader) {
        let mut buffer = vec![0; 64 << 10];
        loop {
            // The engine keeps its end open for as long as it likes, so the end of the guest's
            // output can't be waited for: this looks up now and then to see whether it is over.
            let mut waiting = libc::pollfd {
                fd: from_guest.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // Safety: one valid pollfd.
            let ready = unsafe { libc::poll(&mut waiting, 1, 200) };
            if ready <= 0 {
                // Only once the guest has nothing more to say: its final lines are the ones that
                // tell why it stopped.
                if self.closing.load(Ordering::SeqCst) {
                    return;
                }
                continue;
            }
            match from_guest.read(&mut buffer) {
                Ok(0) => return,
                Ok(count) => self.inner.lock().unwrap().take(&buffer[..count]),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => return,
            }
        }
    }

    /// Types `bytes` to the guest. What a guest that isn't reading has no room for is dropped.
    fn type_in(&self, mut bytes: &[u8]) {
        let mut to_guest = self.to_guest.lock().unwrap();
        while !bytes.is_empty() {
            match to_guest.write(bytes) {
                Ok(count) if count > 0 => bytes = &bytes[count..],
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                _ => return,
            }
        }
    }

    /// Connects `stream` to the console: it is sent the transcript and then what the guest
    /// prints, and what arrives on it is typed to the guest. Returns when the other side leaves
    /// or the console is closed.
    pub fn attach(&self, mut stream: UnixStream) {
        let (Ok(mut output), Ok(kept)) = (stream.try_clone(), stream.try_clone()) else {
            return;
        };
        _ = output.set_write_timeout(Some(LISTENER_PATIENCE));
        let (queue, queued) = mpsc::channel::<Vec<u8>>();
        let waiting = Arc::new(AtomicUsize::new(0));
        let id = {
            // With the lock held, so that nothing the guest prints is missed or sent twice.
            // Nothing is written to the listener here: the lock is the guest's console's too.
            let mut inner = self.inner.lock().unwrap();
            inner.next_listener += 1;
            let listener = Listener {
                id: inner.next_listener,
                stream: kept,
                queue,
                waiting: waiting.clone(),
            };
            let (older, newer) = inner.transcript.as_slices();
            listener.send(older);
            listener.send(newer);
            inner.listeners.push(listener);
            inner.next_listener
        };
        let writer = std::thread::spawn(move || {
            // Until the queue's other end goes: the listener left, fell behind, or the console
            // was closed. What is in the queue by then is still written out.
            for bytes in queued {
                if output.write_all(&bytes).is_err() {
                    break;
                }
                waiting.fetch_sub(bytes.len(), Ordering::SeqCst);
            }
            // Wakes the thread below, which waits for what the listener types.
            _ = output.shutdown(std::net::Shutdown::Both);
        });
        let mut buffer = [0; 4096];
        loop {
            match stream.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => self.type_in(&buffer[..count]),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        self.inner
            .lock()
            .unwrap()
            .listeners
            .retain(|listener| listener.id != id);
        _ = writer.join();
    }

    /// Ends the console of a machine that has stopped: takes in what the guest printed last, and
    /// lets go of everyone who is listening. Each is hung up on once it has been sent the rest.
    pub fn close(&self) {
        self.closing.store(true, Ordering::SeqCst);
        if let Some(reader) = self.reader.lock().unwrap().take() {
            _ = reader.join();
        }
        let mut inner = self.inner.lock().unwrap();
        inner.listeners.clear();
        if let Some(log) = &inner.log {
            _ = log.sync_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_listener_that_reads_nothing_holds_nobody_up() {
        let folder = std::env::temp_dir().join(format!("console-test-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        let log = folder.join("console.log");
        let (console, guest) = Console::open(&log).unwrap();
        // Kept open and never read from.
        let (_stalled, theirs) = UnixStream::pair().unwrap();
        let attached = {
            let console = console.clone();
            std::thread::spawn(move || console.attach(theirs))
        };
        while console.inner.lock().unwrap().listeners.is_empty() {
            std::thread::sleep(Duration::from_millis(5));
        }

        // More than the listener may fall behind by, printed by a guest that waits for nobody.
        let printed = 3 * LISTENER_BACKLOG;
        let (done, finished) = mpsc::channel();
        std::thread::spawn(move || {
            let mut output = File::from(guest.output);
            _ = done.send(output.write_all(&vec![b'x'; printed]));
        });
        finished
            .recv_timeout(Duration::from_secs(20))
            .expect("the guest's console is held up")
            .unwrap();
        console.close();
        assert_eq!(std::fs::metadata(&log).unwrap().len(), printed as u64);
        // The listener was dropped, which ends its connection.
        attached.join().unwrap();

        std::fs::remove_file(&log).unwrap();
        std::fs::remove_dir(&folder).unwrap();
    }
}
