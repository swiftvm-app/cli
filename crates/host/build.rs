//! Builds the guest's agent, a static Linux program for the processor this library is built for,
//! and hands it to the code as `$OUT_DIR/agent`. Guests have the host's processor.
//!
//! It is linked with the linker that comes with Rust, so that no C toolchain for Linux is needed.
//! `CARGO_TARGET_<TARGET>_LINKER` names another one.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").expect("cargo says which processor");
    let target = format!("{arch}-unknown-linux-musl");
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for watched in ["crates/agent", "crates/proto", "Cargo.toml", "Cargo.lock"] {
        println!("cargo::rerun-if-changed={}", root.join(watched).display());
    }
    // Apart from this build's own, which waits for this script to end.
    let target_directory = root.join("target/agent");
    let linker = format!("CARGO_TARGET_{}_LINKER", target.to_uppercase().replace('-', "_"));
    println!("cargo::rerun-if-env-changed={linker}");

    // Of two builds of one version, the later replaces the earlier in guests.
    let built = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("the clock is past 1970")
        .as_secs();
    println!("cargo::rustc-env=AGENT_BUILT={built}");

    let mut command = Command::new(std::env::var_os("CARGO").expect("cargo says where it is"));
    command
        .current_dir(&root)
        .args(["build", "--release", "--package", "agent", "--target", &target])
        .arg("--target-dir")
        .arg(&target_directory);
    // What this build was given is for the program it builds, not for the agent.
    for variable in [
        "CARGO_ENCODED_RUSTFLAGS",
        "RUSTFLAGS",
        "CARGO_MAKEFLAGS",
        "CARGO_TARGET_DIR",
        "CARGO_BUILD_TARGET",
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
    ] {
        command.env_remove(variable);
    }
    command.env("AGENT_BUILT", built.to_string());
    if std::env::var_os(&linker).is_none() {
        command.env(&linker, "rust-lld");
    }
    let status = command.status().expect("cargo can be started");
    if !status.success() {
        panic!(
            "Couldn't build the guest's agent for {target}. If the target is missing: rustup target add {target}"
        );
    }
    let built: PathBuf = target_directory.join(&target).join("release/agent");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo gives a folder")).join("agent");
    std::fs::copy(&built, &out).expect("the agent was built");
}
