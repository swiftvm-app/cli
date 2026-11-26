//! The command: creates, starts and stops machines, and is the process of each one that runs.

mod console;
mod desktop;
mod disk;
mod guest;
mod help;
mod image;
mod mcp;
mod ui;
mod watch;

use std::collections::HashMap;
use std::io::Write;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use host::{CreateOptions, Error, ErrorCode, Host, Machine, MachineState, NAME, Progress, Result, ShareInfo};
use serde_json::json;
use ui::{Tone, Ui};

/// The version of what `--json` prints, and of `watch`'s lines: docs/api.md. Fields may be added
/// to a version; one that renames, removes or changes the meaning of anything is a new version.
const API: u32 = 1;

/// The words of a command line, sorted into what they are.
struct Arguments {
    positional: Vec<String>,
    /// Each option's values, in the order they were given.
    options: HashMap<String, Vec<String>>,
    /// What follows `--`.
    rest: Vec<String>,
}

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorCode::InvalidArgument, message)
}

impl Arguments {
    /// `valued` are the options that take a value, and `flags` those that don't.
    fn parse(words: &[String], valued: &[&str], flags: &[&str]) -> Result<Self> {
        let mut arguments = Self {
            positional: Vec::new(),
            options: HashMap::new(),
            rest: Vec::new(),
        };
        let mut words = words.iter();
        while let Some(word) = words.next() {
            if word == "--" {
                arguments.rest = words.cloned().collect();
                break;
            }
            let Some(option) = word.strip_prefix("--").filter(|option| !option.is_empty()) else {
                arguments.positional.push(word.clone());
                continue;
            };
            let (name, inline) = match option.split_once('=') {
                Some((name, value)) => (name, Some(value.to_string())),
                None => (option, None),
            };
            let value = if flags.contains(&name) && inline.is_none() {
                String::new()
            } else if valued.contains(&name) {
                match inline.or_else(|| words.next().cloned()) {
                    Some(value) => value,
                    None => return Err(invalid(format!("--{name} needs a value."))),
                }
            } else {
                return Err(invalid(format!("This command has no option --{name}.")));
            };
            arguments.options.entry(name.to_string()).or_default().push(value);
        }
        Ok(arguments)
    }

    fn flag(&self, name: &str) -> bool {
        self.options.contains_key(name)
    }

    /// The option's value, the last one if it was given more than once.
    fn value(&self, name: &str) -> Option<String> {
        self.options.get(name).and_then(|values| values.last()).cloned()
    }

    fn values(&self, name: &str) -> Vec<String> {
        self.options.get(name).cloned().unwrap_or_default()
    }

    fn number<T: std::str::FromStr>(&self, name: &str) -> Result<Option<T>> {
        match self.value(name) {
            None => Ok(None),
            Some(value) => value
                .parse()
                .map(Some)
                .map_err(|_| invalid(format!("--{name} takes a number, not \"{value}\"."))),
        }
    }

    /// The one thing a command is about: an image or a machine.
    fn subject(&self, what: &str) -> Result<&str> {
        match self.positional.as_slice() {
            [subject] => Ok(subject),
            [] => Err(invalid(format!("Say which {what}."))),
            _ => Err(invalid(format!("One {what} at a time."))),
        }
    }

    fn nothing_else(&self) -> Result<()> {
        match self.positional.first() {
            None => Ok(()),
            Some(word) => Err(invalid(format!("\"{word}\" isn't expected here."))),
        }
    }
}

/// A size as it is typed: a number of bytes, or one with K, M, G or T after it, which count in
/// steps of 1024.
fn parse_size(text: &str) -> Result<u64> {
    let unit = text.trim_start_matches(|character: char| character.is_ascii_digit() || character == '.');
    let number = &text[..text.len() - unit.len()];
    let scale: u64 = match unit
        .to_ascii_uppercase()
        .trim_end_matches("IB")
        .trim_end_matches('B')
    {
        "" => 1,
        "K" => 1 << 10,
        "M" => 1 << 20,
        "G" => 1 << 30,
        "T" => 1 << 40,
        _ => 0,
    };
    match number.parse::<f64>() {
        Ok(number) if scale > 0 && number.is_finite() => Ok((number * scale as f64) as u64),
        _ => Err(invalid(format!(
            "\"{text}\" isn't a size. Write one like 4G or 512M."
        ))),
    }
}

/// `text` without the characters that a terminal takes for orders, or that turn the direction of
/// the text around: what a guest says can be anything, and is shown as text.
fn plain(text: &str) -> String {
    text.chars()
        .filter(|character| {
            !character.is_control()
                && !matches!(character, '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
        .collect()
}

/// A size as it is shown: `512 MiB`, `1.5 GiB`.
fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut amount = bytes as f64;
    let mut unit = 0;
    while amount >= 1024.0 && unit + 1 < UNITS.len() {
        amount /= 1024.0;
        unit += 1;
    }
    let rounded = (amount * 10.0).round() / 10.0;
    if rounded.fract() == 0.0 {
        format!("{rounded:.0} {}", UNITS[unit])
    } else {
        format!("{rounded:.1} {}", UNITS[unit])
    }
}

/// How long ago a moment in seconds since 1970 was, roughly.
fn ago(seconds: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let past = now.saturating_sub(seconds);
    match past {
        0..60 => "just now".into(),
        60..7200 => format!("{} minutes ago", past / 60),
        7200..172_800 => format!("{} hours ago", past / 3600),
        _ => format!("{} days ago", past / 86400),
    }
}

/// Writes one JSON document as a line on the error output: how far a command has come, for a
/// program that runs it with `--json`. Standard output holds only the result.
fn json_note(value: serde_json::Value) {
    _ = writeln!(std::io::stderr().lock(), "{value}");
}

/// Shows how far a long task has come, on the error output so that the result stays clean.
struct Meter {
    /// The result is JSON, and so is each step: one line each ([`json_note`]).
    json: bool,
    ui: Ui,
    step: Option<&'static str>,
    shown: Instant,
    line_width: usize,
    detail_below: bool,
}

impl Meter {
    fn new(json: bool) -> Self {
        Self {
            json,
            ui: Ui::new(std::io::stderr(), !json),
            step: None,
            shown: Instant::now(),
            line_width: 0,
            detail_below: false,
        }
    }

    fn show(&mut self, progress: Progress) {
        let (step, detail) = match progress {
            Progress::Downloading {
                received,
                total: Some(total),
            } => (
                "Downloading the image",
                format!(
                    "{} of {} ({:.0}%)",
                    size(received),
                    size(total),
                    received as f64 / total.max(1) as f64 * 100.0
                ),
            ),
            Progress::Downloading {
                received,
                total: None,
            } => ("Downloading the image", size(received)),
            Progress::DownloadingEngine { received, total } => (
                "Downloading the engine",
                match total {
                    Some(total) => format!("{} of {}", size(received), size(total)),
                    None => size(received),
                },
            ),
            Progress::WritingDisk { fraction } => {
                ("Preparing the image", format!("{:.0}%", fraction * 100.0))
            }
            Progress::Compressing { fraction } => {
                ("Compressing the disk", format!("{:.0}%", fraction * 100.0))
            }
            Progress::Importing { fraction } => ("Importing the image", format!("{:.0}%", fraction * 100.0)),
        };
        let is_new = self.step != Some(step);
        self.step = Some(step);
        // Each new step, and then at most ten lines a second.
        let due = is_new || self.shown.elapsed() >= Duration::from_millis(100);
        if self.json {
            if due {
                self.shown = Instant::now();
                json_note(json!(progress));
            }
            return;
        }
        let mut output = std::io::stderr().lock();
        if let Some(columns) = self.ui.columns {
            if due {
                self.shown = Instant::now();
                self.draw(&mut output, columns, step, &detail, is_new);
            }
        } else if is_new {
            _ = writeln!(output, "{step}...");
        }
    }

    fn draw(&mut self, output: &mut impl Write, columns: usize, step: &str, detail: &str, is_new: bool) {
        if is_new {
            self.detail_below = false;
        }
        let width = step.len() + 2 + detail.len();
        if width >= columns && !self.detail_below {
            self.clear_line(output);
            _ = writeln!(output, "{}", self.ui.paint(Tone::Heading, step));
            self.detail_below = true;
            self.line_width = 0;
        }
        let (line, width) = if self.detail_below {
            // Progress details are ASCII numbers, units and punctuation.
            let detail = &detail[..detail.len().min(columns.saturating_sub(1))];
            (detail.to_string(), detail.len())
        } else {
            (format!("{}  {detail}", self.ui.paint(Tone::Heading, step)), width)
        };
        let padding = self.line_width.saturating_sub(width);
        _ = write!(output, "\r{line}{:padding$}", "");
        self.line_width = width;
    }

    fn clear_line(&self, output: &mut impl Write) {
        if self.line_width > 0 {
            _ = write!(output, "\r{:width$}\r", "", width = self.line_width);
        }
    }

    fn finish(&self) {
        self.clear_line(&mut std::io::stderr().lock());
    }
}

/// A share as people read it: where the guest sees which folder.
fn share_line(share: &ShareInfo) -> String {
    let mut line = format!("{} is {}", host::shared_path(&share.name), share.path.display());
    if share.read_only {
        line.push_str(", read-only");
    }
    if share.problem.is_some() {
        line.push_str(", which isn't there");
    }
    line
}

fn machine_row(ui: Ui, machine: &Machine) -> [String; 6] {
    let name = ui.paint(Tone::Bold, &machine.name);
    let state = ui.state(machine.state);
    if machine.state == MachineState::Broken {
        return [name, state, "-".into(), "-".into(), "-".into(), "-".into()];
    }
    [
        name,
        state,
        plain(&machine.image),
        machine.cpus.to_string(),
        size(machine.memory),
        format!("{} of {}", size(machine.disk_used), size(machine.disk_size)),
    ]
}

/// What a command that worked has to show: what `--json` prints, and the exit status.
struct Done {
    result: serde_json::Value,
    status: u8,
}

impl From<serde_json::Value> for Done {
    fn from(result: serde_json::Value) -> Self {
        Self { result, status: 0 }
    }
}

/// Runs a command.
fn run(command: &str, words: &[String], json: bool) -> Result<Done> {
    let say = |message| ui::success(json, message);
    let result = match command {
        "version" | "--version" => {
            if !json {
                println!("{}", Ui::stdout().version());
            }
            Ok(json!({ "name": NAME, "version": env!("CARGO_PKG_VERSION"), "api": API }))
        }
        "images" => return image::list(words, json),
        "image" => return image::run(words, json),
        "snapshot" | "snapshots" | "revert" | "delete-snapshot" => {
            return image::snapshots(command, words, json);
        }
        "create" => {
            let arguments = Arguments::parse(
                words,
                &[
                    "name", "cpus", "memory", "disk", "user", "password", "script", "share",
                ],
                &["nested"],
            )?;
            let options = CreateOptions {
                image: arguments.subject("image")?.to_string(),
                name: arguments.value("name"),
                cpus: arguments.number("cpus")?,
                memory: arguments.value("memory").as_deref().map(parse_size).transpose()?,
                disk: arguments.value("disk").as_deref().map(parse_size).transpose()?,
                user: arguments.value("user"),
                password: arguments.value("password"),
                scripts: arguments.values("script"),
                nested: arguments.flag("nested"),
                shares: arguments.values("share").into_iter().map(Into::into).collect(),
            };
            let host = Host::open()?;
            let mut meter = Meter::new(json);
            let created = host.create(options, &mut |progress| meter.show(progress));
            meter.finish();
            let machine = created?;
            say(format!("Created {}.", machine.name));
            Ok(json!({ "machine": machine }))
        }
        "list" | "ls" => {
            Arguments::parse(words, &[], &[])?.nothing_else()?;
            let machines = Host::open()?.list()?;
            if !json && machines.is_empty() {
                let ui = Ui::stdout();
                println!(
                    "No machines. Browse available images with {}.",
                    ui.paint(Tone::Bold, &format!("{NAME} images"))
                );
            } else if !json {
                let ui = Ui::stdout();
                ui.table(
                    ["NAME", "STATE", "IMAGE", "CPUS", "MEMORY", "DISK"],
                    machines.iter().map(|machine| machine_row(ui, machine)),
                );
                for machine in &machines {
                    if let Some(problem) = &machine.problem {
                        eprintln!(
                            "{}: {}",
                            Ui::stderr().paint(Tone::Warning, &machine.name),
                            plain(problem)
                        );
                    }
                }
            }
            Ok(json!({ "machines": machines }))
        }
        "start" => {
            let arguments = Arguments::parse(words, &[], &[])?;
            let name = arguments.subject("machine")?;
            let host = Host::open()?;
            let was_running = host.machine(name)?.state == MachineState::Running;
            let machine = host.start(name)?;
            say(if was_running {
                format!("{name} is running already.")
            } else {
                format!("Started {name}.")
            });
            Ok(json!({ "machine": machine }))
        }
        "stop" => {
            let arguments = Arguments::parse(words, &["timeout"], &["force"])?;
            let name = arguments.subject("machine")?;
            let timeout = arguments.number("timeout")?.map(Duration::from_secs);
            let host = Host::open()?;
            let was_running = host.machine(name)?.state == MachineState::Running;
            let machine = host.stop(name, arguments.flag("force"), timeout)?;
            say(if was_running {
                format!("Stopped {name}.")
            } else {
                format!("{name} isn't running.")
            });
            Ok(json!({ "machine": machine }))
        }
        "share" => {
            let arguments = Arguments::parse(words, &["name"], &["read-only"])?;
            let [name, folder] = arguments.positional.as_slice() else {
                return Err(invalid("Say which machine, and which folder to share with it."));
            };
            let share = Host::open()?.share(
                name,
                folder.as_ref(),
                arguments.value("name").as_deref(),
                arguments.flag("read-only"),
            )?;
            say(format!(
                "{name} shares {} at {}{}.",
                share.path.display(),
                host::shared_path(&share.name),
                if share.read_only { ", read-only" } else { "" }
            ));
            Ok(json!({ "share": share }))
        }
        "unshare" => {
            let arguments = Arguments::parse(words, &[], &[])?;
            let [name, share] = arguments.positional.as_slice() else {
                return Err(invalid(
                    "Say which machine, and the name of the folder it shares.",
                ));
            };
            Host::open()?.unshare(name, share)?;
            say(format!("{name} no longer shares {share}."));
            Ok(json!({ "unshared": share }))
        }
        "delete" => {
            let arguments = Arguments::parse(words, &[], &["force"])?;
            let name = arguments.subject("machine")?;
            Host::open()?.delete(name, arguments.flag("force"))?;
            say(format!("Deleted {name}."));
            Ok(json!({ "deleted": name }))
        }
        "console" => {
            let arguments = Arguments::parse(words, &[], &[])?;
            let name = arguments.subject("machine")?;
            let stream = Host::open()?.console(name)?;
            // What follows is the guest's text, with --json as without it.
            console::run(stream, name)
        }
        "info" | "wait" | "exec" | "cp" | "ssh" | "forward" | "tunnel" => {
            return guest::run(command, words, json);
        }
        "disk" => return disk::run(words, json),
        "vnc" | "screenshot" | "click" | "move" | "drag" | "scroll" | "type" | "key" | "open"
        | "clipboard" => {
            return desktop::run(command, words, json);
        }
        "mcp" => return mcp::run(words),
        "watch" => return watch::run(words, json),
        _ => Err(invalid(format!(
            "{NAME} has no command \"{command}\". `{NAME} help` lists the commands."
        ))),
    };
    result.map(Done::from)
}

fn main() -> ExitCode {
    let mut words: Vec<String> = std::env::args().skip(1).collect();
    if words.first().map(String::as_str) == Some(host::process::ARGUMENT) {
        return match words.get(1) {
            Some(directory) => host::process::run(directory.as_ref()),
            None => ExitCode::from(2),
        };
    }
    // Not among what follows `--`, which is the guest's.
    let own = words.iter().position(|word| word == "--").unwrap_or(words.len());
    let json = words[..own].iter().any(|word| word == "--json");
    let rest = words.split_off(own);
    words.retain(|word| word != "--json");
    words.extend(rest);
    let Some(command) = words.first().cloned() else {
        if json {
            let error = invalid(format!("Say which command. `{NAME} help` lists them."));
            println!("{}", json!({ "error": error }));
        } else {
            eprintln!("{}", help::overview(Ui::stderr()));
        }
        return ExitCode::from(2);
    };
    let result = if let Some(topic) = help::requested(&words, json) {
        help::page(Ui::new(std::io::stdout(), !json), topic).map(|page| {
            println!("{page}");
            serde_json::Value::Null.into()
        })
    } else {
        run(&command, &words[1..], json)
    };
    match result {
        Ok(done) => {
            if json && !done.result.is_null() {
                println!("{}", done.result);
            }
            ExitCode::from(done.status)
        }
        Err(error) => {
            if json {
                println!("{}", json!({ "error": error }));
            } else {
                ui::error(&error.message);
            }
            // A command that was written wrongly ends as the usage does.
            ExitCode::from(if error.code == ErrorCode::InvalidArgument {
                2
            } else {
                1
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_growing_progress_line_keeps_its_heading() {
        let mut meter = Meter::new(false);
        meter.ui.color = false;
        let mut output = Vec::new();
        let step = "Downloading the image";
        meter.draw(&mut output, 43, step, "0 B of 1 GiB (0%)", true);
        assert!(!output.contains(&b'\n'));

        output.clear();
        meter.draw(&mut output, 43, step, "101 MiB of 1 GiB (10%)", false);
        assert!(
            std::str::from_utf8(&output)
                .unwrap()
                .contains("Downloading the image\n")
        );

        output.clear();
        meter.draw(&mut output, 43, step, "1 GiB of 1 GiB (100%)", false);
        assert!(std::str::from_utf8(&output).unwrap().starts_with("\r1 GiB"));
    }
}
