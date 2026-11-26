//! Exercise the human output through the CLI, with pipes and real terminal descriptors.

use std::fs::File;
use std::io::Read;
use std::os::fd::FromRawFd;
use std::process::{Command, Output, Stdio};
use std::ptr;

use textwrap::core::display_width;

fn command(args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_cli"));
    let store = std::env::temp_dir().join(format!("cli-output-{}", std::process::id()));
    cmd.args(args)
        .env(proto::variable("HOME"), store.join("home"))
        .env(proto::variable("CACHE"), store.join("cache"))
        .env("TERM", "xterm-256color")
        .env_remove("NO_COLOR");
    cmd
}

enum Stream {
    Stdout,
    Stderr,
}

fn terminal(mut cmd: Command, stream: Stream, columns: u16) -> (Output, String) {
    let (mut master, mut slave) = (-1, -1);
    let mut size = libc::winsize {
        ws_row: 30,
        ws_col: columns,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // Safety: openpty initializes both descriptors on success. Each is owned by exactly one File.
    let mut reader;
    let writer;
    unsafe {
        assert_eq!(
            libc::openpty(
                &mut master,
                &mut slave,
                ptr::null_mut(),
                ptr::null_mut(),
                &raw mut size
            ),
            0
        );
        reader = File::from_raw_fd(master);
        writer = File::from_raw_fd(slave);
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    match stream {
        Stream::Stdout => cmd.stdout(writer),
        Stream::Stderr => cmd.stderr(writer),
    };
    let child = cmd.spawn().unwrap();
    drop(cmd);
    let mut bytes = Vec::new();
    if let Err(error) = reader.read_to_end(&mut bytes) {
        // Linux signals the close of the last slave descriptor with EIO.
        assert_eq!(error.raw_os_error(), Some(libc::EIO));
    }
    (
        child.wait_with_output().unwrap(),
        String::from_utf8(bytes).unwrap(),
    )
}

#[test]
fn help_works_at_each_command_depth_and_respects_guest_arguments() {
    for (topic, expected) in [
        (vec!["create"], "--script <script>"),
        (vec!["image"], "Actions"),
        (vec!["image", "save"], "--delete"),
        (vec!["disk", "check"], ".layer"),
        (vec!["mcp"], "claude mcp add"),
    ] {
        let by_name = command(&[&["help"][..], &topic].concat()).output().unwrap();
        let by_flag = command(&[&topic[..], &["--help"]].concat()).output().unwrap();
        assert!(by_name.status.success() && by_flag.status.success());
        assert_eq!(by_name.stdout, by_flag.stdout);
        let text = String::from_utf8(by_name.stdout).unwrap();
        assert!(text.contains(expected) && !text.contains('\x1b'));
    }
    for args in [
        &["exec", "absent", "--", "ls", "--help"][..],
        &["create", "--password", "--help"],
        &["type", "absent", "-h"],
    ] {
        let output = command(args).output().unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty() && !output.stderr.is_empty());
    }
}

#[test]
fn color_follows_each_stream_and_can_be_disabled() {
    let (output, help) = terminal(command(&[]), Stream::Stderr, 80);
    assert_eq!(output.status.code(), Some(2));
    assert!(help.contains("\x1b[") && output.stdout.is_empty());
    let (output, stdout) = terminal(command(&["unknown"]), Stream::Stdout, 80);
    assert!(stdout.is_empty());
    assert!(!output.stderr.is_empty() && !output.stderr.contains(&0x1b));

    for (key, value) in [("NO_COLOR", "1"), ("TERM", "dumb")] {
        let mut cmd = command(&["--help"]);
        cmd.env(key, value);
        let (output, help) = terminal(cmd, Stream::Stdout, 80);
        assert!(output.status.success() && !help.contains('\x1b'));
        assert!(help.contains("Commands"));
    }
    let (output, json) = terminal(command(&["images", "--json"]), Stream::Stdout, 80);
    assert!(output.status.success() && output.stderr.is_empty());
    assert!(serde_json::from_str::<serde_json::Value>(&json).unwrap()["images"].is_array());

    let (output, json) = terminal(command(&["--json", "create", "--help"]), Stream::Stdout, 80);
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&json).unwrap()["error"]["code"],
        "invalid-argument"
    );
}

#[test]
fn narrow_terminals_wrap_help_and_keep_table_values_with_their_labels() {
    let (_, help) = terminal(command(&["create", "--help"]), Stream::Stdout, 40);
    assert!(help.contains("\x1b[") && help.contains("--password"));
    assert!(help.lines().all(|line| display_width(line) <= 40));
    let example = help.rsplit("\r\n\r\n").next().unwrap();
    let lines: Vec<_> = example.lines().skip(1).collect();
    assert!(lines.len() > 1);
    assert!(lines[..lines.len() - 1].iter().all(|line| line.ends_with(" \\")));

    let (_, images) = terminal(command(&["images"]), Stream::Stdout, 38);
    assert!(images.contains("debian-13") && images.contains("Ubuntu 24.04 LTS"));
    assert!(images.matches("DESCRIPTION").count() >= 2);
    assert!(images.lines().all(|line| display_width(line) <= 38));
}
