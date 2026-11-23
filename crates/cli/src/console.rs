//! `console`: connects this terminal to a machine's serial console.

use std::io::{IsTerminal, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;

use host::Result;

/// The key that ends the session: Control-], as in telnet. Everything else, Control-C included,
/// goes to the guest.
const LEAVE: u8 = 0x1D;
/// With input that is piped in rather than typed: how long to keep printing after the input
/// has ended and the guest has gone quiet.
const QUIET: i32 = 1500;

/// Relays between the terminal and the console until the user leaves or the machine stops.
pub fn run(mut stream: UnixStream, name: &str) -> Result<serde_json::Value> {
    let mut input = std::io::stdin();
    let mut output = std::io::stdout();
    let descriptor = input.as_raw_fd();
    // A job in the background of a terminal may neither change the terminal's settings nor read
    // from it: it would be stopped for trying. It only listens, for as long as it is left to.
    // Safety: both calls only look.
    let in_background = input.is_terminal() && unsafe { libc::tcgetpgrp(descriptor) != libc::getpgrp() };
    let typed = input.is_terminal() && !in_background;

    // Safety: an all-zero termios is a valid value for tcgetattr to fill in.
    let mut cooked: libc::termios = unsafe { std::mem::zeroed() };
    if typed {
        // Keys go to the guest as they are pressed, and the guest echoes them.
        // Safety: valid pointers to termios values.
        unsafe {
            libc::tcgetattr(descriptor, &mut cooked);
            let mut raw = cooked;
            libc::cfmakeraw(&mut raw);
            libc::tcsetattr(descriptor, libc::TCSANOW, &raw);
        }
        eprint!("[Connected to the console of {name}. Return shows a prompt, Control-] leaves.]\r\n");
    }

    let mut waiting = [
        libc::pollfd {
            fd: stream.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: descriptor,
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    let mut input_ended = in_background;
    let mut buffer = vec![0; 16 << 10];
    let farewell = loop {
        // Once piped input has run out, stay only until the guest stops answering.
        let limit = if input_ended && !in_background { QUIET } else { -1 };
        let count = if input_ended { 1 } else { 2 };
        // Safety: the array holds at least `count` pollfds.
        let ready = unsafe { libc::poll(waiting.as_mut_ptr(), count, limit) };
        if ready == 0 {
            break "Disconnected.";
        }
        if ready < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break "The connection was lost.";
        }
        if waiting[0].revents != 0 {
            match stream.read(&mut buffer) {
                Ok(count) if count > 0 => {
                    if output
                        .write_all(&buffer[..count])
                        .and_then(|()| output.flush())
                        .is_err()
                    {
                        break "Disconnected.";
                    }
                }
                _ => break "The machine has stopped.",
            }
        }
        if !input_ended && waiting[1].revents != 0 {
            match input.read(&mut buffer) {
                Ok(count) if count > 0 => {
                    let keys = &buffer[..count];
                    let leave = keys.iter().position(|&key| typed && key == LEAVE);
                    if stream.write_all(&keys[..leave.unwrap_or(count)]).is_err() {
                        break "The machine has stopped.";
                    }
                    if leave.is_some() {
                        break "Disconnected.";
                    }
                }
                _ => input_ended = true,
            }
        }
    };
    if typed {
        // Safety: puts back the settings that were read above.
        unsafe { libc::tcsetattr(descriptor, libc::TCSANOW, &cooked) };
        eprintln!("\n[{farewell}]");
    }
    Ok(serde_json::Value::Null)
}
