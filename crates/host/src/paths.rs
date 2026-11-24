//! Where the store, the cache and the runtime folder are, and where sockets go.
//!
//! Each has a variable of its own, so that a program shipped in an app keeps to the app's folders
//! and never meets a copy installed on the same computer.

use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};

use proto::{Error, ErrorCode, NAME, Result, variable};

use crate::Context;

/// A path of a Unix socket has room for this many bytes on macOS, which has less than Linux.
const LONGEST_SOCKET_PATH: usize = 103;

fn environment(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn user_home() -> Result<PathBuf> {
    environment("HOME").ok_or_else(|| {
        Error::new(
            ErrorCode::InvalidArgument,
            format!(
                "HOME isn't set. Set {} and {} to folders to use.",
                variable("HOME"),
                variable("CACHE")
            ),
        )
    })
}

/// A folder named by the product's own variable, or the system's place for such folders.
fn directory(suffix: &str, xdg: &str, on_mac: &str, elsewhere: &str) -> Result<PathBuf> {
    if let Some(chosen) = environment(&variable(suffix)) {
        // A machine's process is given its folder, and doesn't start where this program did.
        return std::path::absolute(&chosen).at("find", &chosen);
    }
    if cfg!(target_os = "macos") {
        return Ok(user_home()?.join(on_mac).join(NAME));
    }
    match environment(xdg).filter(|path| path.is_absolute()) {
        Some(base) => Ok(base.join(NAME)),
        None => Ok(user_home()?.join(elsewhere).join(NAME)),
    }
}

/// The store: `<NAME>_HOME`, or the system's folder for an application's data.
pub fn home_directory() -> Result<PathBuf> {
    directory(
        "HOME",
        "XDG_DATA_HOME",
        "Library/Application Support",
        ".local/share",
    )
}

/// The cache of downloads: `<NAME>_CACHE`, or the system's folder for caches.
pub fn cache_directory() -> Result<PathBuf> {
    directory("CACHE", "XDG_CACHE_HOME", "Library/Caches", ".cache")
}

/// Where a machine's socket called `name` goes.
///
/// In the machine's folder, where that is short enough for a socket's path. The store can be
/// anywhere, so it may not be: then the socket goes to a short folder that only this user can
/// open. Whoever connects reads the path from what the machine's process wrote down, and never
/// works it out.
pub fn socket_path(machine: &Path, id: &str, name: &str) -> Result<PathBuf> {
    let inside = machine.join(format!("{name}.sock"));
    if inside.as_os_str().len() <= LONGEST_SOCKET_PATH {
        return Ok(inside);
    }
    let outside = runtime_directory()?.join(format!("{id}.{name}.sock"));
    if outside.as_os_str().len() > LONGEST_SOCKET_PATH {
        return Err(Error::new(
            ErrorCode::Io,
            format!(
                "No folder is short enough for a socket: {} is too long.",
                outside.display()
            ),
        ));
    }
    Ok(outside)
}

/// The runtime folder, for what lasts no longer than the computer is on: sockets whose path is too
/// long for their machine's folder, and screenshots taken without a path. `<NAME>_RUNTIME`, or a
/// folder of this user's under `XDG_RUNTIME_DIR` where there is one, or in `/tmp`. Its path should
/// be short: a socket's path has room for 103 bytes on macOS.
pub fn runtime_directory() -> Result<PathBuf> {
    runtime_in(environment(&variable("RUNTIME")))
}

fn runtime_in(chosen: Option<PathBuf>) -> Result<PathBuf> {
    // Safety: getuid has nothing to fail at.
    let user = unsafe { libc::getuid() };
    if let Some(chosen) = chosen {
        let directory = std::path::absolute(&chosen).at("find", &chosen)?;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&directory)
            .at("create", &directory)?;
        return Ok(directory);
    }
    let directory = match environment("XDG_RUNTIME_DIR").filter(|path| path.is_absolute()) {
        Some(runtime) if cfg!(not(target_os = "macos")) => runtime.join(NAME),
        _ => PathBuf::from(format!("/tmp/{NAME}-{user}")),
    };
    match std::fs::DirBuilder::new().mode(0o700).create(&directory) {
        Err(error) if error.kind() != std::io::ErrorKind::AlreadyExists => {
            return Err(error).at("create", &directory);
        }
        _ => {}
    }
    // Anyone can make a folder in /tmp before this user does, and wait there for its sockets.
    let found = std::fs::symlink_metadata(&directory).at("look at", &directory)?;
    if !found.is_dir() || found.uid() != user || found.mode() & 0o077 != 0 {
        return Err(Error::new(
            ErrorCode::Io,
            format!(
                "{} isn't a folder that only this user can open.",
                directory.display()
            ),
        ));
    }
    Ok(directory)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_socket_leaves_a_folder_that_is_too_long_for_it() {
        let short = Path::new("/s/machines/a");
        assert_eq!(
            socket_path(short, "0123", "control").unwrap(),
            short.join("control.sock")
        );

        let long = PathBuf::from(format!("/{}/machines/a", "x".repeat(100)));
        let path = socket_path(&long, "0123", "control").unwrap();
        assert!(!path.starts_with(&long));
        assert!(path.as_os_str().len() <= LONGEST_SOCKET_PATH);
        assert!(path.ends_with("0123.control.sock"));
    }

    #[test]
    fn a_chosen_runtime_folder_is_made_for_this_user_alone() {
        let root = std::env::temp_dir().join(format!("runtime-test-{}", std::process::id()));
        let chosen = root.join("a/b");
        assert_eq!(runtime_in(Some(chosen.clone())).unwrap(), chosen);
        let found = std::fs::metadata(&chosen).unwrap();
        assert!(found.is_dir());
        assert_eq!(found.mode() & 0o777, 0o700);
        // Again, once it is there.
        assert_eq!(runtime_in(Some(chosen.clone())).unwrap(), chosen);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
