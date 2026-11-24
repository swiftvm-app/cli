//! The commands that see and work a guest's desktop: `vnc`, `screenshot`, `click`, `move`,
//! `drag`, `scroll`, `type`, `key`, `open` and `clipboard`.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use host::{Button, Direction, Error, ErrorCode, Host, NAME, Result};
use serde_json::json;

use crate::{Arguments, Done, invalid, plain};

/// A number of pixels from the screen's left or top edge.
fn coordinate(word: &str) -> Result<u32> {
    word.parse().map_err(|_| {
        invalid(format!(
            "\"{word}\" isn't a place on the screen: give x and y in pixels."
        ))
    })
}

/// The machine, and the words after it.
fn machine_and(arguments: &Arguments) -> Result<(&str, &[String])> {
    match arguments.positional.split_first() {
        Some((name, rest)) => Ok((name, rest)),
        None => Err(invalid("Say which machine.")),
    }
}

/// The time now as `20261124-153102`, in this computer's time zone.
pub(crate) fn stamp() -> String {
    // Safety: time and localtime_r only write to what they are given.
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut local: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut local);
        format!(
            "{:04}{:02}{:02}-{:02}{:02}{:02}",
            local.tm_year + 1900,
            local.tm_mon + 1,
            local.tm_mday,
            local.tm_hour,
            local.tm_min,
            local.tm_sec
        )
    }
}

/// Where a screenshot goes when nobody said: a folder of this program's in the system's
/// temporary folder.
fn default_path(machine: &str) -> Result<PathBuf> {
    let folder = std::env::temp_dir().join(NAME);
    std::fs::create_dir_all(&folder).map_err(|error| {
        Error::new(
            ErrorCode::Io,
            format!("Couldn't create {}: {error}.", folder.display()),
        )
    })?;
    Ok(folder.join(format!("{machine}-{}.png", stamp())))
}

/// Writes `bytes` to `path` whole or not at all.
fn write_whole(path: &Path, bytes: &[u8]) -> Result<()> {
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default();
    let partial = path.with_file_name(format!(".{file_name}.{}", std::process::id()));
    std::fs::write(&partial, bytes)
        .and_then(|()| std::fs::rename(&partial, path))
        .map_err(|error| {
            _ = std::fs::remove_file(&partial);
            Error::new(
                ErrorCode::Io,
                format!("Couldn't write {}: {error}.", path.display()),
            )
        })
}

pub fn run(command: &str, words: &[String], json: bool) -> Result<Done> {
    let say = |text: String| {
        if !json {
            println!("{text}");
        }
    };
    let host = Host::open()?;
    match command {
        "vnc" => {
            let arguments = Arguments::parse(words, &[], &[])?;
            let name = arguments.subject("machine")?;
            let socket = host.vnc_socket(name)?;
            say(socket.display().to_string());
            Ok(json!({ "socket": socket }).into())
        }
        "screenshot" => {
            let arguments = Arguments::parse(words, &[], &[])?;
            let (name, rest) = machine_and(&arguments)?;
            let file = match rest {
                [] => None,
                [file] => Some(file.as_str()),
                _ => return Err(invalid("A screenshot goes to one file.")),
            };
            if file == Some("-") && std::io::stdout().is_terminal() {
                return Err(invalid(
                    "The picture would go to the terminal. Send it to a file or a program: screenshot <machine> - > file.png.",
                ));
            }
            let picture = host.screenshot(name)?;
            if file == Some("-") {
                let mut output = std::io::stdout().lock();
                output
                    .write_all(&picture.png)
                    .and_then(|()| output.flush())
                    .map_err(|error| {
                        Error::new(ErrorCode::Io, format!("Couldn't write the picture: {error}."))
                    })?;
                return Ok(serde_json::Value::Null.into());
            }
            let path = match file {
                Some(file) => PathBuf::from(file),
                None => default_path(name)?,
            };
            write_whole(&path, &picture.png)?;
            say(path.display().to_string());
            Ok(json!({ "path": path, "width": picture.width, "height": picture.height }).into())
        }
        "click" => {
            let arguments = Arguments::parse(words, &["count"], &["right", "middle", "double"])?;
            let (name, rest) = machine_and(&arguments)?;
            let [x, y] = rest else {
                return Err(invalid("Say where to click: click <machine> <x> <y>."));
            };
            let button = if arguments.flag("right") {
                Button::Right
            } else if arguments.flag("middle") {
                Button::Middle
            } else {
                Button::Left
            };
            let count = match arguments.number::<u32>("count")? {
                Some(count @ 1..=3) => count,
                Some(_) => return Err(invalid("--count is 1, 2 or 3.")),
                None if arguments.flag("double") => 2,
                None => 1,
            };
            host.click(name, coordinate(x)?, coordinate(y)?, button, count)?;
            Ok(json!({ "clicked": [coordinate(x)?, coordinate(y)?] }).into())
        }
        "move" => {
            let arguments = Arguments::parse(words, &[], &[])?;
            let (name, rest) = machine_and(&arguments)?;
            let [x, y] = rest else {
                return Err(invalid("Say where to: move <machine> <x> <y>."));
            };
            host.move_pointer(name, coordinate(x)?, coordinate(y)?)?;
            Ok(json!({ "moved": [coordinate(x)?, coordinate(y)?] }).into())
        }
        "drag" => {
            let arguments = Arguments::parse(words, &[], &["right", "middle"])?;
            let (name, rest) = machine_and(&arguments)?;
            let [x, y, to_x, to_y] = rest else {
                return Err(invalid(
                    "Say from where and to where: drag <machine> <x> <y> <to x> <to y>.",
                ));
            };
            let button = if arguments.flag("right") {
                Button::Right
            } else if arguments.flag("middle") {
                Button::Middle
            } else {
                Button::Left
            };
            let (from, to) = (
                (coordinate(x)?, coordinate(y)?),
                (coordinate(to_x)?, coordinate(to_y)?),
            );
            host.drag(name, from, to, button)?;
            Ok(json!({ "dragged": { "from": [from.0, from.1], "to": [to.0, to.1] } }).into())
        }
        "scroll" => {
            let arguments = Arguments::parse(words, &[], &[])?;
            let (name, rest) = machine_and(&arguments)?;
            // [<x> <y>] <direction> [<notches>]
            let (at, rest) = match rest {
                [x, y, rest @ ..] if Direction::from_name(x).is_none() => {
                    (Some((coordinate(x)?, coordinate(y)?)), rest)
                }
                rest => (None, rest),
            };
            let (direction, notches) = match rest {
                [direction] => (direction, None),
                [direction, notches] => (direction, Some(notches)),
                _ => {
                    return Err(invalid(
                        "Say which way: scroll <machine> [<x> <y>] up|down|left|right [<notches>].",
                    ));
                }
            };
            let direction = Direction::from_name(direction)
                .ok_or_else(|| invalid(format!("\"{direction}\" isn't up, down, left or right.")))?;
            let notches = match notches {
                None => 3,
                Some(notches) => match notches.parse::<u32>() {
                    Ok(count @ 1..=100) => count,
                    _ => return Err(invalid("The notches are a number from 1 to 100.")),
                },
            };
            host.scroll(name, at, direction, notches)?;
            Ok(json!({ "scrolled": direction }).into())
        }
        "type" => {
            let arguments = Arguments::parse(words, &[], &[])?;
            let (name, rest) = machine_and(&arguments)?;
            // What follows `--` is typed too, for text that starts with one.
            let text: Vec<&str> = rest.iter().chain(&arguments.rest).map(String::as_str).collect();
            if text.is_empty() {
                return Err(invalid("Say what to type: type <machine> <text>."));
            }
            host.type_text(name, &text.join(" "))?;
            Ok(json!({ "typed": text.join(" ").chars().count() }).into())
        }
        "key" => {
            let arguments = Arguments::parse(words, &[], &[])?;
            let (name, keys) = machine_and(&arguments)?;
            if keys.is_empty() {
                return Err(invalid("Say which keys: key <machine> Return, or ctrl+s."));
            }
            host.press_keys(name, keys)?;
            Ok(json!({ "pressed": keys }).into())
        }
        "open" => {
            let arguments = Arguments::parse(words, &[], &[])?;
            let (name, rest) = machine_and(&arguments)?;
            let [what] = rest else {
                return Err(invalid("Say what to open: an application, a file or a URL."));
            };
            host.open_in_guest(name, what)?;
            say(format!("Opened {what}."));
            Ok(json!({ "opened": what }).into())
        }
        "clipboard" => {
            let arguments = Arguments::parse(words, &[], &[])?;
            let (name, rest) = machine_and(&arguments)?;
            match rest {
                [] => {
                    let text = host.clipboard(name)?;
                    if json {
                    } else if std::io::stdout().is_terminal() {
                        // What a guest put there can be anything: a terminal isn't given orders.
                        for line in text.lines() {
                            println!("{}", plain(line));
                        }
                    } else {
                        print!("{text}");
                        _ = std::io::stdout().flush();
                    }
                    Ok(json!({ "text": text }).into())
                }
                [text] => {
                    host.set_clipboard(name, text)?;
                    Ok(json!({ "text": text }).into())
                }
                _ => Err(invalid("Put one text on the clipboard. Quote it.")),
            }
        }
        _ => unreachable!("only the desktop's commands are passed here"),
    }
}
