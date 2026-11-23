//! The store: a folder with one folder per machine.
//!
//! ```text
//! <store>/lock                  held while the set of machines changes
//! <store>/machines/<name>/      a machine
//!     config.json               what it is made of
//!     disk.raw  seed.iso        its disk, and what sets it up on its first start
//!     lock                      held by its process for as long as it runs
//!     process.json              that process and its socket
//!     console.log  machine.log  what the guest printed, and what its process had to say
//! ```
//!
//! A machine that is being created is put together in a folder that isn't a machine yet, and
//! becomes one by being renamed.

use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::lock::Lock;
use crate::{Context, Error, ErrorCode, Result, random_hex};

/// The newest layout of `config.json` that this build reads, and the one it writes.
pub(crate) const CONFIG_VERSION: u32 = 1;

/// What a machine is made of: its `config.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// The layout of this file.
    pub version: u32,
    /// Stays the same when the machine is renamed.
    pub id: String,
    pub name: String,
    /// The image of the catalog that it was created from.
    pub image: String,
    /// `arm64` or `x86-64`.
    pub arch: String,
    pub cpus: u32,
    /// In bytes.
    pub memory: u64,
    pub disk: DiskConfig,
    /// The guest finds its network card by this address, so it never changes.
    pub mac_address: String,
    /// The account that the first start creates.
    pub user: String,
    /// Seconds since 1970.
    pub created: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskConfig {
    /// The file's name in the machine's folder.
    pub file: String,
    pub format: DiskFormat,
    /// What the guest sees, in bytes.
    pub size: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DiskFormat {
    Raw,
}

impl Config {
    pub(crate) const FILE: &'static str = "config.json";

    pub fn load(machine: &Path) -> Result<Self> {
        let path = machine.join(Self::FILE);
        let text = std::fs::read(&path).at("read", &path)?;
        let unreadable =
            |why: String| Error::new(ErrorCode::Io, format!("{} can't be used: {why}.", path.display()));
        let config: Self = serde_json::from_slice(&text).map_err(|error| unreadable(error.to_string()))?;
        if config.version > CONFIG_VERSION {
            return Err(unreadable("it was written by a newer version".into()));
        }
        Ok(config)
    }

    /// Writes the file whole or not at all.
    pub fn save(&self, machine: &Path) -> Result<()> {
        let path = machine.join(Self::FILE);
        let partial = machine.join(format!("{}.new", Self::FILE));
        let mut text = serde_json::to_vec_pretty(self).expect("a configuration is plain data");
        text.push(b'\n');
        std::fs::write(&partial, text).at("write", &partial)?;
        std::fs::rename(&partial, &path).at("write", &path)
    }
}

/// What is wrong with `name` as a machine's name, which is also its folder's name and its host
/// name in the guest.
pub(crate) fn check_name(name: &str) -> Result<()> {
    let allowed = |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-';
    let valid = (1..=63).contains(&name.len())
        && name.bytes().all(allowed)
        && !name.starts_with('-')
        && !name.ends_with('-');
    if valid {
        return Ok(());
    }
    Err(Error::new(
        ErrorCode::InvalidArgument,
        format!(
            "\"{name}\" can't be a machine's name. Use 1 to 63 lowercase letters, digits and \"-\", \
             with a letter or a digit first and last."
        ),
    ))
}

#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

/// A machine that is being put together. Dropped without having been published, it is removed.
pub(crate) struct Staged {
    pub directory: PathBuf,
    /// Says that someone is still at work here.
    lock: Option<Lock>,
}

impl Drop for Staged {
    fn drop(&mut self) {
        if self.lock.is_some() {
            _ = std::fs::remove_dir_all(&self.directory);
        }
    }
}

const STAGING: &str = ".staging-";
const TRASH: &str = ".trash-";
pub(crate) const LOCK: &str = "lock";

impl Store {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn machines(&self) -> PathBuf {
        self.root.join("machines")
    }

    /// Where the machine called `name` is, if there is one.
    pub fn directory(&self, name: &str) -> PathBuf {
        self.machines().join(name)
    }

    /// The folder of a machine that exists.
    pub(crate) fn find(&self, name: &str) -> Result<PathBuf> {
        let directory = self.directory(name);
        // A name that can't be a machine's is not looked for: it could lead out of the store.
        if check_name(name).is_err() || !directory.is_dir() {
            return Err(Error::new(
                ErrorCode::NotFound,
                format!("There is no machine called \"{name}\"."),
            ));
        }
        Ok(directory)
    }

    /// The names of the machines, sorted.
    pub fn names(&self) -> Result<Vec<String>> {
        let machines = self.machines();
        let entries = match std::fs::read_dir(&machines) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error).at("list", &machines),
        };
        let mut names: Vec<String> = entries
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.path().is_dir())
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| check_name(name).is_ok())
            .collect();
        names.sort();
        Ok(names)
    }

    /// Held while the set of machines changes.
    fn lock(&self) -> Result<Lock> {
        std::fs::create_dir_all(self.machines()).at("create", &self.machines())?;
        let path = self.root.join(LOCK);
        Lock::take(&path).at("lock", &path)
    }

    /// A new folder to put a machine together in.
    pub(crate) fn stage(&self) -> Result<Staged> {
        let _changing = self.lock()?;
        self.sweep();
        let directory = self.machines().join(format!("{STAGING}{}", random_hex(8)?));
        // A machine's disk and its account's password are nobody else's to read.
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .at("create", &directory)?;
        let lock = Lock::try_take(&directory.join(LOCK)).at("lock", &directory)?;
        Ok(Staged { directory, lock })
    }

    /// Makes what was staged the machine called `name`.
    pub(crate) fn publish(&self, staged: &mut Staged, name: &str) -> Result<PathBuf> {
        let _changing = self.lock()?;
        let directory = self.directory(name);
        // A rename would put the new machine in the place of an empty folder.
        if directory.exists() {
            return Err(already_exists(name));
        }
        std::fs::rename(&staged.directory, &directory).at("create", &directory)?;
        // From here on the lock means that the machine is running.
        staged.lock = None;
        Ok(directory)
    }

    /// Removes the machine in `directory`, which the caller has locked so that it can't start.
    pub(crate) fn remove(&self, directory: &Path, _stopped: Lock) -> Result<()> {
        let _changing = self.lock()?;
        // Gone in one step, also when deleting its files then takes time or fails halfway.
        let trash = self.machines().join(format!("{TRASH}{}", random_hex(8)?));
        std::fs::rename(directory, &trash).at("delete", directory)?;
        _ = std::fs::remove_dir_all(&trash);
        self.sweep();
        Ok(())
    }

    /// Removes what creations that were cut short and deletions that didn't finish left behind.
    /// Called with the store's lock held.
    fn sweep(&self) {
        let Ok(entries) = std::fs::read_dir(self.machines()) else {
            return;
        };
        for entry in entries.filter_map(|entry| entry.ok()) {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let abandoned = name.starts_with(TRASH)
                || name.starts_with(STAGING)
                    && matches!(Lock::try_take(&entry.path().join(LOCK)), Ok(Some(_)));
            if abandoned {
                _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }
}

pub(crate) fn already_exists(name: &str) -> Error {
    Error::new(
        ErrorCode::AlreadyExists,
        format!("A machine called \"{name}\" exists already."),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(test: &str) -> Store {
        let root = std::env::temp_dir().join(format!("store-test-{test}-{}", random_hex(4).unwrap()));
        Store::new(root)
    }

    #[test]
    fn a_staged_machine_appears_only_when_published() {
        let store = store("publish");
        let mut staged = store.stage().unwrap();
        std::fs::write(staged.directory.join(Config::FILE), "{}").unwrap();
        assert!(store.names().unwrap().is_empty());

        let directory = store.publish(&mut staged, "a").unwrap();
        assert_eq!(store.names().unwrap(), ["a"]);
        assert!(directory.join(Config::FILE).exists());
        // Published, its lock is free: it isn't running.
        assert!(!Lock::is_held(&directory.join(LOCK)));

        let mut second = store.stage().unwrap();
        assert_eq!(
            store.publish(&mut second, "a").unwrap_err().code,
            ErrorCode::AlreadyExists
        );
        std::fs::remove_dir_all(store.root()).unwrap();
    }

    #[test]
    fn what_a_creation_left_behind_is_swept_but_not_one_under_way() {
        let store = store("sweep");
        let under_way = store.stage().unwrap();
        let abandoned = store.machines().join(format!("{STAGING}abandoned"));
        std::fs::create_dir(&abandoned).unwrap();
        std::fs::write(abandoned.join(LOCK), "").unwrap();

        let another = store.stage().unwrap();
        assert!(!abandoned.exists());
        assert!(under_way.directory.exists());

        let directory = under_way.directory.clone();
        drop(under_way);
        assert!(!directory.exists());
        drop(another);
        std::fs::remove_dir_all(store.root()).unwrap();
    }
}
