//! The guest's agent, as the host reaches it: through the machine's process, which connects to
//! the agent's port of the guest's socket device.
//!
//! Before a job, an agent that is older than the one this program carries is replaced, and a
//! guest clock that has fallen behind, as it does while the host sleeps, is set.

use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use proto::agent::{AGENT_PORT, AgentHello, GuestInfo, Job, ask, greet_agent, run_job};

use crate::{AGENT, AGENT_BUILT, Error, ErrorCode, NAME, Result, client};

/// How long the agent gets to say hello, and to answer.
const PATIENCE: Duration = Duration::from_secs(10);
/// How long a replaced agent gets to come back.
const RESTART: Duration = Duration::from_secs(20);
/// A guest's clock that is off by more than this is set.
const CLOCK_TOLERANCE: Duration = Duration::from_secs(2);

struct Connection {
    stream: UnixStream,
    hello: AgentHello,
}

/// A connection to the agent that has said hello.
fn connect(directory: &Path) -> Result<Connection> {
    let mut stream = client::connect_guest(directory, AGENT_PORT).map_err(|error| match error.code {
        ErrorCode::Guest => Error::new(
            ErrorCode::Guest,
            "The guest's agent doesn't answer. It starts with the guest's system, which may still be starting.",
        ),
        // A process started by a build from before there were agents.
        ErrorCode::Protocol => Error::new(
            ErrorCode::Protocol,
            format!(
                "The machine was started by an older build of {NAME}, which can't reach its agent. Stop it and start it again."
            ),
        ),
        _ => error,
    })?;
    stream
        .set_read_timeout(Some(PATIENCE))
        .map_err(|error| Error::new(ErrorCode::Io, format!("Couldn't set a time limit: {error}.")))?;
    let hello = greet_agent(&mut stream, env!("CARGO_PKG_VERSION"))?;
    Ok(Connection { stream, hello })
}

/// The numbers of a version: `0.1.0` gives 0, 1, 0.
fn version(text: &str) -> Vec<u64> {
    text.split('.').map(|part| part.parse().unwrap_or(0)).collect()
}

/// Whether the agent that said `hello` is to be replaced by the one this program carries: it is
/// of an older version, or an earlier build of the same version. Never the other way round, so
/// that two programs that use one machine don't take turns replacing it.
fn outdated(hello: &AgentHello) -> bool {
    let (theirs, mine) = (version(&hello.version), version(env!("CARGO_PKG_VERSION")));
    theirs < mine || theirs == mine && hello.built < AGENT_BUILT.parse().unwrap_or(0)
}

/// The time on this computer, in nanoseconds since 1970.
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos() as u64)
}

/// Replaces the agent with the one this program carries, and returns once the new one answers.
fn update(directory: &Path, mut connection: Connection) -> Result<Connection> {
    ask(
        &mut connection.stream,
        &Job::Update {
            size: AGENT.len() as u64,
        },
    )?;
    let ended = run_job(
        &connection.stream,
        Some(Box::new(AGENT)),
        &mut std::io::sink(),
        &mut std::io::sink(),
    )?;
    if let Some(error) = ended.error {
        return Err(error);
    }
    let deadline = Instant::now() + RESTART;
    loop {
        match connect(directory) {
            Ok(connection) if !outdated(&connection.hello) => return Ok(connection),
            // The old one, which hasn't ended yet.
            Ok(_) => {}
            Err(error) if Instant::now() >= deadline => return Err(error),
            Err(_) => {}
        }
        if Instant::now() >= deadline {
            return Err(Error::new(
                ErrorCode::Guest,
                "The guest's agent was replaced, and the new one doesn't answer.",
            ));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// A connection to the agent of the machine in `directory` on which it has taken `job` on, and
/// what it answered.
pub(crate) fn start(directory: &Path, job: &Job) -> Result<(UnixStream, Option<GuestInfo>)> {
    let mut connection = connect(directory)?;
    if outdated(&connection.hello) {
        // An agent that couldn't be replaced, in a guest whose disk is full for one, still
        // does the job: that may be what frees the disk.
        connection = match update(directory, connection) {
            Ok(connection) => connection,
            Err(_) => connect(directory)?,
        };
    }
    let off = now().abs_diff(connection.hello.time);
    if off > CLOCK_TOLERANCE.as_nanos() as u64 {
        // A clock that can't be set leaves the job to be done all the same.
        _ = ask(&mut connection.stream, &Job::SetClock { nanoseconds: now() });
        connection = connect(directory)?;
    }
    let answer = ask(&mut connection.stream, job)?;
    _ = connection.stream.set_read_timeout(None);
    Ok((connection.stream, answer))
}

/// What the guest is and how it is doing.
pub(crate) fn info(directory: &Path) -> Result<GuestInfo> {
    let (_, info) = start(directory, &Job::Info)?;
    info.ok_or_else(|| {
        Error::new(
            ErrorCode::Protocol,
            "The guest's agent said nothing about the guest.",
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello(version: &str, built: u64) -> AgentHello {
        AgentHello {
            protocol: 1,
            version: version.into(),
            built,
            time: 0,
        }
    }

    #[test]
    fn only_an_older_agent_or_an_earlier_build_is_replaced() {
        let ours: u64 = AGENT_BUILT.parse().unwrap();
        let mine = env!("CARGO_PKG_VERSION");
        assert!(!outdated(&hello(mine, ours)));
        assert!(outdated(&hello(mine, ours - 1)));
        assert!(!outdated(&hello(mine, ours + 1)));
        assert!(outdated(&hello("0.0.1", ours + 1)));
        assert!(!outdated(&hello("999.0", 0)));
    }
}
