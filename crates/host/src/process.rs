//! The process of a running machine: the same program as the command, started in a mode of its
//! own and left to itself.
//!
//! It holds the machine's lock for as long as it lives, which is how everyone else tells that
//! the machine is running. It runs the engine, keeps the console, and answers on a socket whose
//! path it writes to `process.json`.

use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use proto::{Request, Response, State, Status};
use serde::{Deserialize, Serialize};

use crate::console::Console;
use crate::engine::{self, DiskAttachment, Engine, Spec};
use crate::lock::Lock;
use crate::paths::socket_path;
use crate::store::{Config, DiskFormat, LOCK, Store};
use crate::{Context, Error, ErrorCode, Result, layers, now};

/// The first argument that makes the program a machine's process. The second is the machine's
/// folder. A program that embeds this library passes both on to [`run`].
pub const ARGUMENT: &str = "machine-process";

/// How long a guest gets to shut down before its power is cut, unless the request says.
pub(crate) const STOP_TIMEOUT: Duration = Duration::from_secs(30);
/// The same, when the process itself is told to end: the system doesn't wait long for it.
const TERMINATION_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) const PROCESS_FILE: &str = "process.json";
pub(crate) const SEED_FILE: &str = "seed.iso";

/// What a machine's process writes down about itself: `process.json`.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ProcessFile {
    pub pid: u32,
    /// Where it listens.
    pub socket: PathBuf,
    /// Seconds since 1970.
    pub started: u64,
}

/// What a connection carries once a request has been answered, other than more requests.
enum Then {
    Console,
    /// A connection to a port in the guest, a stream socket.
    Relay(UnixStream),
}

struct Phase {
    state: State,
    /// Why the machine stopped, if something went wrong.
    failure: Option<String>,
}

struct Service {
    engine: Box<dyn Engine>,
    console: Arc<Console>,
    shared: Arc<Shared>,
    /// Requests that haven't been answered yet.
    answering: AtomicUsize,
}

struct Shared {
    phase: Mutex<Phase>,
    changed: Condvar,
}

impl Shared {
    fn state(&self) -> State {
        self.phase.lock().unwrap().state
    }

    /// Moves on to `state`. A machine that has stopped stays stopped.
    fn set(&self, state: State) {
        let mut phase = self.phase.lock().unwrap();
        if phase.state != State::Stopped {
            phase.state = state;
            self.changed.notify_all();
        }
    }

    fn stopped(&self, failure: Option<String>) {
        let mut phase = self.phase.lock().unwrap();
        if phase.state != State::Stopped {
            phase.state = State::Stopped;
            phase.failure = failure;
            self.changed.notify_all();
        }
    }

    /// Waits until the machine has stopped, for `limit` at most. Says whether it has.
    fn wait_until_stopped(&self, limit: Duration) -> bool {
        let phase = self.phase.lock().unwrap();
        let (phase, _) = self
            .changed
            .wait_timeout_while(phase, limit, |phase| phase.state != State::Stopped)
            .unwrap();
        phase.state == State::Stopped
    }
}

impl Service {
    fn status(&self) -> Status {
        Status {
            state: self.shared.state(),
            pid: std::process::id(),
        }
    }

    /// Turns the machine off: asks the guest to shut down, and cuts the power if it hasn't after
    /// `timeout`, or at once with `force`.
    fn stop(&self, force: bool, timeout: Duration) -> Result<()> {
        self.shared.set(State::Stopping);
        let asked = !force && self.engine.request_stop().is_ok();
        if asked && self.shared.wait_until_stopped(timeout) || self.shared.state() == State::Stopped {
            return Ok(());
        }
        match self.engine.stop() {
            Ok(()) => {
                self.shared.stopped(None);
                Ok(())
            }
            Err(error) => {
                self.shared.set(State::Running);
                Err(error)
            }
        }
    }

    /// Answers the requests of one connection.
    fn answer(&self, mut stream: UnixStream) {
        if proto::welcome(&mut stream, env!("CARGO_PKG_VERSION")).is_err() {
            return;
        }
        loop {
            let request = match proto::receive::<Request>(&mut stream) {
                Ok(Some(request)) => request,
                Ok(None) => return,
                // A request of a newer version, most likely: the next one may be known.
                Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
                    let unknown = Error::new(
                        ErrorCode::Protocol,
                        "The machine's process doesn't know this request.",
                    );
                    if proto::send(&mut stream, &Response::failed(unknown)).is_err() {
                        return;
                    }
                    continue;
                }
                Err(_) => return,
            };
            self.answering.fetch_add(1, Ordering::SeqCst);
            let stopped = || Response::failed(Error::new(ErrorCode::NotRunning, "The machine has stopped."));
            let mut then = None;
            let response = match request {
                Request::Status => Response::done(Some(self.status())),
                Request::Stop { force, timeout } => {
                    let timeout = timeout.map_or(STOP_TIMEOUT, Duration::from_secs);
                    match self.stop(force, timeout) {
                        Ok(()) => Response::done(Some(self.status())),
                        Err(error) => Response::failed(error),
                    }
                }
                Request::Console if self.shared.state() == State::Stopped => stopped(),
                Request::Connect { .. } if self.shared.state() != State::Running => stopped(),
                Request::Console => {
                    then = Some(Then::Console);
                    Response::done(None)
                }
                Request::Connect { port } => match self.engine.connect(port) {
                    Ok(guest) => {
                        then = Some(Then::Relay(UnixStream::from(guest)));
                        Response::done(None)
                    }
                    Err(error) => Response::failed(error),
                },
            };
            let sent = proto::send(&mut stream, &response);
            self.answering.fetch_sub(1, Ordering::SeqCst);
            if sent.is_err() {
                return;
            }
            match then {
                // From here on the connection carries the console, or the guest's connection.
                Some(Then::Console) => return self.console.attach(stream),
                Some(Then::Relay(guest)) => return proto::relay::relay(stream, guest),
                None => {}
            }
        }
    }
}

/// A machine's disk of layers, served to its engine for as long as this lives.
struct ServedDisk {
    server: disk::nbd::Server,
    layer: Arc<disk::Layer>,
}

impl ServedDisk {
    fn start(directory: &Path, config: &Config) -> Result<Self> {
        // The machine's folder is <store>/machines/<name>.
        let store = directory
            .parent()
            .and_then(Path::parent)
            .map(Store::new)
            .ok_or_else(|| {
                Error::new(
                    ErrorCode::InvalidArgument,
                    "The machine's folder isn't in a store.",
                )
            })?;
        let path = directory.join(&config.disk.file);
        let layer = layers::open(&store, &path, &disk::Options::default())?;
        let socket = socket_path(directory, &config.id, "disk")?;
        let server = disk::nbd::Server::start(layer.clone(), &socket).at("serve the disk on", &socket)?;
        Ok(Self { server, layer })
    }
}

impl Drop for ServedDisk {
    fn drop(&mut self) {
        self.server.stop();
        // What can't be committed now is in the layer's journal, and comes back the next time.
        if let Err(error) = self.layer.close() {
            _ = std::io::stderr().write_all(format!("Couldn't close the disk: {error}.\n").as_bytes());
        }
    }
}

static TERMINATED: AtomicBool = AtomicBool::new(false);

extern "C" fn terminated(_signal: libc::c_int) {
    TERMINATED.store(true, Ordering::SeqCst);
}

/// Gives whoever started this process its answer, on the standard output, and lets go of it.
fn tell(response: &Response) {
    let mut output = std::io::stdout().lock();
    // Nobody may be there any more: the machine starts all the same.
    _ = proto::send(&mut output, response);
    drop(output);
    if let Ok(nowhere) = std::fs::File::options().write(true).open("/dev/null") {
        // Safety: both descriptors are open.
        unsafe { libc::dup2(nowhere.as_raw_fd(), libc::STDOUT_FILENO) };
    }
}

/// Runs the machine in `directory` until it stops. Whoever started this process reads from its
/// standard output one line: a [`Response`] that says whether the machine has started.
pub fn run(directory: &Path) -> ExitCode {
    let mut told = false;
    match serve(directory, &mut told) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // In one piece: several processes may be writing to the machine's log.
            _ = std::io::stderr().write_all(format!("{error}\n").as_bytes());
            if !told {
                tell(&Response::failed(error));
            }
            ExitCode::FAILURE
        }
    }
}

fn serve(directory: &Path, told: &mut bool) -> Result<()> {
    let lock = directory.join(LOCK);
    // Someone who only looks whether the machine is running may hold the lock for a moment.
    let Some(_running) = Lock::take_within(&lock, Duration::from_secs(2)).at("lock", &lock)? else {
        return Err(Error::new(ErrorCode::Running, "The machine is running already."));
    };
    // A process that was killed left its note behind. Nobody should take it for this one's.
    let process_file = directory.join(PROCESS_FILE);
    _ = std::fs::remove_file(&process_file);
    let config = Config::load(directory)?;
    let shared = Arc::new(Shared {
        phase: Mutex::new(Phase {
            state: State::Starting,
            failure: None,
        }),
        changed: Condvar::new(),
    });
    let (console, guest_end) = Console::open(&directory.join("console.log"))?;
    let seed = directory.join(SEED_FILE);
    // Closed when this returns, after the machine has stopped and before its lock is let go of.
    let served = match config.disk.format {
        DiskFormat::Raw => None,
        DiskFormat::Layer => Some(ServedDisk::start(directory, &config)?),
    };
    let disk = match &served {
        Some(served) => DiskAttachment::Network(served.server.url()),
        None => DiskAttachment::File(directory.join(&config.disk.file)),
    };
    let spec = Spec {
        cpus: config.cpus,
        memory: config.memory,
        disk,
        seed: seed.exists().then_some(seed),
        mac_address: config.mac_address.clone(),
        directory: directory.to_owned(),
        console: guest_end,
    };
    let engine = engine::new(spec, {
        let shared = shared.clone();
        Box::new(move |failure| shared.stopped(failure))
    })?;

    let socket = socket_path(directory, &config.id, "control")?;
    _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket).at("listen on", &socket)?;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).at("protect", &socket)?;
    let forget = || {
        _ = std::fs::remove_file(&socket);
        _ = std::fs::remove_file(&process_file);
    };
    let about = ProcessFile {
        pid: std::process::id(),
        socket: socket.clone(),
        started: now(),
    };
    std::fs::write(&process_file, serde_json::to_vec(&about).expect("plain data"))
        .at("write", &process_file)?;

    if let Err(error) = engine.start() {
        forget();
        return Err(error);
    }
    shared.set(State::Running);
    let service = Arc::new(Service {
        engine,
        console,
        shared,
        answering: AtomicUsize::new(0),
    });
    tell(&Response::done(Some(service.status())));
    *told = true;

    {
        let service = service.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else {
                    // Out of descriptors, for one: that passes.
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                };
                let service = service.clone();
                std::thread::spawn(move || service.answer(stream));
            }
        });
    }

    // Told to end, the process shuts its guest down first, as it does when asked to stop.
    // Safety: the handler only stores to an atomic.
    unsafe {
        libc::signal(
            libc::SIGTERM,
            terminated as extern "C" fn(libc::c_int) as libc::sighandler_t,
        )
    };
    let mut ending = false;
    while !service.shared.wait_until_stopped(Duration::from_millis(250)) {
        if TERMINATED.load(Ordering::SeqCst) && !ending {
            ending = true;
            let service = service.clone();
            std::thread::spawn(move || _ = service.stop(false, TERMINATION_TIMEOUT));
        }
    }

    service.console.close();
    // Whoever asked for the stop is still owed an answer.
    let deadline = Instant::now() + Duration::from_secs(2);
    while service.answering.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(served);
    forget();
    match service.shared.phase.lock().unwrap().failure.take() {
        Some(failure) => Err(Error::new(
            ErrorCode::Engine,
            format!("The machine stopped: {failure}"),
        )),
        None => Ok(()),
    }
}
