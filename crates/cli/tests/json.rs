//! The shapes of what `--json` prints, as docs/api.md writes them down. Each test runs the program
//! with a store and a cache of its own, which hold nothing.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use serde_json::{Value, json};

const PROGRAM: &str = env!("CARGO_BIN_EXE_cli");

/// A store and a cache that nothing else uses.
fn folder(test: &str) -> PathBuf {
    let folder = std::env::temp_dir().join(format!("cli-json-{test}-{}", std::process::id()));
    std::fs::create_dir_all(&folder).unwrap();
    folder
}

fn command(folder: &std::path::Path, words: &[&str]) -> Command {
    let mut command = Command::new(PROGRAM);
    command
        .args(words)
        .env(proto::variable("HOME"), folder.join("home"))
        .env(proto::variable("CACHE"), folder.join("cache"));
    command
}

fn run(test: &str, words: &[&str]) -> (Output, Value) {
    let folder = folder(test);
    let output = command(&folder, words).output().unwrap();
    _ = std::fs::remove_dir_all(&folder);
    let value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("{words:?} printed no JSON ({error}): {:?}", output.stdout));
    (output, value)
}

#[test]
fn version_says_which_api() {
    let (output, value) = run("version", &["version", "--json"]);
    assert!(output.status.success());
    assert_eq!(value["name"], proto::NAME);
    assert_eq!(value["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(value["api"], 1);
}

#[test]
fn an_empty_store_has_no_machines() {
    let (output, value) = run("list", &["list", "--json"]);
    assert!(output.status.success());
    assert_eq!(value, json!({ "machines": [] }));
    assert!(output.stderr.is_empty());
}

#[test]
fn images_are_listed_with_their_kind() {
    let (output, value) = run("images", &["images", "--json"]);
    assert!(output.status.success());
    let images = value["images"].as_array().unwrap();
    let debian = images.iter().find(|image| image["id"] == "debian-13").unwrap();
    assert_eq!(debian["kind"], "catalog");
    assert_eq!(debian["cached"], false);
    assert!(debian["name"].is_string());
}

#[test]
fn errors_have_a_code_and_a_message() {
    let (output, value) = run("missing", &["start", "nothing", "--json"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(value["error"]["code"], "not-found");
    assert!(value["error"]["message"].is_string());

    let (output, value) = run("invalid", &["list", "--json", "--wrong"]);
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(value["error"]["code"], "invalid-argument");

    let (output, value) = run("unknown", &["nonsense", "--json"]);
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(value["error"]["code"], "invalid-argument");

    let (output, value) = run("nothing", &["--json"]);
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(value["error"]["code"], "invalid-argument");
    assert!(output.stderr.is_empty());
}

#[test]
fn watch_says_what_there_is_at_once() {
    let folder = folder("watch");
    let mut child = command(&folder, &["watch", "--json"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.as_mut().unwrap())
        .read_line(&mut line)
        .unwrap();
    _ = child.kill();
    _ = child.wait();
    _ = std::fs::remove_dir_all(&folder);
    assert_eq!(
        serde_json::from_str::<Value>(&line).unwrap(),
        json!({ "machines": [] })
    );
}

#[test]
fn watch_ends_when_nobody_reads() {
    let folder = folder("watch-closed");
    let mut child = command(&folder, &["watch", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // Read its first line, then go: nothing changes in an empty store, so it has to notice.
    let mut line = String::new();
    BufReader::new(child.stdout.as_mut().unwrap())
        .read_line(&mut line)
        .unwrap();
    drop(child.stdout.take());
    let ended = (0..50).any(|_| {
        std::thread::sleep(std::time::Duration::from_millis(100));
        child.try_wait().unwrap().is_some()
    });
    _ = child.kill();
    _ = child.wait();
    _ = std::fs::remove_dir_all(&folder);
    assert!(ended, "watch kept running with nobody reading");
}

#[test]
fn progress_is_an_object_named_by_its_step() {
    let lines = [
        (
            host::Progress::Downloading {
                received: 1,
                total: Some(2),
            },
            json!({ "progress": "downloading", "received": 1, "total": 2 }),
        ),
        (
            host::Progress::DownloadingEngine {
                received: 1,
                total: None,
            },
            json!({ "progress": "downloading-engine", "received": 1, "total": null }),
        ),
        (
            host::Progress::WritingDisk { fraction: 0.5 },
            json!({ "progress": "writing-disk", "fraction": 0.5 }),
        ),
        (
            host::Progress::Compressing { fraction: 0.5 },
            json!({ "progress": "compressing", "fraction": 0.5 }),
        ),
        (
            host::Progress::Importing { fraction: 0.5 },
            json!({ "progress": "importing", "fraction": 0.5 }),
        ),
    ];
    for (progress, expected) in lines {
        assert_eq!(serde_json::to_value(progress).unwrap(), expected);
    }
}

#[test]
fn folders_are_shared_and_unshared() {
    let folder = folder("share");
    // A stopped machine, as far as its settings go: sharing needs nothing else.
    let machine = folder.join("home/machines/m");
    std::fs::create_dir_all(&machine).unwrap();
    host::Config {
        version: 2,
        id: "0".repeat(32),
        name: "m".into(),
        image: "debian-13".into(),
        arch: "arm64".into(),
        cpus: 1,
        memory: 1 << 30,
        disk: host::DiskConfig {
            file: "disk.layer".into(),
            format: host::DiskFormat::Layer,
            size: 1 << 30,
        },
        mac_address: "02:00:00:00:00:01".into(),
        user: "username".into(),
        created: 0,
        snapshots: Vec::new(),
        automatic: None,
        nested: false,
        shares: Vec::new(),
    }
    .save(&machine)
    .unwrap();
    let projects = folder.join("My Projects");
    std::fs::create_dir_all(&projects).unwrap();
    let projects = projects.canonicalize().unwrap();
    let json = |words: &[&str]| {
        let output = command(&folder, words).output().unwrap();
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        (output.status.code(), value)
    };
    let path = projects.to_str().unwrap();

    let (status, value) = json(&["share", "m", path, "--read-only", "--json"]);
    assert_eq!(status, Some(0), "{value}");
    assert_eq!(
        value,
        json!({ "share": { "name": "My-Projects", "path": path, "read_only": true } })
    );
    let (status, value) = json(&["share", "m", path, "--json"]);
    assert_eq!(
        (status, &value["error"]["code"]),
        (Some(1), &json!("already-exists"))
    );
    let (status, _) = json(&["share", "m", path, "--name", "src", "--json"]);
    assert_eq!(status, Some(0));
    let (status, value) = json(&["share", "m", path, "--name", "a/b", "--json"]);
    assert_eq!(
        (status, &value["error"]["code"]),
        (Some(2), &json!("invalid-argument"))
    );
    let (status, value) = json(&["share", "m", "/nonexistent-folder", "--json"]);
    assert_eq!((status, &value["error"]["code"]), (Some(1), &json!("not-found")));
    let (status, value) = json(&["share", "nothing", path, "--json"]);
    assert_eq!((status, &value["error"]["code"]), (Some(1), &json!("not-found")));

    let (_, value) = json(&["list", "--json"]);
    assert_eq!(
        value["machines"][0]["shares"],
        json!([
            { "name": "My-Projects", "path": path, "read_only": true },
            { "name": "src", "path": path, "read_only": false },
        ])
    );
    std::fs::remove_dir_all(&projects).unwrap();
    let (_, value) = json(&["list", "--json"]);
    assert!(value["machines"][0]["shares"][1]["problem"].is_string());

    let (status, value) = json(&["unshare", "m", "src", "--json"]);
    assert_eq!((status, value), (Some(0), json!({ "unshared": "src" })));
    let (status, value) = json(&["unshare", "m", "src", "--json"]);
    assert_eq!((status, &value["error"]["code"]), (Some(1), &json!("not-found")));
    let (_, value) = json(&["list", "--json"]);
    assert_eq!(value["machines"][0]["shares"].as_array().unwrap().len(), 1);
    _ = std::fs::remove_dir_all(&folder);
}
