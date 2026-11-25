//! Machines on KVM, through cloud-hypervisor.
//!
//! Each machine has a cloud-hypervisor process of its own, and a passt process that gives it a
//! network: passt serves a vhost-user network device, and ends when the machine hangs up. Both are
//! children of the machine's process and end with it. The guest's console is
//! cloud-hypervisor's standard input and output. cloud-hypervisor is told what to do over its
//! API socket, a little HTTP, and passes connections to ports of the guest's socket device on
//! from a Unix socket: `CONNECT <port>`, answered `OK <port>`.
//!
//! Each shared folder has a virtiofsd process of its own, which serves a virtio-fs device tagged
//! with the share's name. Like passt, it serves one connection and is started again when it ends
//! while the machine runs.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::{DiskAttachment, Engine, Spec, Stopped, share_socket_name, tools};
use crate::paths::socket_path;
use crate::store::Share;
use crate::{Context, Error, ErrorCode, Result};

/// How long cloud-hypervisor gets to start a machine, or to answer a request.
const LIMIT: Duration = Duration::from_secs(60);
/// How long passt, or virtiofsd, gets to start listening.
const PASST_LIMIT: Duration = Duration::from_secs(10);
/// How often a virtiofsd that couldn't be started again is tried.
const RETRY: Duration = Duration::from_secs(5);
/// How long a port of the guest gets to answer.
const CONNECT_LIMIT: Duration = Duration::from_secs(5);
/// The guest's address on its socket device. The host's is 2.
const GUEST_CID: u32 = 3;
/// What is kept of what cloud-hypervisor says on its standard error, to say why it failed.
const ERROR_TAIL: usize = 4096;
/// How large passt's log grows. It notes every packet it drops, which the machine's log, which
/// is never cut short, has no room for.
const NETWORK_LOG_SIZE: u64 = 1 << 20;

fn failed(what: impl std::fmt::Display) -> Error {
    Error::new(ErrorCode::Engine, what.to_string())
}

pub(crate) struct Machine {
    inner: Arc<Inner>,
    /// What cloud-hypervisor is started with. Taken when it is.
    command: Mutex<Option<Command>>,
}

struct Inner {
    /// The machine's folder and ID, which name the sockets of its shares.
    directory: PathBuf,
    id: String,
    api: PathBuf,
    vsock: PathBuf,
    network: PathBuf,
    passt: PathBuf,
    /// passt's log.
    network_log: PathBuf,
    /// The end of what passt said on its standard error, which its log has too.
    passt_said: Arc<Mutex<Vec<u8>>>,
    stopped: Stopped,
    state: Mutex<Processes>,
    changed: Condvar,
    /// The end of what cloud-hypervisor said on its standard error.
    said: Arc<Mutex<Vec<u8>>>,
}

#[derive(Default)]
struct Processes {
    /// cloud-hypervisor's, while it hasn't been waited for.
    hypervisor: Option<u32>,
    /// cloud-hypervisor was started.
    started: bool,
    passt: Option<Child>,
    /// The power was cut on purpose: the end of cloud-hypervisor is no failure.
    cut: bool,
    /// cloud-hypervisor has ended.
    ended: bool,
    /// The shared folders, by name.
    shares: BTreeMap<String, Served>,
}

/// A shared folder, and the virtiofsd that serves it.
struct Served {
    share: Share,
    socket: PathBuf,
    /// While it runs.
    server: Option<Child>,
}

impl Processes {
    /// The machine has stopped, or is stopping, and nothing is to be started for it.
    fn over(&self) -> bool {
        self.cut || self.ended || (self.started && self.hypervisor.is_none())
    }
}

/// What cloud-hypervisor calls the device of the share called `name`.
fn device_id(name: &str) -> String {
    share_socket_name(name)
}

/// virtiofsd's socket, and the file next to it that holds its process ID.
fn remove_share_sockets(socket: &Path) {
    let mut pid = socket.as_os_str().to_owned();
    pid.push(".pid");
    _ = std::fs::remove_file(socket);
    _ = std::fs::remove_file(pid);
}

fn end(mut child: Child) {
    _ = child.kill();
    _ = child.wait();
}

/// Has the child end when this process does, whatever ends it.
fn end_with_parent(command: &mut Command) {
    // Safety: prctl is safe to call between fork and exec. The signal comes when the thread that
    // started the child ends: machines are started on the process's main thread, which lasts.
    unsafe {
        command.pre_exec(|| {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// Starts `command` from a thread that lasts as long as the child does: one that [`end_with_parent`]
/// set up gets its signal when the thread that started it ends.
fn spawn_lasting(mut command: Command) -> std::io::Result<Child> {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let child = command.spawn();
        let pid = child.as_ref().ok().map(Child::id);
        _ = sender.send(child);
        if let Some(pid) = pid {
            // Waited for without taking it away: whoever holds the child does that.
            // Safety: a plain call with a buffer that lives through it.
            unsafe {
                let mut info: libc::siginfo_t = std::mem::zeroed();
                while libc::waitid(libc::P_PID, pid, &mut info, libc::WEXITED | libc::WNOWAIT) == -1
                    && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
                {
                }
            }
        }
    });
    receiver
        .recv()
        .unwrap_or_else(|_| Err(std::io::Error::other("the thread that starts it ended")))
}

/// Says what is wrong if this user can't use KVM.
fn check_kvm() -> Result<()> {
    match std::fs::File::options().read(true).write(true).open("/dev/kvm") {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(Error::new(
            ErrorCode::Unsupported,
            "This computer has no /dev/kvm: its processor's virtualization is off, or KVM isn't loaded.",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => Err(Error::new(
            ErrorCode::Unsupported,
            "This user can't use /dev/kvm. Add it to the kvm group (sudo usermod -aG kvm $USER) and sign in again.",
        )),
        Err(error) => Err(error).context("open /dev/kvm"),
    }
}

impl Machine {
    pub fn new(spec: Spec, stopped: Stopped) -> Result<Self> {
        if spec.nested {
            return Err(super::no_nesting());
        }
        check_kvm()?;
        let tools = tools::find(&crate::cache_directory()?)?;
        let socket = |name: &str| socket_path(&spec.directory, &spec.id, name);
        let api = socket("api")?;
        let vsock = socket("vsock")?;
        let network = socket("network")?;

        let mut command = Command::new(&tools.hypervisor);
        command
            .arg("--api-socket")
            .arg(format!("path={}", api.display()))
            .arg("--firmware")
            .arg(&tools.firmware)
            .arg("--cpus")
            .arg(format!("boot={}", spec.cpus.max(1)))
            // vhost-user devices need memory that the processes serving them can map.
            .arg("--memory")
            .arg(format!("size={}M,shared=on", (spec.memory >> 20).max(128)));
        let mut disks = vec![match &spec.disk {
            DiskAttachment::File(path) => format!("path={},image_type=raw", path.display()),
            DiskAttachment::VhostUser(socket) => format!(
                "vhost_user=true,socket={},num_queues={},queue_size={}",
                socket.display(),
                disk::vhost::queues(spec.cpus),
                disk::vhost::QUEUE_SIZE
            ),
            DiskAttachment::Network(_) => {
                return Err(failed(
                    "cloud-hypervisor can't take a disk from a network block device.",
                ));
            }
        }];
        if let Some(seed) = &spec.seed {
            disks.push(format!("path={},readonly=on,image_type=raw", seed.display()));
        }
        let mut shares = BTreeMap::new();
        for share in &spec.shares {
            let socket = socket(&share_socket_name(&share.name))?;
            shares.insert(
                share.name.clone(),
                Served {
                    share: share.clone(),
                    socket,
                    server: None,
                },
            );
        }
        if !shares.is_empty() {
            command.arg("--fs").args(shares.values().map(|served| {
                format!(
                    "tag={},socket={},id={}",
                    served.share.name,
                    served.socket.display(),
                    device_id(&served.share.name)
                )
            }));
        }
        command
            .arg("--disk")
            .args(disks)
            .arg("--net")
            .arg(format!(
                "vhost_user=true,socket={},mac={}",
                network.display(),
                spec.mac_address
            ))
            .arg("--vsock")
            .arg(format!("cid={GUEST_CID},socket={}", vsock.display()))
            // The firmware writes to the serial port, which nobody reads. The guest's console is
            // `hvc0`, the virtio console.
            .args(["--serial", "null", "--console", "tty"])
            .stdin(Stdio::from(spec.console.input))
            .stdout(Stdio::from(spec.console.output))
            .stderr(Stdio::piped());
        end_with_parent(&mut command);

        Ok(Self {
            inner: Arc::new(Inner {
                directory: spec.directory.clone(),
                id: spec.id.clone(),
                api,
                vsock,
                network,
                passt: tools.passt,
                network_log: spec.directory.join("network.log"),
                passt_said: Arc::new(Mutex::new(Vec::new())),
                stopped,
                state: Mutex::new(Processes {
                    shares,
                    ..Processes::default()
                }),
                changed: Condvar::new(),
                said: Arc::new(Mutex::new(Vec::new())),
            }),
            command: Mutex::new(Some(command)),
        })
    }

    /// Starts passt, and returns once it listens.
    fn start_passt(inner: &Inner) -> Result<Child> {
        let network = &inner.network;
        remove_network_sockets(network);
        let mut command = Command::new(&inner.passt);
        command
            .args(["--vhost-user", "--foreground", "--quiet", "--socket"])
            .arg(network)
            .arg("--log-file")
            .arg(&inner.network_log)
            .arg("--log-size")
            .arg(NETWORK_LOG_SIZE.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        end_with_parent(&mut command);
        let mut passt = command.spawn().at("start", &inner.passt)?;
        if let Some(said) = passt.stderr.take() {
            let kept = inner.passt_said.clone();
            std::thread::spawn(move || keep_end(said, kept, false));
        }
        let deadline = Instant::now() + PASST_LIMIT;
        while !network.exists() {
            if let Ok(Some(status)) = passt.try_wait() {
                // What it said may still be on its way.
                std::thread::sleep(Duration::from_millis(50));
                return Err(failed(match last_line(&inner.passt_said.lock().unwrap()) {
                    Some(line) => format!("passt ended at once ({status}): {line}"),
                    None => format!(
                        "passt ended at once ({status}). {} may say why.",
                        inner.network_log.display()
                    ),
                }));
            }
            if Instant::now() > deadline {
                _ = passt.kill();
                _ = passt.wait();
                return Err(failed("passt didn't start listening in time."));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(passt)
    }

    /// Starts virtiofsd for `share` on `socket`, and returns once it listens.
    fn start_virtiofsd(share: &Share, socket: &Path) -> Result<Child> {
        let program = tools::virtiofsd()?;
        remove_share_sockets(socket);
        // Safety: neither can fail.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        let mut command = Command::new(&program);
        command
            .arg(format!("--socket-path={}", socket.display()))
            .arg("--shared-dir")
            .arg(&share.path)
            // As this user, who can't make the namespaces of a sandbox. The guest is the
            // user's own, and reaches nothing that the user can't.
            .args(["--sandbox=none", "--cache=auto", "--log-level=error"])
            // Files that anyone in the guest writes are this user's, which is all that a
            // virtiofsd of this user can write anyway.
            .arg(format!("--translate-uid=squash-guest:0:{uid}:4294967295"))
            .arg(format!("--translate-gid=squash-guest:0:{gid}:4294967295"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        if share.read_only {
            command.arg("--readonly");
        }
        end_with_parent(&mut command);
        // Shares are added from the thread of a request, which ends soon after.
        let mut server = spawn_lasting(command).at("start", &program)?;
        let said = Arc::new(Mutex::new(Vec::new()));
        if let Some(stderr) = server.stderr.take() {
            let kept = said.clone();
            std::thread::spawn(move || keep_end(stderr, kept, true));
        }
        let deadline = Instant::now() + PASST_LIMIT;
        while !socket.exists() {
            if let Ok(Some(status)) = server.try_wait() {
                std::thread::sleep(Duration::from_millis(50));
                return Err(failed(match last_line(&said.lock().unwrap()) {
                    Some(line) => format!("virtiofsd ended at once ({status}): {line}"),
                    None => format!("virtiofsd ended at once ({status})."),
                }));
            }
            if Instant::now() > deadline {
                end(server);
                return Err(failed("virtiofsd didn't start listening in time."));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(server)
    }

    /// Starts virtiofsd for each share that has none running. Says what failed, after trying
    /// them all.
    fn start_shares(inner: &Inner) -> Result<()> {
        let missing: Vec<(String, Share, PathBuf)> = {
            let state = inner.state.lock().unwrap();
            state
                .shares
                .iter()
                .filter(|(_, served)| served.server.is_none())
                .map(|(name, served)| (name.clone(), served.share.clone(), served.socket.clone()))
                .collect()
        };
        let mut failure = None;
        for (name, share, socket) in missing {
            let server = match Self::start_virtiofsd(&share, &socket) {
                Ok(server) => server,
                Err(error) => {
                    failure.get_or_insert(error);
                    continue;
                }
            };
            // A share that was taken away or shared anew in the meantime has what it needs.
            match inner.state.lock().unwrap().shares.get_mut(&name) {
                Some(served) if served.server.is_none() => served.server = Some(server),
                _ => end(server),
            }
        }
        failure.map_or(Ok(()), Err)
    }

    /// Ends every virtiofsd.
    fn end_shares(inner: &Inner) {
        let servers: Vec<(Option<Child>, PathBuf)> = {
            let mut state = inner.state.lock().unwrap();
            state
                .shares
                .values_mut()
                .map(|served| (served.server.take(), served.socket.clone()))
                .collect()
        };
        for (server, socket) in servers {
            if let Some(server) = server {
                end(server);
            }
            remove_share_sockets(&socket);
        }
    }

    /// Starts passt, and starts it and virtiofsd again each time they end while the machine
    /// runs: each serves one connection, and cloud-hypervisor connects again when the guest
    /// restarts.
    fn start_network(&self) -> Result<()> {
        let passt = Self::start_passt(&self.inner)?;
        self.inner.state.lock().unwrap().passt = Some(passt);
        let inner = self.inner.clone();
        // The programs end with the thread that started them, so this one lasts as long as the
        // machine.
        std::thread::spawn(move || {
            let mut retried = Instant::now();
            loop {
                std::thread::sleep(Duration::from_millis(100));
                let (passt_ended, server_ended) = {
                    let mut state = inner.state.lock().unwrap();
                    if state.over() {
                        return;
                    }
                    let passt_ended = match state.passt.as_mut().map(Child::try_wait) {
                        Some(Ok(None)) => false,
                        None => return,
                        Some(_) => true,
                    };
                    if passt_ended && let Some(mut ended) = state.passt.take() {
                        _ = ended.wait();
                    }
                    // One that couldn't be started is tried again now and then.
                    let mut server_ended = retried.elapsed() > RETRY
                        && state.shares.values().any(|served| served.server.is_none());
                    for served in state.shares.values_mut() {
                        if let Some(server) = &mut served.server
                            && !matches!(server.try_wait(), Ok(None))
                        {
                            _ = server.wait();
                            served.server = None;
                            server_ended = true;
                        }
                    }
                    (passt_ended, server_ended)
                };
                if passt_ended {
                    let passt = match Self::start_passt(&inner) {
                        Ok(passt) => passt,
                        Err(error) => {
                            crate::process::log(&format!("Couldn't start passt again: {error}"));
                            return;
                        }
                    };
                    let mut state = inner.state.lock().unwrap();
                    if state.over() {
                        drop(state);
                        end(passt);
                        return;
                    }
                    state.passt = Some(passt);
                }
                if server_ended {
                    retried = Instant::now();
                    // One that was shared again in the meantime has a server of its own already.
                    if let Err(error) = Self::start_shares(&inner) {
                        crate::process::log(&format!("Couldn't start virtiofsd again: {error}"));
                    }
                    if inner.state.lock().unwrap().over() {
                        Self::end_shares(&inner);
                        return;
                    }
                }
            }
        });
        Ok(())
    }

    /// Ends passt, which ends by itself once the machine has hung up.
    fn end_network(inner: &Inner) {
        let passt = inner.state.lock().unwrap().passt.take();
        if let Some(mut passt) = passt {
            _ = passt.kill();
            _ = passt.wait();
        }
        remove_network_sockets(&inner.network);
    }

    /// Waits for cloud-hypervisor to end, and says so.
    fn watch(inner: Arc<Inner>, mut hypervisor: Child) {
        let pid = hypervisor.id();
        // Waited for without taking it away, so that its process ID can't belong to another
        // process while `stop` may still signal it.
        // Safety: a plain call with a buffer that lives through it.
        unsafe {
            let mut info: libc::siginfo_t = std::mem::zeroed();
            while libc::waitid(libc::P_PID, pid, &mut info, libc::WEXITED | libc::WNOWAIT) == -1
                && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
            {}
        }
        let cut = {
            let mut state = inner.state.lock().unwrap();
            state.hypervisor = None;
            state.cut
        };
        let status = hypervisor.wait();
        Self::end_network(&inner);
        Self::end_shares(&inner);
        _ = std::fs::remove_file(&inner.api);
        _ = std::fs::remove_file(&inner.vsock);
        let failure = match status {
            _ if cut => None,
            Ok(status) if status.success() => None,
            Ok(status) => Some(match last_line(&inner.said.lock().unwrap()) {
                Some(line) => format!("cloud-hypervisor ended ({status}): {line}"),
                None => format!("cloud-hypervisor ended ({status})."),
            }),
            Err(error) => Some(format!("Couldn't wait for cloud-hypervisor: {error}.")),
        };
        {
            let mut state = inner.state.lock().unwrap();
            state.ended = true;
            inner.changed.notify_all();
        }
        (inner.stopped)(failure);
    }

    /// Waits until cloud-hypervisor answers on its socket with the machine running.
    fn wait_until_running(&self) -> Result<()> {
        let deadline = Instant::now() + LIMIT;
        loop {
            if self.inner.state.lock().unwrap().ended {
                // What it said last may still be on its way.
                std::thread::sleep(Duration::from_millis(50));
                let said = self.inner.said.lock().unwrap();
                return Err(failed(match last_line(&said) {
                    Some(line) => format!("cloud-hypervisor didn't start the machine: {line}"),
                    None => {
                        "cloud-hypervisor didn't start the machine. The machine's log may say why.".into()
                    }
                }));
            }
            if let Ok((200, body)) = api(&self.inner.api, "GET", "vm.info", None)
                && body.contains("\"Running\"")
            {
                return Ok(());
            }
            if Instant::now() > deadline {
                return Err(failed("cloud-hypervisor didn't start the machine in time."));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// The last line of what a program said that is an error, or else the last that isn't empty,
/// without its log's prefix.
fn last_line(said: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(said);
    let mut lines = text.lines().rev().map(str::trim).filter(|line| !line.is_empty());
    let line = lines
        .clone()
        .find(|line| line.to_ascii_lowercase().contains("error"))
        .or_else(|| lines.next())?;
    // cloud-hypervisor's lines start with a time and a place in its code.
    let line = line.rsplit_once("] ").map_or(line, |(_, message)| message);
    Some(line.to_string())
}

/// passt's socket, and the one next to it that it makes for moving a machine elsewhere.
fn remove_network_sockets(network: &Path) {
    let mut repair = network.as_os_str().to_owned();
    repair.push(".repair");
    _ = std::fs::remove_file(network);
    _ = std::fs::remove_file(repair);
}

/// Keeps the end of what `from` says, and passes it on to this process's standard error, which
/// is the machine's log, if `pass_on`.
fn keep_end(mut from: impl Read, kept: Arc<Mutex<Vec<u8>>>, pass_on: bool) {
    let mut buffer = [0; 4096];
    loop {
        match from.read(&mut buffer) {
            Ok(0) | Err(_) => return,
            Ok(count) => {
                if pass_on {
                    _ = std::io::stderr().write_all(&buffer[..count]);
                }
                let mut kept = kept.lock().unwrap();
                kept.extend_from_slice(&buffer[..count]);
                let excess = kept.len().saturating_sub(ERROR_TAIL);
                kept.drain(..excess);
            }
        }
    }
}

/// Asks cloud-hypervisor's API for `/api/v1/<what>`, with a JSON body if there is one. Returns
/// the status and the body of the answer.
fn api(
    socket: &Path,
    method: &str,
    what: &str,
    body: Option<&serde_json::Value>,
) -> std::io::Result<(u16, String)> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(LIMIT))?;
    stream.set_write_timeout(Some(LIMIT))?;
    let body = body.map(serde_json::Value::to_string).unwrap_or_default();
    let kind = if body.is_empty() {
        ""
    } else {
        "Content-Type: application/json\r\n"
    };
    stream.write_all(
        format!(
            "{method} /api/v1/{what} HTTP/1.1\r\nHost: localhost\r\nAccept: */*\r\n{kind}Content-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .as_bytes(),
    )?;
    let mut received = Vec::new();
    let mut buffer = [0; 4096];
    let invalid = |what: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, what.to_string());
    loop {
        if let Some(end) = received.windows(4).position(|window| window == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&received[..end]).to_string();
            let status = head
                .split(' ')
                .nth(1)
                .and_then(|status| status.parse().ok())
                .ok_or_else(|| invalid("an answer that isn't HTTP"))?;
            let length: usize = head
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
                .and_then(|(_, value)| value.trim().parse().ok())
                .unwrap_or(0);
            while received.len() < end + 4 + length {
                match stream.read(&mut buffer)? {
                    0 => break,
                    count => received.extend_from_slice(&buffer[..count]),
                }
            }
            let body = received.get(end + 4..).unwrap_or_default();
            let body = &body[..length.min(body.len())];
            return Ok((status, String::from_utf8_lossy(body).to_string()));
        }
        match stream.read(&mut buffer)? {
            0 => return Err(invalid("the answer ended early")),
            count => received.extend_from_slice(&buffer[..count]),
        }
    }
}

impl Engine for Machine {
    fn start(&self) -> Result<()> {
        let Some(mut command) = self.command.lock().unwrap().take() else {
            return Err(failed("The machine was started already."));
        };
        _ = std::fs::remove_file(&self.inner.api);
        _ = std::fs::remove_file(&self.inner.vsock);
        if let Err(error) = Self::start_shares(&self.inner) {
            Self::end_shares(&self.inner);
            return Err(error);
        }
        if let Err(error) = self.start_network() {
            Self::end_shares(&self.inner);
            return Err(error);
        }
        let mut hypervisor = match command.spawn() {
            Ok(hypervisor) => hypervisor,
            Err(error) => {
                Self::end_network(&self.inner);
                Self::end_shares(&self.inner);
                return Err(error).context(format_args!("start {}", command.get_program().display()));
            }
        };
        if let Some(said) = hypervisor.stderr.take() {
            let kept = self.inner.said.clone();
            std::thread::spawn(move || keep_end(said, kept, true));
        }
        {
            let mut state = self.inner.state.lock().unwrap();
            state.hypervisor = Some(hypervisor.id());
            state.started = true;
        }
        {
            let inner = self.inner.clone();
            std::thread::spawn(move || Self::watch(inner, hypervisor));
        }
        if let Err(error) = self.wait_until_running() {
            _ = self.stop();
            return Err(error);
        }
        Ok(())
    }

    fn request_stop(&self) -> Result<()> {
        match api(&self.inner.api, "PUT", "vm.power-button", None) {
            Ok((200..=299, _)) => Ok(()),
            Ok((status, body)) => Err(failed(format_args!(
                "cloud-hypervisor didn't press the power button ({status}): {}",
                body.trim()
            ))),
            Err(error) => Err(failed(format_args!(
                "Couldn't ask cloud-hypervisor to stop: {error}."
            ))),
        }
    }

    fn stop(&self) -> Result<()> {
        let mut state = self.inner.state.lock().unwrap();
        state.cut = true;
        if let Some(pid) = state.hypervisor {
            // Safety: the process hasn't been waited for, so the ID is still its own.
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
        } else if !state.started {
            return Ok(());
        }
        let (state, _) = self
            .inner
            .changed
            .wait_timeout_while(state, LIMIT, |state| !state.ended)
            .unwrap();
        if state.ended {
            Ok(())
        } else {
            Err(failed("cloud-hypervisor didn't end in time."))
        }
    }

    fn share(&self, shares: &[Share]) -> Result<()> {
        let wanted: BTreeMap<&str, &Share> =
            shares.iter().map(|share| (share.name.as_str(), share)).collect();
        let going: Vec<String> = {
            let state = self.inner.state.lock().unwrap();
            state
                .shares
                .iter()
                .filter(|(name, served)| wanted.get(name.as_str()) != Some(&&served.share))
                .map(|(name, _)| name.clone())
                .collect()
        };
        for name in going {
            let removed = api(
                &self.inner.api,
                "PUT",
                "vm.remove-device",
                Some(&serde_json::json!({ "id": device_id(&name) })),
            );
            match removed {
                Ok((200..=299, _)) => {}
                Ok((status, body)) => {
                    return Err(failed(format_args!(
                        "cloud-hypervisor didn't remove the share \"{name}\" ({status}): {}",
                        body.trim()
                    )));
                }
                Err(error) => {
                    return Err(failed(format_args!(
                        "Couldn't ask cloud-hypervisor to remove the share \"{name}\": {error}."
                    )));
                }
            }
            let served = self.inner.state.lock().unwrap().shares.remove(&name);
            if let Some(served) = served {
                if let Some(server) = served.server {
                    end(server);
                }
                remove_share_sockets(&served.socket);
            }
        }
        for share in shares {
            if self.inner.state.lock().unwrap().shares.contains_key(&share.name) {
                continue;
            }
            let socket = socket_path(
                &self.inner.directory,
                &self.inner.id,
                &share_socket_name(&share.name),
            )?;
            let server = Self::start_virtiofsd(share, &socket)?;
            let body = serde_json::json!({
                "tag": share.name,
                "socket": socket,
                "id": device_id(&share.name),
                "num_queues": 1,
                "queue_size": 1024,
            });
            let added = api(&self.inner.api, "PUT", "vm.add-fs", Some(&body));
            let refused = match added {
                Ok((200..=299, _)) => None,
                Ok((status, body)) => Some(format!(
                    "cloud-hypervisor didn't add the share \"{}\" ({status}): {}",
                    share.name,
                    body.trim()
                )),
                Err(error) => Some(format!(
                    "Couldn't ask cloud-hypervisor to add the share \"{}\": {error}.",
                    share.name
                )),
            };
            if let Some(refused) = refused {
                end(server);
                remove_share_sockets(&socket);
                return Err(failed(refused));
            }
            self.inner.state.lock().unwrap().shares.insert(
                share.name.clone(),
                Served {
                    share: share.clone(),
                    socket,
                    server: Some(server),
                },
            );
        }
        Ok(())
    }

    fn connect(&self, port: u32) -> Result<OwnedFd> {
        let unanswered = || {
            Error::new(
                ErrorCode::Guest,
                format!("Nothing in the guest answers on port {port} of its socket device."),
            )
        };
        let mut stream =
            UnixStream::connect(&self.inner.vsock).context("connect to the guest's socket device")?;
        stream
            .set_read_timeout(Some(CONNECT_LIMIT))
            .context("connect to the guest's socket device")?;
        stream
            .write_all(format!("CONNECT {port}\n").as_bytes())
            .context("connect to the guest's socket device")?;
        // One byte at a time: what follows the line is the guest's.
        let mut line = Vec::new();
        let mut byte = [0];
        loop {
            match stream.read(&mut byte) {
                Ok(1) if byte[0] == b'\n' => break,
                Ok(1) if line.len() < 64 => line.push(byte[0]),
                _ => return Err(unanswered()),
            }
        }
        if !line.starts_with(b"OK ") {
            return Err(unanswered());
        }
        stream
            .set_read_timeout(None)
            .context("connect to the guest's socket device")?;
        Ok(OwnedFd::from(stream))
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        _ = self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reason_is_the_last_line() {
        let said =
            b"cloud-hypervisor: 1.2s: <vmm> ERROR:vmm/src/lib.rs:100 -- [x] Error booting VM: no memory\n\n";
        assert_eq!(last_line(said).as_deref(), Some("Error booting VM: no memory"));
        assert_eq!(last_line(b"plain\n").as_deref(), Some("plain"));
        assert_eq!(
            last_line(b"error: a value is required\n\nFor more information, try '--help'.\n").as_deref(),
            Some("error: a value is required")
        );
        assert_eq!(last_line(b"\n \n"), None);
    }

    #[test]
    fn the_api_is_asked_over_http() {
        let folder = std::env::temp_dir().join(format!("ch-api-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        let socket = folder.join("api.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 1024];
            let count = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..count]).to_string();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 19\r\n\r\n{\"state\":\"Running\"}")
                .unwrap();
            request
        });
        let (status, body) = api(&socket, "GET", "vm.info", None).unwrap();
        assert_eq!((status, body.as_str()), (200, "{\"state\":\"Running\"}"));
        assert!(
            server
                .join()
                .unwrap()
                .starts_with("GET /api/v1/vm.info HTTP/1.1\r\n")
        );
        std::fs::remove_dir_all(&folder).unwrap();
    }

    #[test]
    fn a_body_goes_with_its_length() {
        let folder = std::env::temp_dir().join(format!("ch-api-body-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        let socket = folder.join("api.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 1024];
            while !request.ends_with(b"}") {
                let count = stream.read(&mut buffer).unwrap();
                request.extend_from_slice(&buffer[..count]);
            }
            stream.write_all(b"HTTP/1.1 204 No Content\r\n\r\n").unwrap();
            String::from_utf8(request).unwrap()
        });
        let body = serde_json::json!({ "id": "fs-a" });
        let (status, _) = api(&socket, "PUT", "vm.remove-device", Some(&body)).unwrap();
        assert_eq!(status, 204);
        let request = server.join().unwrap();
        assert!(request.starts_with("PUT /api/v1/vm.remove-device HTTP/1.1\r\n"));
        assert!(
            request.contains("Content-Type: application/json\r\nContent-Length: 13\r\n\r\n{\"id\":\"fs-a\"}")
        );
        std::fs::remove_dir_all(&folder).unwrap();
    }
}
