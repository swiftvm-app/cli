//! The command index and the help page for each command.

use host::{NAME, Result};

use crate::invalid;
use crate::ui::{Tone, Ui};

/// Name, arguments, and description. The index and command pages share these entries.
type Entry = (&'static str, &'static str, &'static str);

const GROUPS: &[(&str, &[Entry])] = &[
    (
        "Machines",
        &[
            (
                "create",
                "<image> [options]",
                "Create a stopped machine from a catalog or saved image.",
            ),
            (
                "list",
                "",
                "List machines, their states and the resources they use. Also available as ls.",
            ),
            (
                "info",
                "<machine>",
                "Show the machine's configuration and details reported by its guest.",
            ),
            (
                "start",
                "<machine>",
                "Start the machine and return once it begins to boot.",
            ),
            (
                "wait",
                "<machine> [options]",
                "Wait for the guest to boot and finish its setup scripts.",
            ),
            (
                "stop",
                "<machine> [options]",
                "Shut down the guest. If it exceeds the timeout, cut the power.",
            ),
            (
                "delete",
                "<machine> [options]",
                "Delete the machine and its disk.",
            ),
            (
                "watch",
                "",
                "Report changes to machines until interrupted. With --json, emit the full list at startup and on each change.",
            ),
        ],
    ),
    (
        "Guest access",
        &[
            (
                "ssh",
                "<machine> [-- <ssh arguments>]",
                "Sign in with SSH through the guest agent.",
            ),
            (
                "exec",
                "<machine> [options] -- <command> [arguments]",
                "Run a guest command, pass piped input through and return its exit status.",
            ),
            (
                "console",
                "<machine>",
                "Connect to the serial console. Press Control-] to leave.",
            ),
            (
                "cp",
                "<from> <to> [options]",
                "Copy a file or directory between host and guest. Use <machine>:<path> for the guest side. Relative guest paths start in the account's home directory.",
            ),
            (
                "forward",
                "<machine> <port>[:<guest-port>]...",
                "Forward connections from localhost to guest ports until interrupted.",
            ),
            (
                "tunnel",
                "<machine> <port>",
                "Connect standard input and output to a guest port. SSH can use this as its ProxyCommand.",
            ),
        ],
    ),
    (
        "Shared folders",
        &[
            (
                "share",
                "<machine> <folder> [options]",
                "Mount a host folder at /mnt/shared/<name>. A running guest gets the share immediately. Files it writes belong to the host user.",
            ),
            (
                "unshare",
                "<machine> <name>",
                "Stop sharing a folder with the guest.",
            ),
        ],
    ),
    (
        "Images & disks",
        &[
            (
                "images",
                "[--all]",
                "List catalog images and images saved from machines.",
            ),
            (
                "image",
                "<action>",
                "Save, rename, transfer and delete reusable machine images.",
            ),
            (
                "disk",
                "<action>",
                "Inspect the layers of a disk or check their integrity.",
            ),
        ],
    ),
    (
        "Snapshots",
        &[
            (
                "snapshot",
                "<machine> <name>",
                "Take a disk snapshot. A running machine shuts down for the snapshot and restarts afterward.",
            ),
            (
                "snapshots",
                "<machine>",
                "List the machine's snapshots and the space each one uses.",
            ),
            (
                "revert",
                "<machine> <name>",
                "Restore the disk to a snapshot. Later writes are lost. A running machine shuts down for the restore and restarts afterward.",
            ),
            ("delete-snapshot", "<machine> <name>", "Delete a disk snapshot."),
        ],
    ),
    (
        "Desktop",
        &[
            (
                "vnc",
                "<machine>",
                "Print the path to the guest's VNC socket. Only the host user can connect. No password is required.",
            ),
            (
                "screenshot",
                "<machine> [<file> | -]",
                "Save the full guest screen as a PNG. By default, print its path in the runtime folder. Use - to write the PNG to standard output.",
            ),
            (
                "click",
                "<machine> <x> <y> [options]",
                "Click a pixel measured from the top left of the screen.",
            ),
            (
                "move",
                "<machine> <x> <y>",
                "Move the pointer to a screen coordinate.",
            ),
            (
                "drag",
                "<machine> <x> <y> <to-x> <to-y> [options]",
                "Drag the pointer between two screen coordinates.",
            ),
            (
                "scroll",
                "<machine> [<x> <y>] up|down|left|right [<notches>]",
                "Scroll the wheel. Defaults to three notches at the center of the screen.",
            ),
            (
                "type",
                "<machine> <text>",
                "Type text at the current keyboard focus.",
            ),
            (
                "key",
                "<machine> <key>...",
                "Press keys in order. Examples include Return, Escape, Tab, F5, ctrl+s and ctrl+shift+t.",
            ),
            (
                "open",
                "<machine> <target>",
                "Open an application, file or URL on the guest desktop.",
            ),
            (
                "clipboard",
                "<machine> [<text>]",
                "Read the guest clipboard or replace it with the given text.",
            ),
        ],
    ),
    (
        "Tools",
        &[
            (
                "mcp",
                "",
                "Serve the Model Context Protocol over standard input and output.",
            ),
            (
                "version",
                "",
                "Print the installed version. With --json, include the API version.",
            ),
            (
                "help",
                "[command] [action]",
                "Show the command index or help for one command.",
            ),
        ],
    ),
];

fn actions(command: &str) -> &'static [Entry] {
    match command {
        "image" => &[
            (
                "save",
                "<machine> <name> [--delete]",
                "Save a machine as an image. Each new machine gets its own hostname, machine ID and SSH keys. A running source machine shuts down and restarts unless deleted.",
            ),
            (
                "delete",
                "<image>",
                "Delete a saved image once no machines use it. Also available as image rm.",
            ),
            ("rename", "<image> <name>", "Give a saved image a new name."),
            ("export", "<image> <file>", "Write a saved image to one file."),
            ("import", "<file> [options]", "Import an exported image."),
        ],
        "disk" => &[
            (
                "info",
                "<machine|layer>",
                "Show disk layers and what each holds. Accepts a machine name or a .layer file.",
            ),
            (
                "check",
                "<machine|layer>",
                "Check every layer of a stopped machine or a .layer file. Exit status 1 means the check found a problem.",
            ),
        ],
        _ => &[],
    }
}

fn options(topic: &str) -> &'static [(&'static str, &'static str)] {
    match topic {
        "images" => &[("--all", "Include automatic images created by setup scripts.")],
        "create" => &[
            ("--name <name>", "Machine name. Defaults to the image name."),
            (
                "--cpus <count>",
                "Virtual CPUs. Defaults to 4 or the host's available count, whichever is smaller.",
            ),
            ("--memory <size>", "Guest memory, such as 4G or 512M. Default 4G."),
            ("--disk <size>", "Virtual disk capacity. Default 32G."),
            ("--user <name>", "Guest account. Defaults to your host username."),
            (
                "--password <password>",
                "Set the guest account's password. Password sign-in is disabled by default.",
            ),
            (
                "--script <script>",
                "Run a built-in script or a script file during first boot. Repeat to run several in order.",
            ),
            (
                "--share <folder>",
                "Mount a host folder in the guest. Repeat to share more folders.",
            ),
            (
                "--nested",
                "Enable nested virtualization on Macs with M3 or later and macOS 15 or later.",
            ),
        ],
        "stop" => &[
            (
                "--timeout <seconds>",
                "Time to allow for shutdown. Default 30 seconds.",
            ),
            ("--force", "Cut the power immediately."),
        ],
        "wait" => &[("--timeout <seconds>", "Limit the wait. The default is unlimited.")],
        "delete" => &[("--force", "Allow deletion of a running machine.")],
        "share" => &[
            (
                "--name <name>",
                "Mount name in the guest. Defaults to the folder's name.",
            ),
            (
                "--read-only",
                "Allow the guest to read the folder without changing it.",
            ),
        ],
        "exec" => &[
            (
                "--user <name>",
                "Guest account to run as. Defaults to the machine's account.",
            ),
            (
                "--cwd <folder>",
                "Working directory. Defaults to the account's home folder.",
            ),
            (
                "--env <name=value>",
                "Set an environment variable. Repeat to set more.",
            ),
            (
                "--timeout <seconds>",
                "End the command after this interval with exit status 124.",
            ),
        ],
        "cp" => &[("--user <name>", "Guest account to use for the copy.")],
        "image save" => &[("--delete", "Delete the source machine after saving the image.")],
        "image import" => &[("--name <name>", "Override the name stored in the exported image.")],
        "click" => &[
            ("--right", "Use the right mouse button."),
            ("--middle", "Use the middle mouse button."),
            ("--double", "Click twice."),
            ("--count <n>", "Click one, two or three times."),
        ],
        "drag" => &[
            ("--right", "Drag with the right mouse button."),
            ("--middle", "Drag with the middle mouse button."),
        ],
        _ => &[],
    }
}

fn invocation(ui: Ui, command: &str, args: &str) -> String {
    let command = ui.paint(Tone::Bold, format!("{NAME} {command}").trim_end());
    ui.command(&format!("{command} {args}"))
}

fn section(ui: Ui, title: &str, body: String) -> String {
    format!("{}\n{body}", ui.paint(Tone::Heading, title))
}

pub fn overview(ui: Ui) -> String {
    let groups: Vec<_> = GROUPS
        .iter()
        .map(|(label, commands)| {
            (
                *label,
                commands
                    .iter()
                    .map(|(name, _, _)| *name)
                    .collect::<Vec<_>>()
                    .join("  "),
            )
        })
        .collect();
    let examples = [
        ("create", "debian-13 --name dev"),
        ("start", "dev"),
        ("wait", "dev"),
        ("ssh", "dev"),
    ]
    .map(|(cmd, args)| invocation(ui, cmd, args))
    .join("\n");
    [
        format!(
            "{}\n{}",
            ui.version(),
            ui.wrap("Run Linux virtual machines on macOS and Linux.", "", "")
        ),
        section(
            ui,
            "Usage",
            [
                invocation(ui, "", "<command> [options]"),
                invocation(ui, "help", "<command> [action]"),
            ]
            .join("\n"),
        ),
        section(ui, "Commands", ui.pairs(&groups)),
        section(ui, "Quick start", examples),
        section(
            ui,
            "Options",
            ui.pairs(&[
                (
                    "-h, --help",
                    "Show the index. Place directly after a command or action for its help.",
                ),
                (
                    "--version",
                    "Use in place of a command to print the installed version.",
                ),
                (
                    "--json",
                    "Write JSON results to stdout and progress events to stderr.",
                ),
            ]),
        ),
    ]
    .join("\n\n")
}

/// Look for a help flag on our side of `--`, skipping values such as `--password --help`.
pub fn requested(words: &[String], json: bool) -> Option<&[String]> {
    let own = &words[..words.iter().position(|w| w == "--").unwrap_or(words.len())];
    let (command, rest) = own.split_first()?;
    if matches!(command.as_str(), "help" | "-h" | "--help") {
        if let Some((last, topic)) = rest.split_last()
            && matches!(last.as_str(), "--help" | "-h")
        {
            return Some(topic);
        }
        return Some(rest);
    }
    // JSON commands keep their documented result or argument error on stdout.
    if json {
        return None;
    }
    let nested = !actions(command).is_empty() && rest.first().is_some_and(|w| !w.starts_with('-'));
    let (topic, args) = own.split_at(if nested { 2 } else { 1 });
    // A lone -h requests help. It is also valid positional text, e.g. `type dev -h`.
    if args == ["-h"] {
        return Some(topic);
    }
    if args.iter().any(|arg| arg == "--help") {
        let options = options(&topic.join(" "));
        let mut words = args.iter();
        while let Some(word) = words.next() {
            if word == "--help" {
                return Some(topic);
            }
            if options
                .iter()
                .any(|(usage, _)| usage.split_once(' ').is_some_and(|(flag, _)| flag == word))
            {
                words.next();
            }
        }
    }
    None
}

pub fn page(ui: Ui, topic: &[String]) -> Result<String> {
    if topic.is_empty() {
        return Ok(overview(ui));
    }
    let entry = match topic {
        [name] => {
            let name = match name.as_str() {
                "ls" => "list",
                "--version" => "version",
                other => other,
            };
            GROUPS
                .iter()
                .flat_map(|(_, entries)| *entries)
                .find(|(key, _, _)| *key == name)
        }
        [parent, name] => {
            let name = if parent == "image" && name == "rm" {
                "delete"
            } else {
                name
            };
            actions(parent).iter().find(|(key, _, _)| *key == name)
        }
        _ => None,
    };
    let Some(&(name, args, about)) = entry else {
        return Err(invalid(format!(
            "Unknown help topic \"{}\". Run `{NAME} help` to list commands.",
            topic.join(" ")
        )));
    };
    let name = if topic.len() == 2 {
        format!("{} {name}", topic[0])
    } else {
        name.to_string()
    };
    let mut sections = vec![
        section(ui, "Usage", invocation(ui, &name, args)),
        ui.wrap(about, "", ""),
    ];
    let children = actions(&name);
    if !children.is_empty() {
        let rows: Vec<_> = children
            .iter()
            .map(|(action, _, about)| (*action, *about))
            .collect();
        sections.push(section(ui, "Actions", ui.pairs(&rows)));
        sections.push(section(
            ui,
            "Example",
            invocation(ui, "help", &format!("{name} {}", children[0].0)),
        ));
    }
    let options = options(&name);
    if !options.is_empty() {
        sections.push(section(ui, "Options", ui.pairs(options)));
    }
    match name.as_str() {
        "create" => {
            sections.push(section(
                ui,
                "Built-in scripts",
                ui.wrap(&host::setup_scripts().join(", "), "  ", "  "),
            ));
            sections.push(ui.wrap(
                "Catalog machines reuse an automatic image when the setup scripts and account match.",
                "",
                "",
            ));
            sections.push(section(
                ui,
                "Example",
                invocation(ui, "create", "debian-13 --name dev --memory 2G"),
            ));
        }
        "exec" => sections.push(section(ui, "Example", invocation(ui, "exec", "dev -- uname -a"))),
        "mcp" => {
            sections.push(section(
                ui,
                "Claude Code",
                ui.command(&format!("claude mcp add {NAME} -- {NAME} mcp")),
            ));
            sections.push(section(
                ui,
                "Client configuration",
                ui.wrap(
                    &format!(r#"{{"mcpServers": {{"{NAME}": {{"command": "{NAME}", "args": ["mcp"]}}}}}}"#),
                    "  ",
                    "    ",
                ),
            ));
            sections.push(ui.wrap("Start with vm_list. Desktop tools need a machine created with a desktop script such as --script xfce.", "", ""));
        }
        _ => {}
    }
    Ok(sections.join("\n\n"))
}
