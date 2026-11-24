//! The guest's screen, as the machine's process reaches it: the VNC server of the desktop in the
//! guest, through the agent's tunnel to [`SCREEN_PORT`].
//!
//! The process keeps one connection of its own for screenshots and input, and passes each
//! connection to its `vnc` socket on to the guest's server: a viewer, or an app that shows the
//! machine's screen, speaks VNC there as it would to any server.

use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use proto::agent::Job;
use proto::{Input, SCREEN_PORT, ScreenSize};

use crate::rfb::Rfb;
use crate::{Error, ErrorCode, Result, guest, png};

/// How long the guest's VNC server gets to answer.
const PATIENCE: Duration = Duration::from_secs(15);

/// A connection to the guest's VNC server, through the agent.
fn connect(directory: &Path) -> Result<UnixStream> {
    let (stream, _) = guest::start(directory, &Job::Tunnel { port: SCREEN_PORT }).map_err(|error| {
        match error.code {
            ErrorCode::Guest if error.message.contains(&format!("port {SCREEN_PORT}")) => Error::new(
                ErrorCode::Guest,
                format!(
                    "The guest shows no desktop: nothing listens on its screen's port, {SCREEN_PORT}. A machine gets one from a desktop setup script, such as --script xfce, and it comes up a few seconds after the guest has started."
                ),
            ),
            _ => error,
        }
    })?;
    Ok(stream)
}

fn failed(error: std::io::Error) -> Error {
    Error::new(
        ErrorCode::Guest,
        format!("The guest's screen didn't answer: {error}."),
    )
}

/// The process's own connection to the guest's screen, made when it is first needed and again
/// after the guest's server has gone, as it does when the guest restarts.
pub(crate) struct Screen {
    directory: PathBuf,
    client: Mutex<Option<Rfb<UnixStream>>>,
}

impl Screen {
    pub fn new(directory: &Path) -> Self {
        Self {
            directory: directory.to_owned(),
            client: Mutex::new(None),
        }
    }

    /// Does `work` with a connection, and once more with a new one if the old one has failed
    /// before anything of the work reached the server.
    fn with<T>(&self, work: impl Fn(&mut Rfb<UnixStream>) -> Result<T>) -> Result<T> {
        let mut client = self.client.lock().unwrap();
        let reused = client.is_some();
        for attempt in 0..2 {
            if client.is_none() {
                let stream = connect(&self.directory)?;
                stream
                    .set_read_timeout(Some(PATIENCE))
                    .and_then(|()| stream.set_write_timeout(Some(PATIENCE)))
                    .map_err(failed)?;
                *client = Some(Rfb::connect(stream).map_err(failed)?);
            }
            let connected = client.as_mut().expect("connected above");
            match work(connected) {
                Ok(value) => return Ok(value),
                // Said of the request, not of the connection.
                Err(error) if error.code == ErrorCode::InvalidArgument => return Err(error),
                Err(error) => {
                    // Input that went out in part isn't sent again: it would be done twice.
                    let partly_done = connected.partly_sent();
                    *client = None;
                    // Only a connection that had been made before may have gone stale.
                    if attempt == 1 || !reused || partly_done {
                        return Err(error);
                    }
                }
            }
        }
        unreachable!("the second attempt returns")
    }

    /// The screen as it is now, as a PNG.
    pub fn screenshot(&self) -> Result<(ScreenSize, Vec<u8>)> {
        let (size, rgb) = self.with(|client| {
            client.capture().map_err(failed)?;
            Ok((client.size(), client.rgb()))
        })?;
        Ok((size, png::encode(size.width, size.height, &rgb)))
    }

    /// Sends the events, unless one of them is off the screen: a place that isn't there is a
    /// mistake, which clicking at the screen's edge instead would hide.
    pub fn input(&self, events: &[Input]) -> Result<ScreenSize> {
        self.with(|client| {
            let size = client.size();
            for event in events {
                if let Input::Pointer { x, y, .. } = *event
                    && (x >= size.width || y >= size.height)
                {
                    return Err(Error::new(
                        ErrorCode::InvalidArgument,
                        format!(
                            "{x}, {y} is off the screen, which is {}x{}: places go from 0, 0 at its top left.",
                            size.width, size.height
                        ),
                    ));
                }
            }
            client.input(events).map_err(failed)?;
            Ok(size)
        })
    }

    /// Lets go of the connection, as the machine stops.
    pub fn close(&self) {
        *self.client.lock().unwrap() = None;
    }
}

/// Passes each connection to `listener` on to the guest's VNC server, for as long as the process
/// lives.
pub(crate) fn serve_viewers(directory: PathBuf, listener: UnixListener) {
    for viewer in listener.incoming() {
        let Ok(viewer) = viewer else {
            std::thread::sleep(Duration::from_millis(100));
            continue;
        };
        let directory = directory.clone();
        std::thread::spawn(move || match connect(&directory) {
            Ok(guest) => proto::relay::relay(viewer, guest),
            // A viewer has nobody to tell why but the machine's log.
            Err(error) => crate::process::log(&format!("A viewer couldn't reach the screen: {error}")),
        });
    }
}
