//! Machines on KVM, through cloud-hypervisor.
//!
//! Each machine has a cloud-hypervisor process of its own, and a passt process that gives it a
//! network: passt serves a vhost-user network device, and ends when the machine hangs up. Both are
//! children of the machine's process and end with it. The guest's console is
//! cloud-hypervisor's standard input and output. cloud-hypervisor is told what to do over its
//! API socket, a little HTTP, and passes connections to ports of the guest's socket device on
//! from a Unix socket: `CONNECT <port>`, answered `OK <port>`.

use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::{DiskAttachment, Engine, Spec, Stopped, tools};
use crate::paths::socket_path;
use crate::{Context, Error, ErrorCode, Result};

/// How long cloud-hypervisor gets to start a machine, or to answer a request.
const LIMIT: Duration = Duration::from_secs(60);
/// How long passt gets to start listening.
const PASST_LIMIT: Duration = Duration::from_secs(10);
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
        let socket = |name| socket_path(&spec.directory, &spec.id, name);
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
                api,
                vsock,
                network,
                passt: tools.passt,
                network_log: spec.directory.join("network.log"),
                passt_said: Arc::new(Mutex::new(Vec::new())),
                stopped,
                state: Mutex::new(Processes::default()),
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

    /// Starts passt, and starts it again each time it ends while the machine runs: it serves
    /// one connection, and cloud-hypervisor connects again when the guest restarts.
    fn start_network(&self) -> Result<()> {
        let passt = Self::start_passt(&self.inner)?;
        self.inner.state.lock().unwrap().passt = Some(passt);
        let inner = self.inner.clone();
        // passt ends with the thread that started it, so this one lasts as long as the machine.
        std::thread::spawn(move || {
            let over =
                |state: &Processes| state.cut || state.ended || (state.started && state.hypervisor.is_none());
            loop {
                std::thread::sleep(Duration::from_millis(100));
                {
                    let mut state = inner.state.lock().unwrap();
                    if over(&state) {
                        return;
                    }
                    match state.passt.as_mut().map(Child::try_wait) {
                        Some(Ok(None)) => continue,
                        None => return,
                        Some(_) => {}
                    }
                    if let Some(mut ended) = state.passt.take() {
                        _ = ended.wait();
                    }
                }
                let passt = match Self::start_passt(&inner) {
                    Ok(passt) => passt,
                    Err(error) => {
                        crate::process::log(&format!("Couldn't start passt again: {error}"));
                        return;
                    }
                };
                let mut state = inner.state.lock().unwrap();
                if over(&state) {
                    drop(state);
                    let mut passt = passt;
                    _ = passt.kill();
                    _ = passt.wait();
                    return;
                }
                state.passt = Some(passt);
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
            if let Ok((200, body)) = api(&self.inner.api, "GET", "vm.info")
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

/// Asks cloud-hypervisor's API for `/api/v1/<what>`. Returns the status and the body.
fn api(socket: &Path, method: &str, what: &str) -> std::io::Result<(u16, String)> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(LIMIT))?;
    stream.set_write_timeout(Some(LIMIT))?;
    stream.write_all(
        format!(
            "{method} /api/v1/{what} HTTP/1.1\r\nHost: localhost\r\nAccept: */*\r\nContent-Length: 0\r\n\r\n"
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
        self.start_network()?;
        let mut hypervisor = match command.spawn() {
            Ok(hypervisor) => hypervisor,
            Err(error) => {
                Self::end_network(&self.inner);
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
        match api(&self.inner.api, "PUT", "vm.power-button") {
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
        let (status, body) = api(&socket, "GET", "vm.info").unwrap();
        assert_eq!((status, body.as_str()), (200, "{\"state\":\"Running\"}"));
        assert!(
            server
                .join()
                .unwrap()
                .starts_with("GET /api/v1/vm.info HTTP/1.1\r\n")
        );
        std::fs::remove_dir_all(&folder).unwrap();
    }
}
