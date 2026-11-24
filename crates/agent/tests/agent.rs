//! The agent as the host sees it, listening on a Unix socket instead of the socket device.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use proto::ErrorCode;
use proto::agent::{Exec, Job, ask, greet_agent, receive_copy, run_job, send_copy};

struct Agent {
    folder: PathBuf,
    socket: PathBuf,
    process: Child,
}

impl Drop for Agent {
    fn drop(&mut self) {
        _ = self.process.kill();
        _ = self.process.wait();
        _ = std::fs::remove_dir_all(&self.folder);
    }
}

fn start(test: &str) -> Agent {
    let folder = std::env::temp_dir().join(format!("agent-test-{test}-{}", std::process::id()));
    _ = std::fs::remove_dir_all(&folder);
    std::fs::create_dir_all(&folder).unwrap();
    let socket = folder.join("agent.sock");
    let process = Command::new(env!("CARGO_BIN_EXE_agent"))
        .arg("listen")
        .arg(&socket)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while UnixStream::connect(&socket).is_err() {
        assert!(Instant::now() < deadline, "the agent doesn't listen");
        std::thread::sleep(Duration::from_millis(20));
    }
    Agent {
        folder,
        socket,
        process,
    }
}

/// The account the tests run as, which the agent runs commands as without switching.
fn user() -> String {
    let output = Command::new("id").arg("-un").output().unwrap();
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn exec(arguments: &[&str]) -> Exec {
    Exec {
        arguments: arguments.iter().map(|argument| argument.to_string()).collect(),
        user: user(),
        directory: None,
        environment: Default::default(),
        timeout: None,
    }
}

impl Agent {
    /// A connection on which the agent has said yes to `job`.
    fn job(&self, job: &Job) -> Result<UnixStream, proto::Error> {
        let mut stream = UnixStream::connect(&self.socket).unwrap();
        let hello = greet_agent(&mut stream, "test").unwrap();
        assert_eq!(hello.version, env!("CARGO_PKG_VERSION"));
        ask(&mut stream, job)?;
        Ok(stream)
    }
}

#[test]
fn a_command_takes_input_and_gives_output_errors_and_status() {
    let agent = start("exec");
    let mut job = exec(&["sh", "-c", "tr a-z A-Z; echo \"$GREETING\" >&2; pwd >&2; exit 3"]);
    job.environment.insert("GREETING".into(), "hello".into());
    job.directory = Some(agent.folder.to_string_lossy().into_owned());
    let stream = agent.job(&Job::Exec(job)).unwrap();
    let (mut output, mut errors) = (Vec::new(), Vec::new());
    let input = Box::new(&b"shout this"[..]);
    let ended = run_job(&stream, Some(input), &mut output, &mut errors).unwrap();
    assert_eq!(String::from_utf8(output).unwrap(), "SHOUT THIS");
    let errors = String::from_utf8(errors).unwrap();
    assert!(errors.starts_with("hello\n"), "{errors}");
    assert!(
        errors
            .trim_end()
            .ends_with(&*agent.folder.file_name().unwrap().to_string_lossy())
    );
    assert_eq!(ended.status, Some(3));
    assert!(!ended.timed_out);
}

#[test]
fn a_command_that_runs_too_long_is_ended() {
    let agent = start("timeout");
    let mut job = exec(&["sleep", "30"]);
    job.timeout = Some(1);
    let started = Instant::now();
    let stream = agent.job(&Job::Exec(job)).unwrap();
    let ended = run_job(&stream, None, &mut Vec::new(), &mut Vec::new()).unwrap();
    assert!(ended.timed_out);
    assert_eq!(ended.signal, Some(libc::SIGTERM));
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[test]
fn hanging_up_calls_a_command_off() {
    hang_up("hang-up", None);
}

#[test]
fn hanging_up_calls_a_command_off_before_its_time_is_up() {
    hang_up("hang-up-timeout", Some(60));
}

fn hang_up(test: &str, timeout: Option<u64>) {
    let agent = start(test);
    let pid_file = agent.folder.join("pid");
    let script = format!("echo $$ > '{}'; exec sleep 30", pid_file.display());
    let mut job = exec(&["sh", "-c", &script]);
    job.timeout = timeout;
    let stream = agent.job(&Job::Exec(job)).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let pid: i32 = loop {
        if let Ok(pid) = std::fs::read_to_string(&pid_file)
            .unwrap_or_default()
            .trim()
            .parse()
        {
            break pid;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    };
    drop(stream);
    // Safety: signal 0 only asks whether the process is there.
    while unsafe { libc::kill(pid, 0) } == 0 {
        assert!(Instant::now() < deadline, "the command still runs");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn what_is_refused_says_why() {
    let agent = start("refused");
    let mut nobody = exec(&["true"]);
    nobody.user = "no-such-account".into();
    assert_eq!(
        agent.job(&Job::Exec(nobody)).unwrap_err().code,
        ErrorCode::NotFound
    );
    let missing = exec(&["no-such-program-here"]);
    assert_eq!(
        agent.job(&Job::Exec(missing)).unwrap_err().code,
        ErrorCode::NotFound
    );
    let closed = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = closed.local_addr().unwrap().port();
    drop(closed);
    assert_eq!(
        agent.job(&Job::Tunnel { port }).unwrap_err().code,
        ErrorCode::Guest
    );
}

fn write_tree(root: &Path) {
    std::fs::create_dir_all(root.join("inner")).unwrap();
    std::fs::write(root.join("inner/data"), vec![42; 300_000]).unwrap();
    std::fs::write(root.join("note"), "a note").unwrap();
}

#[test]
fn a_folder_is_copied_in_and_out() {
    let agent = start("copy");
    let source = agent.folder.join("source");
    write_tree(&source);
    let guest = agent.folder.join("guest");
    let stream = agent
        .job(&Job::CopyIn {
            path: guest.to_string_lossy().into_owned(),
            user: user(),
        })
        .unwrap();
    send_copy(&stream, &source).unwrap();
    assert_eq!(
        std::fs::read(guest.join("inner/data")).unwrap(),
        vec![42; 300_000]
    );

    let back = agent.folder.join("back");
    std::fs::create_dir(&back).unwrap();
    let stream = agent
        .job(&Job::CopyOut {
            path: guest.to_string_lossy().into_owned(),
            user: user(),
        })
        .unwrap();
    receive_copy(&stream, &guest.to_string_lossy(), &back).unwrap();
    assert_eq!(
        std::fs::read_to_string(back.join("guest/note")).unwrap(),
        "a note"
    );

    let stream = agent
        .job(&Job::CopyOut {
            path: agent.folder.join("missing").to_string_lossy().into_owned(),
            user: user(),
        })
        .unwrap();
    let error = receive_copy(&stream, "missing", &back.join("missing")).unwrap_err();
    assert!(error.message.contains("missing"), "{}", error.message);
}

#[test]
fn a_tunnel_reaches_a_port() {
    let agent = start("tunnel");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let (mut connection, _) = listener.accept().unwrap();
        let mut heard = [0; 4];
        connection.read_exact(&mut heard).unwrap();
        connection
            .write_all(&heard.map(|byte| byte.to_ascii_uppercase()))
            .unwrap();
    });
    let mut stream = agent.job(&Job::Tunnel { port }).unwrap();
    stream.write_all(b"ping").unwrap();
    let mut heard = String::new();
    stream.read_to_string(&mut heard).unwrap();
    assert_eq!(heard, "PING");
}

#[test]
fn the_agent_says_what_the_system_is() {
    let agent = start("info");
    let mut stream = UnixStream::connect(&agent.socket).unwrap();
    greet_agent(&mut stream, "test").unwrap();
    let info = ask(&mut stream, &Job::Info).unwrap().unwrap();
    assert_eq!(info.agent, env!("CARGO_PKG_VERSION"));
    assert!(!info.kernel.is_empty() && !info.host_name.is_empty());
    assert_eq!(info.setup, None);
}
