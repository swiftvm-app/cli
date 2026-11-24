//! A machine made from an image starts with a disk on which another machine has run. cloud-init
//! gives it a host name, SSH host keys and network settings of its own, because the seed it is
//! started with names another instance. The machine ID is left to this: early in each start,
//! before the services that read it, it is renewed when the instance is new on a disk that was
//! used before.
//!
//! The instance that the disk was last used by is noted in `/var/lib/<name>/instance-id`. The
//! first start of a machine only notes it. Where nothing was noted, on the disk of a machine made
//! before this was installed, cloud-init's own note of the instance before is gone by.

use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// What cloud-init calls the instance it has started, which it writes before the services that
/// read the machine ID start.
const INSTANCE: &str = "var/lib/cloud/data/instance-id";
/// The instance before it, which cloud-init writes alongside.
const PREVIOUS_INSTANCE: &str = "var/lib/cloud/data/previous-instance-id";
const MACHINE_ID: &str = "etc/machine-id";
/// What D-Bus reads, on systems where it isn't a link to the machine ID.
const DBUS_MACHINE_ID: &str = "var/lib/dbus/machine-id";

fn noted(root: &Path) -> PathBuf {
    root.join(format!("var/lib/{}/instance-id", proto::NAME))
}

/// What happened.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// cloud-init hasn't said which instance this is.
    Unknown,
    /// The instance is the one that was noted.
    Same,
    /// No instance was noted before: it is now.
    Noted,
    /// Another instance was noted: the machine ID is a new one.
    Renewed,
}

/// Writes `contents` to `path` whole or not at all, readable by everyone.
fn replace(path: &Path, contents: &[u8], mode: u32) -> io::Result<()> {
    let mut name = path.as_os_str().to_owned();
    name.push(".new");
    let partial = PathBuf::from(name);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(&partial)?;
    file.write_all(contents)?;
    file.sync_all()?;
    std::fs::set_permissions(&partial, std::fs::Permissions::from_mode(mode))?;
    std::fs::rename(&partial, path)
}

/// A machine ID as systemd makes one: 128 random bits, marked as a random UUID would be.
fn new_machine_id() -> io::Result<String> {
    let mut id = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut id)?;
    id[6] = id[6] & 0x0F | 0x40;
    id[8] = id[8] & 0x3F | 0x80;
    Ok(id.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Renews the machine ID of the system at `root` when it starts as a new instance on a disk
/// that another one used.
pub fn renew(root: &Path) -> io::Result<Outcome> {
    let instance = match std::fs::read_to_string(root.join(INSTANCE)) {
        Ok(instance) => instance.trim().to_string(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Outcome::Unknown),
        Err(error) => return Err(error),
    };
    if instance.is_empty() {
        return Ok(Outcome::Unknown);
    }
    let noted_path = noted(root);
    let previous = match std::fs::read_to_string(&noted_path) {
        Ok(noted) if noted.trim() == instance => return Ok(Outcome::Same),
        Ok(noted) => Some(noted.trim().to_string()),
        // Nothing noted: the disk of a machine made before this was installed, maybe. cloud-init
        // remembers the instance that it set the system up as before.
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            std::fs::read_to_string(root.join(PREVIOUS_INSTANCE))
                .ok()
                .map(|previous| previous.trim().to_string())
                .filter(|previous| !previous.is_empty() && *previous != instance)
        }
        Err(error) => return Err(error),
    };
    let outcome = match previous {
        Some(_) => {
            let id = format!("{}\n", new_machine_id()?);
            replace(&root.join(MACHINE_ID), id.as_bytes(), 0o444)?;
            let dbus = root.join(DBUS_MACHINE_ID);
            if std::fs::symlink_metadata(&dbus).is_ok_and(|metadata| metadata.is_file()) {
                replace(&dbus, id.as_bytes(), 0o444)?;
            }
            Outcome::Renewed
        }
        None => Outcome::Noted,
    };
    if let Some(folder) = noted_path.parent() {
        std::fs::create_dir_all(folder)?;
    }
    replace(&noted_path, format!("{instance}\n").as_bytes(), 0o644)?;
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_machine_id_is_renewed_only_for_a_new_instance_on_a_used_disk() {
        let root = std::env::temp_dir().join(format!("identity-test-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&root);
        for folder in ["etc", "var/lib/cloud/data", "var/lib/dbus"] {
            std::fs::create_dir_all(root.join(folder)).unwrap();
        }
        let machine_id = || std::fs::read_to_string(root.join(MACHINE_ID)).unwrap();
        let first = "0123456789abcdef0123456789abcdef\n";
        std::fs::write(root.join(MACHINE_ID), first).unwrap();
        std::fs::write(root.join(DBUS_MACHINE_ID), first).unwrap();

        assert_eq!(renew(&root).unwrap(), Outcome::Unknown);
        std::fs::write(root.join(INSTANCE), "a\n").unwrap();
        assert_eq!(renew(&root).unwrap(), Outcome::Noted);
        assert_eq!(renew(&root).unwrap(), Outcome::Same);
        assert_eq!(machine_id(), first);

        // The disk is started as another instance: a machine made from an image.
        std::fs::write(root.join(INSTANCE), "b\n").unwrap();
        assert_eq!(renew(&root).unwrap(), Outcome::Renewed);
        let renewed = machine_id();
        assert_ne!(renewed, first);
        assert_eq!(renewed.trim().len(), 32);
        assert!(renewed.trim().bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(
            std::fs::read_to_string(root.join(DBUS_MACHINE_ID)).unwrap(),
            renewed
        );
        assert_eq!(renew(&root).unwrap(), Outcome::Same);
        assert_eq!(machine_id(), renewed);

        // Nothing noted, and cloud-init says it set the system up as another instance before.
        std::fs::remove_file(noted(&root)).unwrap();
        std::fs::write(root.join(PREVIOUS_INSTANCE), "b\n").unwrap();
        assert_eq!(renew(&root).unwrap(), Outcome::Noted);
        std::fs::remove_file(noted(&root)).unwrap();
        std::fs::write(root.join(INSTANCE), "c\n").unwrap();
        assert_eq!(renew(&root).unwrap(), Outcome::Renewed);
        assert_ne!(machine_id(), renewed);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
