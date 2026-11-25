//! The folders that the host shares: virtio-fs devices, mounted in [`SHARED`].
//!
//! The host says which to mount, and the agent unmounts the shared folders that it no longer
//! lists. What it was told last is kept, and mounted again when the agent starts, which is how
//! shares come back after the guest restarts.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use proto::agent::{Mount, SHARED};
use proto::{Error, ErrorCode, NAME};

/// How long a device that the host has only just added gets to show up in the guest.
const APPEARING: Duration = Duration::from_secs(10);

/// Held while mounts change. Says whether the host has said what to mount since the agent
/// started, which makes the list kept from before out of date.
static TOLD: Mutex<bool> = Mutex::new(false);

/// Where the last list the host sent is kept.
fn kept() -> PathBuf {
    PathBuf::from(format!("/var/lib/{NAME}/mounts.json"))
}

/// A virtio-fs mount in [`SHARED`], as the system lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Mounted {
    tag: String,
    path: String,
    read_only: bool,
}

/// What is wrong with `mount`, which only ever goes in [`SHARED`].
fn check(mount: &Mount) -> Result<(), Error> {
    let name = mount.path.strip_prefix(SHARED);
    let valid = match name {
        Some("") => true,
        Some(name) => name
            .strip_prefix('/')
            .is_some_and(|name| !name.is_empty() && !name.contains('/') && name != "." && name != ".."),
        None => false,
    };
    if !valid || mount.tag.is_empty() || mount.tag.contains('\0') || mount.path.contains('\0') {
        return Err(Error::new(
            ErrorCode::InvalidArgument,
            format!(
                "The guest's agent doesn't mount \"{}\" at {}.",
                mount.tag, mount.path
            ),
        ));
    }
    Ok(())
}

/// A path as mountinfo writes it, with spaces, tabs, line breaks and backslashes as octal escapes.
fn unescape(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let octal = bytes.get(index + 1..index + 4).filter(|digits| {
            bytes[index] == b'\\' && digits.iter().all(|digit| (b'0'..=b'7').contains(digit))
        });
        match octal {
            Some(digits) => {
                out.push(
                    digits
                        .iter()
                        .fold(0u8, |value, digit| value.wrapping_mul(8) + (digit - b'0')),
                );
                index += 4;
            }
            None => {
                out.push(bytes[index]);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The virtio-fs mounts in [`SHARED`] among what `/proc/self/mountinfo` says.
fn parse(mountinfo: &str) -> Vec<Mounted> {
    mountinfo
        .lines()
        .filter_map(|line| {
            let (before, after) = line.split_once(" - ")?;
            let fields: Vec<&str> = before.split(' ').collect();
            let mut after = after.split(' ');
            let (kind, source) = (after.next()?, after.next()?);
            let path = unescape(fields.get(4)?);
            let read_only = fields.get(5)?.split(',').any(|option| option == "ro");
            let inside = path == SHARED || path.starts_with(&format!("{SHARED}/"));
            (kind == "virtiofs" && inside).then(|| Mounted {
                tag: unescape(source),
                path,
                read_only,
            })
        })
        .collect()
}

/// Mounts what the host shares, and unmounts what it no longer does. A device that isn't there
/// yet is waited for a while if `wait`. Says what couldn't be mounted.
fn apply(mounts: &[Mount], wait: bool) -> Result<(), Error> {
    for mount in mounts {
        check(mount)?;
    }
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo")
        .map_err(|error| Error::new(ErrorCode::Io, format!("Couldn't read what is mounted: {error}.")))?;
    // What is mounted as it should be stays, unless its device now holds other folders: what
    // the guest remembers of the ones before is wrong, and a new mount forgets it.
    let stays = |mounted: &Mounted| {
        mounts.iter().any(|mount| {
            !mount.fresh
                && mount.tag == mounted.tag
                && mount.path == mounted.path
                && mount.read_only == mounted.read_only
        })
    };
    let mut present = parse(&mountinfo);
    // The deepest first: a folder in SHARED before SHARED itself.
    present.sort_by_key(|mounted| std::cmp::Reverse(mounted.path.len()));
    for mounted in present.iter().filter(|mounted| !stays(mounted)) {
        system::unmount(&mounted.path);
        let wanted_there = mounts.iter().any(|mount| mount.path == mounted.path);
        if mounted.path != SHARED && !wanted_there {
            _ = std::fs::remove_dir(&mounted.path);
        }
    }
    let mut failed = Vec::new();
    for mount in mounts {
        let there = present
            .iter()
            .any(|mounted| stays(mounted) && mounted.path == mount.path);
        if there {
            continue;
        }
        let deadline = Instant::now() + if wait { APPEARING } else { Duration::ZERO };
        let result = loop {
            match system::mount(mount) {
                Ok(()) => break Ok(()),
                Err(error) if Instant::now() >= deadline => break Err(error),
                Err(_) => std::thread::sleep(Duration::from_millis(200)),
            }
        };
        if let Err(error) = result {
            if mount.path != SHARED {
                _ = std::fs::remove_dir(&mount.path);
            }
            failed.push(format!("{} at {}: {error}", mount.tag, mount.path));
        }
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(Error::new(
            ErrorCode::Guest,
            format!("The guest couldn't mount {}.", failed.join("; ")),
        ))
    }
}

/// Mounts what the host shares, keeps the list for the next start, and says what couldn't be
/// mounted.
pub fn run(mounts: &[Mount]) -> Result<(), Error> {
    let mut told = TOLD.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    *told = true;
    let applied = apply(mounts, true);
    let path = kept();
    let partial = path.with_extension("json.new");
    let written = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| {
            // Mounted anew once is enough: the next start mounts them anew anyway.
            let kept: Vec<Mount> = mounts
                .iter()
                .map(|mount| Mount {
                    fresh: false,
                    ..mount.clone()
                })
                .collect();
            std::fs::write(&partial, serde_json::to_vec(&kept).expect("plain data"))
        })
        .and_then(|()| std::fs::rename(&partial, &path));
    if let Err(error) = written {
        eprintln!("Couldn't keep the list of shared folders: {error}");
    }
    applied
}

/// Mounts what the host shared when it last said, as the agent starts.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn restore() {
    let told = TOLD.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if *told {
        return;
    }
    let Ok(text) = std::fs::read(kept()) else {
        return;
    };
    let Ok(mounts) = serde_json::from_slice::<Vec<Mount>>(&text) else {
        return;
    };
    // A share that the host took away while the guest was off has no device: the host says
    // what it shares once it reaches the agent.
    if let Err(error) = apply(&mounts, false) {
        eprintln!("{}", error.message);
    }
}

#[cfg(target_os = "linux")]
mod system {
    use std::ffi::CString;

    use proto::agent::Mount;

    pub fn mount(mount: &Mount) -> std::io::Result<()> {
        std::fs::create_dir_all(&mount.path)?;
        let tag = CString::new(mount.tag.as_str())?;
        let path = CString::new(mount.path.as_str())?;
        let mut flags = libc::MS_NOSUID | libc::MS_NODEV;
        if mount.read_only {
            flags |= libc::MS_RDONLY;
        }
        // Safety: strings that end in a zero and live through the call, and no data.
        let result = unsafe {
            libc::mount(
                tag.as_ptr(),
                path.as_ptr(),
                c"virtiofs".as_ptr(),
                flags,
                std::ptr::null(),
            )
        };
        if result != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    /// Unmounts what is at `path`, also while programs still use it: they keep what they have
    /// open.
    pub fn unmount(path: &str) {
        if let Ok(path) = CString::new(path) {
            // Safety: a string that ends in a zero and lives through the call.
            unsafe { libc::umount2(path.as_ptr(), libc::MNT_DETACH) };
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod system {
    use proto::agent::Mount;

    pub fn mount(_mount: &Mount) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "the agent mounts shared folders in Linux only",
        ))
    }

    pub fn unmount(_path: &str) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtio_fs_mounts_in_the_shared_folder_are_found() {
        let mountinfo = "\
22 1 254:1 / / rw,relatime shared:1 - ext4 /dev/vda1 rw
40 22 0:35 / /mnt/shared/My\\040Docs ro,nosuid,nodev,relatime shared:20 - virtiofs My\\040Docs ro
41 22 0:36 / /mnt/shared rw,nosuid,nodev,relatime shared:21 - virtiofs svm rw
42 22 0:37 / /mnt/sharedness rw,relatime shared:22 - virtiofs other rw
43 22 0:38 / /srv rw,relatime - virtiofs elsewhere rw
";
        assert_eq!(
            parse(mountinfo),
            [
                Mounted {
                    tag: "My Docs".into(),
                    path: "/mnt/shared/My Docs".into(),
                    read_only: true,
                },
                Mounted {
                    tag: "svm".into(),
                    path: "/mnt/shared".into(),
                    read_only: false,
                },
            ]
        );
    }

    #[test]
    fn only_the_shared_folder_and_folders_right_in_it_are_mounted() {
        let mount = |path: &str| Mount {
            tag: "a".into(),
            path: path.into(),
            read_only: false,
            fresh: false,
        };
        assert!(check(&mount("/mnt/shared")).is_ok());
        assert!(check(&mount("/mnt/shared/a")).is_ok());
        for path in [
            "/mnt/shared/",
            "/mnt/shared/a/b",
            "/mnt/shared/..",
            "/etc",
            "/mnt/sharedx",
        ] {
            assert!(check(&mount(path)).is_err(), "{path}");
        }
        assert!(
            check(&Mount {
                tag: String::new(),
                ..mount("/mnt/shared")
            })
            .is_err()
        );
    }
}
