//! The command: creates, starts and stops machines, and is the process of each one that runs.

mod console;
mod disk;
mod guest;

use std::collections::HashMap;
use std::io::{IsTerminal, Write};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use host::{CreateOptions, Error, ErrorCode, Host, Machine, MachineState, NAME, Progress, Result};
use serde_json::json;

fn usage() -> String {
    format!(
        "usage: {NAME} <command> [options]

  images                     The systems a machine can be created with.
  create <image>             Create a machine. It isn't started.
      --name <name>          Its name. Without one it is named after the image.
      --cpus <count>         Processors. 4 unless this computer has fewer.
      --memory <size>        Memory, such as 4G or 512M. 4G unless said.
      --disk <size>          The size of its disk. 32G unless said.
      --user <name>          The account to create. This computer's user name unless said.
      --password <password>  The account's password. Without one it has none to sign in with.
      --script <script>      Set something up once the first start is over: {scripts}, or the
                             path of a script. Can be given more than once.
  list                       The machines and whether they are running.
  start <machine>            Start a machine. Returns once it has started to boot.
  stop <machine>             Shut a machine down, and cut its power if that takes too long.
      --timeout <seconds>    How long the guest gets to shut down. 30 unless said.
      --force                Cut the power at once.
  delete <machine>           Delete a machine and its disk.
      --force                Also when it is running.
  console <machine>          Connect this terminal to a machine's serial console.
  info <machine>             A machine, and what its guest says about itself.
  wait <machine>             Wait until the guest is up and its setup scripts have run.
      --timeout <seconds>    How long to wait. As long as it takes unless said.
  exec <machine> -- <command> [arguments]
                             Run a command in the guest. Input that is piped in is passed on.
                             Ends with the command's exit status.
      --user <name>          The account to run it as. The machine's own unless said.
      --cwd <folder>         Where to run it. The account's home folder unless said.
      --env <name=value>     Add to its environment. Can be given more than once.
      --timeout <seconds>    End it after this long, with exit status 124.
  cp <from> <to>             Copy a file or folder into or out of a guest, as cp -R does. The
                             guest's side is written <machine>:<path>, from the home folder.
      --user <name>          The account that the copy is made as.
  ssh <machine> [-- <arguments for ssh>]
                             Sign in to the guest with ssh, through its agent.
  forward <machine> <port>[:<guest port>]...
                             Pass connections to ports of this computer's localhost on to ports
                             of the guest, until stopped.
  tunnel <machine> <port>    Connect this program's input and output to a port of the guest, as
                             ssh's ProxyCommand does.
  disk info <machine>        The layers of a machine's disk, and what each holds. Takes the path
                             of a .layer file too.
  disk check <machine>       Read every layer of a stopped machine's disk back and check it. Ends
                             with status 1 if anything is wrong.
  version

Every command takes --json, and then prints one JSON document: its result, or
{{\"error\": {{\"code\": ..., \"message\": ...}}}}.",
        scripts = host::setup_scripts().join(", ")
    )
}

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

/// Prints `rows` as columns two spaces apart, each as wide as its widest cell.
fn print_table(rows: &[Vec<String>]) {
    let widths: Vec<usize> = (0..rows[0].len())
        .map(|column| {
            rows.iter()
                .map(|row| row[column].chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    for row in rows {
        let line: Vec<String> = row
            .iter()
            .zip(&widths)
            .map(|(cell, width)| format!("{cell:<width$}"))
            .collect();
        println!("{}", line.join("  ").trim_end());
    }
}

/// Shows how far a creation has come, on the error output so that the result stays clean.
struct Meter {
    /// A terminal gets a line that is kept up to date, anything else one line for each step.
    live: bool,
    step: Option<&'static str>,
    shown: Instant,
}

impl Meter {
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
            Progress::WritingDisk { fraction } => {
                ("Preparing the image", format!("{:.0}%", fraction * 100.0))
            }
        };
        let is_new = self.step != Some(step);
        self.step = Some(step);
        let mut output = std::io::stderr().lock();
        if !self.live {
            if is_new {
                _ = writeln!(output, "{step}...");
            }
        } else if is_new || self.shown.elapsed() >= Duration::from_millis(100) {
            self.shown = Instant::now();
            _ = write!(output, "\r\x1b[K{step}: {detail}");
        }
    }

    fn finish(&self) {
        if self.live && self.step.is_some() {
            eprint!("\r\x1b[K");
        }
    }
}

fn machine_row(machine: &Machine) -> Vec<String> {
    let state = match machine.state {
        MachineState::Running => "running",
        MachineState::Stopped => "stopped",
        MachineState::Broken => "broken",
    };
    if machine.state == MachineState::Broken {
        return [&machine.name, state, "-", "-", "-", "-"]
            .map(String::from)
            .to_vec();
    }
    vec![
        machine.name.clone(),
        state.to_string(),
        machine.image.clone(),
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
    let say = |text: String| {
        if !json {
            println!("{text}");
        }
    };
    let result = match command {
        "images" => {
            Arguments::parse(words, &[], &[])?.nothing_else()?;
            let images = Host::open()?.images();
            if !json {
                let mut rows = vec![["IMAGE", "SYSTEM", "DOWNLOADED"].map(String::from).to_vec()];
                rows.extend(images.iter().map(|image| {
                    vec![
                        image.id.clone(),
                        image.name.clone(),
                        if image.cached { "yes" } else { "no" }.to_string(),
                    ]
                }));
                print_table(&rows);
            }
            Ok(json!({ "images": images }))
        }
        "create" => {
            let arguments = Arguments::parse(
                words,
                &["name", "cpus", "memory", "disk", "user", "password", "script"],
                &[],
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
            };
            let host = Host::open()?;
            let mut meter = Meter {
                live: std::io::stderr().is_terminal(),
                step: None,
                shown: Instant::now(),
            };
            let created = host.create(options, &mut |progress| {
                if !json {
                    meter.show(progress);
                }
            });
            meter.finish();
            let machine = created?;
            say(format!("Created {}.", machine.name));
            Ok(json!({ "machine": machine }))
        }
        "list" | "ls" => {
            Arguments::parse(words, &[], &[])?.nothing_else()?;
            let machines = Host::open()?.list()?;
            if !json && machines.is_empty() {
                println!("No machines.");
            } else if !json {
                let mut rows = vec![
                    ["NAME", "STATE", "IMAGE", "CPUS", "MEMORY", "DISK"]
                        .map(String::from)
                        .to_vec(),
                ];
                rows.extend(machines.iter().map(machine_row));
                print_table(&rows);
                for machine in &machines {
                    if let Some(problem) = &machine.problem {
                        eprintln!("{}: {problem}", machine.name);
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
        eprintln!("{}", usage());
        return ExitCode::from(2);
    };
    match command.as_str() {
        "help" | "--help" | "-h" => {
            println!("{}", usage());
            return ExitCode::SUCCESS;
        }
        "version" | "--version" if json => {
            println!(
                "{}",
                json!({ "name": NAME, "version": env!("CARGO_PKG_VERSION") })
            );
            return ExitCode::SUCCESS;
        }
        "version" | "--version" => {
            println!("{NAME} {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        _ => {}
    }
    match run(&command, &words[1..], json) {
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
                eprintln!("{NAME}: {}", plain(&error.message));
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
