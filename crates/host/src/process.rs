//! The process of a running machine: the same program as the command, started in a mode of its
//! own and left to itself.
//!
//! It holds the machine's lock for as long as it lives, which is how everyone else tells that
//! the machine is running. It runs the engine, keeps the console, and answers on a socket whose
//! path it writes to `process.json`. A second socket there passes VNC connections on to the
//! guest's screen.

use std::io::Write as _;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use proto::agent::{Job, SetupState};
use proto::{Request, Response, State, Status};
use serde::{Deserialize, Serialize};

use crate::console::Console;
use crate::engine::{self, DiskAttachment, Engine, Spec};
use crate::lock::Lock;
use crate::paths::socket_path;
use crate::saved::{self, ImageKind, SavedImage};
use crate::screen::{self, Screen};
use crate::store::{Automatic, Config, DiskFormat, LOCK, Share, Store};
use crate::{Context, Error, ErrorCode, Result, guest, layers, now, shares};

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
    /// Where it passes VNC connections on to the guest's screen. Not there for a process
    /// started by a build from before there were screens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vnc: Option<PathBuf>,
    /// Seconds since 1970.
    pub started: u64,
}

/// What a connection carries once a request has been answered, other than more requests.
enum Then {
    Console,
    /// A connection to a port in the guest, a stream socket.
    Relay(UnixStream),
    /// Bytes that follow the answer: a screenshot.
    Bytes(Vec<u8>),
}

struct Phase {
    state: State,
    /// Why the machine stopped, if something went wrong.
    failure: Option<String>,
}

struct Service {
    engine: Box<dyn Engine>,
    /// The machine's folder.
    directory: PathBuf,
    /// The folders that the engine shares. Held while they change, and while the guest is told.
    shares: Mutex<Vec<Share>>,
    /// The guest hasn't been told which shared folders to mount since they last changed.
    unmounted: AtomicBool,
    /// The guest's mount of the shared folders is from before they last changed.
    stale: AtomicBool,
    console: Arc<Console>,
    screen: Screen,
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

    /// Shares the folders that the machine's settings list now, whose folders are there, and has
    /// the guest mount them. A guest whose agent doesn't answer yet mounts them when it does.
    fn share(&self) -> Result<()> {
        let mut shared = self.shares.lock().unwrap();
        let wanted = shares::available(&Config::load(&self.directory)?.shares);
        if *shared != wanted {
            // The guest lets go of what goes before its device does.
            let kept: Vec<Share> = shared
                .iter()
                .filter(|share| wanted.contains(share))
                .cloned()
                .collect();
            if kept.len() < shared.len() {
                guest::mount(&self.directory, engine::mounts(&kept, false))?;
            }
            self.engine.share(&wanted)?;
            *shared = wanted;
            self.stale.store(true, Ordering::SeqCst);
        }
        let stale = self.stale.load(Ordering::SeqCst);
        let mounted = guest::mount(&self.directory, engine::mounts(&shared, stale))?;
        // A guest that is starting, or restarting, is told once its agent answers.
        self.unmounted.store(!mounted, Ordering::SeqCst);
        if mounted {
            self.stale.store(false, Ordering::SeqCst);
        }
        Ok(())
    }

    /// Has the guest mount the shared folders whenever it hasn't been told what they are: after
    /// a start, and after a change that its agent didn't answer to. For as long as the machine runs.
    fn keep_mounted(&self) {
        while self.shared.state() == State::Running {
            if self.unmounted.load(Ordering::SeqCst) {
                let shared = self.shares.lock().unwrap();
                let stale = self.stale.load(Ordering::SeqCst);
                match guest::mount(&self.directory, engine::mounts(&shared, stale)) {
                    Ok(true) => {
                        self.unmounted.store(false, Ordering::SeqCst);
                        self.stale.store(false, Ordering::SeqCst);
                    }
                    Ok(false) => {}
                    Err(error) => {
                        self.unmounted.store(false, Ordering::SeqCst);
                        log(&format!("The guest couldn't mount the shared folders: {error}"));
                    }
                }
            }
            std::thread::sleep(Duration::from_secs(1));
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
                Request::Connect { .. } | Request::Screenshot | Request::Input { .. } | Request::Shares
                    if self.shared.state() != State::Running =>
                {
                    stopped()
                }
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
                Request::Screenshot => match self.screen.screenshot() {
                    Ok((size, png)) => {
                        let response = Response {
                            screen: Some(size),
                            screenshot: Some(proto::Screenshot {
                                width: size.width,
                                height: size.height,
                                size: png.len() as u64,
                            }),
                            ..Response::done(None)
                        };
                        then = Some(Then::Bytes(png));
                        response
                    }
                    Err(error) => Response::failed(error),
                },
                Request::Shares => match self.share() {
                    Ok(()) => Response::done(None),
                    Err(error) => Response::failed(error),
                },
                Request::Input { events } => match self.screen.input(&events) {
                    Ok(size) => Response {
                        screen: Some(size),
                        ..Response::done(None)
                    },
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
                Some(Then::Bytes(bytes)) if stream.write_all(&bytes).is_err() => return,
                Some(Then::Bytes(_)) | None => {}
            }
        }
    }
}

/// What serves a disk to the engine: a network block device for Apple's framework, a vhost-user
/// device for cloud-hypervisor.
enum DiskServer {
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    Network(disk::nbd::Server),
    #[cfg(target_os = "linux")]
    VhostUser(disk::vhost::Server),
}

impl DiskServer {
    fn start(disk: Arc<dyn disk::Disk>, socket: &Path, config: &Config) -> std::io::Result<Self> {
        #[cfg(target_os = "linux")]
        return Ok(Self::VhostUser(disk::vhost::Server::start(
            disk,
            socket,
            config.cpus,
        )?));
        #[cfg(not(target_os = "linux"))]
        {
            _ = config;
            Ok(Self::Network(disk::nbd::Server::start(disk, socket)?))
        }
    }

    fn attachment(&self) -> DiskAttachment {
        match self {
            Self::Network(server) => DiskAttachment::Network(server.url()),
            #[cfg(target_os = "linux")]
            Self::VhostUser(server) => DiskAttachment::VhostUser(server.path().to_owned()),
        }
    }

    fn stop(&mut self) {
        match self {
            Self::Network(server) => server.stop(),
            #[cfg(target_os = "linux")]
            Self::VhostUser(server) => server.stop(),
        }
    }
}

/// A machine's disk of layers, served to its engine until it is closed.
struct ServedDisk {
    /// Taken when the disk is closed. Held while the layer is being sealed, which a close waits
    /// for.
    server: Mutex<Option<DiskServer>>,
    disk: Arc<disk::Switchable<disk::Layer>>,
    store: Store,
    /// The machine's own layer.
    path: PathBuf,
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
        layers::recover(&store, &path)?;
        let layer = layers::open(&store, &path, &disk::Options::default())?;
        let disk = Arc::new(disk::Switchable::new(layer));
        let socket = socket_path(directory, &config.id, "disk")?;
        let server = DiskServer::start(disk.clone(), &socket, config).at("serve the disk on", &socket)?;
        Ok(Self {
            server: Mutex::new(Some(server)),
            disk,
            store,
            path,
        })
    }

    /// Seals the layer that the guest writes to, makes it a layer of the store, and has the
    /// guest write to a new one on top of it from then on. Returns the sealed layer's hash.
    /// Called while the disk is held open.
    fn seal(&self) -> Result<[u8; 32]> {
        // Finishes what the seal left, as a start after a crash would, and opens the new layer.
        let reopen = || {
            layers::recover(&self.store, &self.path)
                .and_then(|()| layers::open(&self.store, &self.path, &disk::Options::default()))
                .map_err(std::io::Error::other)
        };
        self.disk
            .switch(|layer| {
                layer.seal()?;
                // Once it is sealed, the guest has nothing to write to until the new one is
                // open: that is tried again before the guest is left without a disk.
                reopen().or_else(|_| {
                    std::thread::sleep(Duration::from_secs(1));
                    reopen()
                })
            })
            .at("seal", &self.path)?;
        // The new layer is built on the one that was sealed.
        self.disk
            .current()
            .header()
            .parent
            .ok_or_else(|| Error::new(ErrorCode::Io, "The new layer isn't built on the sealed one."))
    }

    /// Stops serving the disk and closes it, once a seal that is under way is done.
    fn close(&self) {
        let Some(mut server) = self.server.lock().unwrap().take() else {
            return;
        };
        server.stop();
        // What can't be committed now is in the layer's journal, and comes back the next time.
        if let Err(error) = self.disk.current().close() {
            log(&format!("Couldn't close the disk: {error}."));
        }
    }
}

impl Drop for ServedDisk {
    fn drop(&mut self) {
        self.close();
    }
}

/// Writes a line to the machine's log, in one piece: several processes may be writing to it.
pub(crate) fn log(line: &str) {
    _ = std::io::stderr().write_all(format!("{line}\n").as_bytes());
}

/// Waits for the setup of the first machine of an image and its scripts to finish, and then makes
/// its disk the automatic image that the next one starts from. The guest keeps running: its file
/// system is frozen while its layer is sealed.
fn take_automatic(directory: &Path, served: &ServedDisk, shared: &Shared, automatic: &Automatic) {
    let running = || shared.state() == State::Running;
    loop {
        for _ in 0..20 {
            if !running() {
                return;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        let Ok(info) = guest::info(directory) else {
            continue;
        };
        match info.setup.map(|setup| setup.state) {
            Some(SetupState::Finished) if info.booted => break,
            // A setup that is finished by hand later, on a machine that has been in use, doesn't
            // make an image for others.
            Some(SetupState::Failed) => {
                log("The setup failed. The disk doesn't become an automatic image.");
                return give_up(directory);
            }
            // A guest without a setup has nothing to make an image of.
            None if info.booted => return give_up(directory),
            _ => {}
        }
    }
    match make_automatic(directory, served, automatic) {
        Ok(name) => log(&format!(
            "The setup has finished. The disk is now the automatic image {name}."
        )),
        Err(error) => log(&format!("Couldn't make the automatic image: {error}")),
    }
}

/// The machine's disk is no longer to become an automatic image.
fn give_up(directory: &Path) {
    if let Err(error) = Config::update(directory, |config| {
        config.automatic = None;
        Ok(())
    }) {
        log(&format!("Couldn't write the machine's settings: {error}"));
    }
}

fn make_automatic(directory: &Path, served: &ServedDisk, automatic: &Automatic) -> Result<String> {
    // Held throughout: the disk isn't closed under the seal, nor before the image is noted.
    let open = served.server.lock().unwrap();
    if open.is_none() {
        return Err(Error::new(ErrorCode::NotRunning, "The machine has stopped."));
    }
    let (frozen, _) = guest::start(directory, &Job::Freeze { seconds: 30 })?;
    let sealed = served.seal();
    // Hanging up thaws the guest's file system.
    drop(frozen);
    let hash = sealed?;
    let config = Config::load(directory)?;
    let name = saved::automatic_name(&automatic.key);
    {
        let _changing = served.store.lock()?;
        // Another first machine of the same kind may have got there first: that one stays.
        if saved::find(&served.store, &name)?.is_none() {
            saved::write(
                &served.store,
                &SavedImage {
                    name: name.clone(),
                    kind: ImageKind::Automatic,
                    layer: disk::layer::to_hex(&hash),
                    arch: config.arch.clone(),
                    user: config.user.clone(),
                    disk_size: config.disk.size,
                    from: config.image.clone(),
                    scripts: automatic.scripts.clone(),
                    key: Some(automatic.key.clone()),
                    created: now(),
                },
            )?;
        }
    }
    Config::update(directory, |config| {
        config.automatic = None;
        Ok(())
    })?;
    drop(open);
    Ok(name)
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
            log(&error.to_string());
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
        DiskFormat::Layer => Some(Arc::new(ServedDisk::start(directory, &config)?)),
    };
    let disk = match &served {
        Some(served) => served
            .server
            .lock()
            .unwrap()
            .as_ref()
            .expect("just started")
            .attachment(),
        None => DiskAttachment::File(directory.join(&config.disk.file)),
    };
    // A folder that isn't there is left out, and shared once it is and the shares change.
    let folders = shares::available(&config.shares);
    let spec = Spec {
        id: config.id.clone(),
        nested: config.nested,
        shares: folders.clone(),
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
    let vnc = socket_path(directory, &config.id, "vnc")?;
    let forget = || {
        _ = std::fs::remove_file(&socket);
        _ = std::fs::remove_file(&vnc);
        _ = std::fs::remove_file(&process_file);
    };
    // What is made of these is taken away again if the rest isn't.
    let listen = |path: &Path| {
        _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path).at("listen on", path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).at("protect", path)?;
        Ok::<_, Error>(listener)
    };
    let listeners = listen(&socket).and_then(|listener| Ok((listener, listen(&vnc)?)));
    let about = ProcessFile {
        pid: std::process::id(),
        socket: socket.clone(),
        vnc: Some(vnc.clone()),
        started: now(),
    };
    let written = listeners.and_then(|listeners| {
        std::fs::write(&process_file, serde_json::to_vec(&about).expect("plain data"))
            .at("write", &process_file)?;
        Ok(listeners)
    });
    let (listener, viewers) = match written {
        Ok(listeners) => listeners,
        Err(error) => {
            forget();
            return Err(error);
        }
    };

    if let Err(error) = engine.start() {
        forget();
        return Err(error);
    }
    shared.set(State::Running);
    let service = Arc::new(Service {
        shares: Mutex::new(folders),
        directory: directory.to_owned(),
        engine,
        console,
        screen: Screen::new(directory),
        shared,
        answering: AtomicUsize::new(0),
        unmounted: AtomicBool::new(true),
        stale: AtomicBool::new(false),
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

    {
        let directory = directory.to_owned();
        std::thread::spawn(move || screen::serve_viewers(directory, viewers));
    }

    {
        let service = service.clone();
        std::thread::spawn(move || service.keep_mounted());
    }

    // Waits for the setup to finish, and is left to itself: it finds the machine stopped and the
    // disk closed.
    if let (Some(automatic), Some(served)) = (&config.automatic, &served) {
        let (directory, served, shared, automatic) = (
            directory.to_owned(),
            served.clone(),
            service.shared.clone(),
            automatic.clone(),
        );
        std::thread::spawn(move || take_automatic(&directory, &served, &shared, &automatic));
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
    service.screen.close();
    // Whoever asked for the stop is still owed an answer.
    let deadline = Instant::now() + Duration::from_secs(2);
    while service.answering.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    // A seal that is under way is waited for.
    if let Some(served) = &served {
        served.close();
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
