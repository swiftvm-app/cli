//! What the command does, as a library: the store, the images and the machines' processes
//! put together.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use proto::agent::{Ended, Exec, GuestInfo, Job, SetupState, receive_copy, run_job, send_copy};
use serde::Serialize;

use crate::engine;
use crate::images::{self, ARCH};
use crate::lock::Lock;
use crate::paths::socket_path;
use crate::process::SEED_FILE;
use crate::saved::{self, ImageKind, SavedImage};
use crate::scripts::Script;
use crate::seed::{self, Seed};
use crate::shares::{self, ShareInfo};
use crate::store::{self, Automatic, Config, DiskConfig, DiskFormat, LOCK, Snapshot, Store};
use crate::{
    AGENT, Context, Error, ErrorCode, NAME, Result, client, download, guest, layers, now, random_hex,
};

const DISK_FILE: &str = "disk.layer";
const DEFAULT_CPUS: u32 = 4;
const DEFAULT_MEMORY: u64 = 4 << 30;
const DEFAULT_DISK: u64 = 32 << 30;
const SMALLEST_MEMORY: u64 = 256 << 20;
/// How hard an image made from a machine is compressed: it is made once, and kept and copied.
const SAVE_LEVEL: i32 = 19;
/// The name of an image's description in a file it is exported to.
const EXPORTED_NOTE: &str = "image.json";
/// The name of its layer there.
const EXPORTED_LAYER: &str = "image.layer";

/// What a machine is created from.
enum Source {
    Catalog(images::Image),
    Saved(SavedImage),
}

pub struct Host {
    store: Store,
    cache: PathBuf,
    program: PathBuf,
}

/// What a machine is created with. What is left out gets a default.
#[derive(Debug, Clone, Default)]
pub struct CreateOptions {
    /// An image of the catalog: `debian-13`.
    pub image: String,
    pub name: Option<String>,
    pub cpus: Option<u32>,
    /// In bytes.
    pub memory: Option<u64>,
    /// The size of the disk in bytes. The guest's system grows into it.
    pub disk: Option<u64>,
    /// The account to create. Without a name it is called after this computer's user.
    pub user: Option<String>,
    /// Without one, nobody can sign in to the account with a password.
    pub password: Option<String>,
    /// Setup scripts, run one after the other once the first start is over: the names of those
    /// that come with this library ([`setup_scripts`](crate::setup_scripts)), or paths of files.
    pub scripts: Vec<String>,
    /// Lets the guest run virtual machines of its own. Not every computer can.
    pub nested: bool,
    /// Folders of this computer that the guest sees in `/mnt/shared`, each under its last part's
    /// name.
    pub shares: Vec<PathBuf>,
}

/// A command to run in a guest.
#[derive(Debug, Clone, Default)]
pub struct ExecOptions {
    /// The program and its arguments. No shell reads them.
    pub arguments: Vec<String>,
    /// The account to run it as. The machine's own unless said.
    pub user: Option<String>,
    /// Where to run it. A relative path is taken from the account's home folder, which is where
    /// it runs without one.
    pub directory: Option<String>,
    /// Added to the command's environment.
    pub environment: BTreeMap<String, String>,
    /// How long it may run before it is ended.
    pub timeout: Option<Duration>,
}

/// How far the creation of a machine has come.
///
/// As JSON it is an object whose `progress` names the step, with the step's fields beside it:
/// `{"progress": "downloading", "received": 1048576, "total": 2097152}`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(tag = "progress", rename_all = "kebab-case")]
pub enum Progress {
    /// The image is on its way: bytes that have arrived, and of how many if the server said.
    Downloading { received: u64, total: Option<u64> },
    /// What runs machines on this computer is on its way, the first time: cloud-hypervisor
    /// and its firmware on Linux.
    DownloadingEngine { received: u64, total: Option<u64> },
    /// The image is being made into the layer that its machines' disks are built on, from 0 to
    /// 1. That is done once for each image.
    WritingDisk { fraction: f64 },
    /// A machine's disk is being compressed into an image, from 0 to 1.
    Compressing { fraction: f64 },
    /// An image is being read from a file, from 0 to 1.
    Importing { fraction: f64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MachineState {
    Running,
    Stopped,
    /// Its folder is there, but what it is made of can't be read.
    Broken,
}

/// A machine as the command shows it.
#[derive(Debug, Clone, Serialize)]
pub struct Machine {
    pub name: String,
    pub state: MachineState,
    /// Its process, while it runs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// What is wrong with a broken machine. The fields below say nothing about one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
    pub id: String,
    pub image: String,
    pub arch: String,
    pub cpus: u32,
    /// In bytes.
    pub memory: u64,
    /// What the guest sees of its disk, in bytes.
    pub disk_size: u64,
    /// What the machine's own layer of its disk takes up on this computer, in bytes.
    pub disk_used: u64,
    /// What the layers its disk is built on take up, in bytes. Other machines share them.
    pub disk_shared: u64,
    pub user: String,
    /// Seconds since 1970.
    pub created: u64,
    /// Whether the guest can run virtual machines of its own.
    pub nested: bool,
    /// The folders of this computer that the guest sees in `/mnt/shared`.
    pub shares: Vec<ShareInfo>,
}

/// An image that machines can be created from.
#[derive(Debug, Clone, Serialize)]
pub struct ImageSummary {
    /// What `create` is given.
    pub id: String,
    /// The system, for an image of the catalog. What it was made of, for one made from a machine.
    pub name: String,
    pub kind: ImageKind,
    /// It is on this computer: downloaded, or made here.
    pub cached: bool,
    /// What its layer takes up on this computer, in bytes. Only for one made from a machine.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// The account that its machines have. Only for one made from a machine.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Seconds since 1970. Only for one made from a machine.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created: Option<u64>,
}

/// A snapshot of a machine's disk.
#[derive(Debug, Clone, Serialize)]
pub struct SnapshotInfo {
    pub name: String,
    /// Seconds since 1970.
    pub created: u64,
    /// What it holds that the layers under it don't, as it takes up room on this computer, in
    /// bytes.
    pub size: u64,
}

/// A disk, as its layers: its own first, then those it is built on.
#[derive(Debug, Clone, Serialize)]
pub struct DiskInfo {
    /// The machine whose disk it is, if it was asked for by its machine.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
    pub layers: Vec<LayerInfo>,
}

/// A layer of a disk.
#[derive(Debug, Clone, Serialize)]
pub struct LayerInfo {
    pub path: PathBuf,
    /// The SHA-256 of its file, which names a sealed layer in the store.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    /// A sealed layer is never written again.
    pub sealed: bool,
    /// The size of the disk in bytes.
    pub capacity: u64,
    /// What its file takes up on this computer, in bytes.
    pub file_bytes: u64,
    /// The layers and the machines' disks that are built on it.
    pub used_by: usize,
    /// What it holds. Left out for the layer of a machine that is running, which is being written.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contents: Option<LayerContents>,
}

/// What a layer holds, in clusters of 64 KiB.
#[derive(Debug, Clone, Serialize)]
pub struct LayerContents {
    pub clusters: u64,
    /// Clusters with data of their own.
    pub stored: u64,
    /// Clusters that it says are zeros, over what is underneath.
    pub zeros: u64,
    /// Clusters that read what is underneath, or zeros where there is nothing.
    pub inherited: u64,
    /// What the stored clusters take up, compressed, in bytes.
    pub stored_bytes: u64,
}

/// What a check of a disk found, layer by layer.
#[derive(Debug, Clone, Serialize)]
pub struct DiskCheck {
    pub layers: Vec<LayerCheck>,
}

impl DiskCheck {
    pub fn is_whole(&self) -> bool {
        self.layers.iter().all(|layer| layer.problems.is_empty())
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct LayerCheck {
    pub path: PathBuf,
    /// What is wrong with it: nothing, for a layer that is whole.
    pub problems: Vec<String>,
}

/// A machine that is held stopped, by its lock.
struct Held {
    directory: PathBuf,
    config: Config,
    /// Its own layer.
    top: PathBuf,
    lock: Lock,
    was_running: bool,
}

fn no_snapshot(name: &str, snapshot: &str) -> Error {
    Error::new(
        ErrorCode::NotFound,
        format!("\"{name}\" has no snapshot called \"{snapshot}\"."),
    )
}

fn no_image(image: &str) -> Error {
    Error::new(
        ErrorCode::NotFound,
        format!("There is no image called \"{image}\"."),
    )
}

/// What is wrong with `name` as a snapshot's name.
fn check_snapshot_name(name: &str) -> Result<()> {
    store::check_name(name).map_err(|_| {
        Error::new(
            ErrorCode::InvalidArgument,
            format!(
                "\"{name}\" can't be a snapshot's name. Use 1 to 63 lowercase letters, digits and \"-\", \
                 with a letter or a digit first and last."
            ),
        )
    })
}

/// The hash that a sealed layer's file is named by.
fn named_hash(path: &Path) -> Option<[u8; 32]> {
    disk::layer::from_hex(&path.file_stem()?.to_string_lossy())
}

/// The process that runs the machine in `directory`, if one does.
///
/// The machine's lock being held isn't enough to say so: someone who is deleting the machine
/// holds it too, and a process that was killed leaves its note behind. The note has to be there
/// and name a process that is alive.
fn running_process(directory: &Path) -> Option<u32> {
    if !client::is_running(directory) {
        return None;
    }
    let pid = client::process_file(directory)?.pid;
    // Safety: signal 0 only asks whether the process is there.
    (unsafe { libc::kill(pid as libc::pid_t, 0) } == 0).then_some(pid)
}

/// What a file takes up on this computer, in bytes.
fn allocated(path: &Path) -> u64 {
    // Blocks are counted in units of 512 bytes, whatever the file system's own are.
    std::fs::metadata(path).map_or(0, |metadata| metadata.blocks() * 512)
}

fn summary(store: &Store, directory: &Path, name: &str) -> Machine {
    let pid = running_process(directory);
    let running = pid.is_some();
    let mut machine = Machine {
        name: name.to_string(),
        state: if running {
            MachineState::Running
        } else {
            MachineState::Stopped
        },
        pid,
        problem: None,
        id: String::new(),
        image: String::new(),
        arch: String::new(),
        cpus: 0,
        memory: 0,
        disk_size: 0,
        disk_used: 0,
        disk_shared: 0,
        user: String::new(),
        created: 0,
        nested: false,
        shares: Vec::new(),
    };
    match Config::load(directory) {
        Ok(config) => {
            let disk = directory.join(&config.disk.file);
            machine.disk_used = allocated(&disk);
            if config.disk.format == DiskFormat::Layer {
                let chain = layers::chain(store, &disk).unwrap_or_default();
                machine.disk_shared = chain.iter().map(|layer| allocated(layer)).sum();
            }
            machine.id = config.id;
            machine.image = config.image;
            machine.arch = config.arch;
            machine.cpus = config.cpus;
            machine.memory = config.memory;
            machine.disk_size = config.disk.size;
            machine.user = config.user;
            machine.created = config.created;
            machine.nested = config.nested;
            machine.shares = config.shares.iter().map(ShareInfo::of).collect();
        }
        Err(error) if !running => {
            machine.state = MachineState::Broken;
            machine.problem = Some(error.message);
        }
        Err(error) => machine.problem = Some(error.message),
    }
    machine
}

/// Why a change to what a running machine shares waits for its next start.
fn started_by_older_build(name: &str) -> Error {
    Error::new(
        ErrorCode::Protocol,
        format!(
            "\"{name}\" was started by an older build of {NAME}, which can't change what it shares while it runs. The change takes effect when it is started again."
        ),
    )
}

/// The ID that the account of a new machine gets: this user's, so that the files that the guest
/// writes in shared folders are the account's in the guest too. Not root's.
fn account_id() -> Option<u32> {
    // Safety: getuid can't fail.
    let uid = unsafe { libc::getuid() };
    (uid != 0).then_some(uid)
}

/// The address of a network card that no manufacturer has given out.
fn mac_address() -> Result<String> {
    let random = random_hex(6)?;
    let mut bytes: Vec<u8> = (0..6)
        .map(|index| u8::from_str_radix(&random[index * 2..][..2], 16).unwrap())
        .collect();
    // Of one card rather than a group, and assigned locally.
    bytes[0] = bytes[0] & 0xFC | 0x02;
    Ok(bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(":"))
}

impl Host {
    /// The store and the cache where the environment or the system put them, and this program
    /// as the one that runs machines.
    pub fn open() -> Result<Self> {
        Ok(Self {
            store: Store::new(crate::home_directory()?),
            cache: crate::cache_directory()?,
            program: std::env::current_exe().context("find this program")?,
        })
    }

    /// Has `program` run the machines instead of this one. It has to call
    /// [`process::run`](crate::process::run) when its first argument is
    /// [`process::ARGUMENT`](crate::process::ARGUMENT).
    pub fn with_program(mut self, program: impl Into<PathBuf>) -> Self {
        self.program = program.into();
        self
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    /// The images that a machine can be created with on this computer: the catalog's, then those
    /// made from machines. Automatic ones only with `all`.
    pub fn images(&self, all: bool) -> Vec<ImageSummary> {
        let mut found: Vec<ImageSummary> = images::catalog()
            .into_iter()
            .map(|image| ImageSummary {
                cached: download::cached(&self.cache, &image).is_some(),
                id: image.id,
                name: image.name,
                kind: ImageKind::Catalog,
                size: None,
                user: None,
                created: None,
            })
            .collect();
        found.extend(
            saved::all(&self.store)
                .into_iter()
                .filter(|image| all || image.kind != ImageKind::Automatic)
                .map(|image| {
                    let layer = image.hash().ok().map(|hash| layers::path(&self.store, &hash));
                    ImageSummary {
                        name: image.description(),
                        cached: layer.as_ref().is_some_and(|layer| layer.is_file()),
                        size: layer.as_deref().map(allocated),
                        id: image.name,
                        kind: image.kind,
                        user: Some(image.user),
                        created: Some(image.created),
                    }
                }),
        );
        found
    }

    pub fn list(&self) -> Result<Vec<Machine>> {
        let names = self.store.names()?;
        Ok(names
            .iter()
            .map(|name| summary(&self.store, &self.store.directory(name), name))
            .collect())
    }

    pub fn machine(&self, name: &str) -> Result<Machine> {
        Ok(summary(&self.store, &self.store.find(name)?, name))
    }

    /// The image called `name`: of the catalog, or one made from a machine.
    fn source(&self, name: &str) -> Result<Source> {
        let catalog = images::find(name);
        if let Ok(image) = catalog {
            return Ok(Source::Catalog(image));
        }
        match saved::find(&self.store, name)? {
            Some(image) => Ok(Source::Saved(image)),
            None => Err(Error::new(
                ErrorCode::NotFound,
                format!("There is no image called \"{name}\". `{NAME} images` lists them."),
            )),
        }
    }

    /// Creates a machine, downloading its image unless the cache has it. The machine isn't
    /// started.
    ///
    /// A machine of a catalog image with setup scripts starts from the automatic image that the
    /// first machine of that image, those scripts and that account left, when there is one: its
    /// scripts have run already.
    pub fn create(&self, options: CreateOptions, progress: &mut dyn FnMut(Progress)) -> Result<Machine> {
        let source = self.source(&options.image)?;
        if let Some(name) = &options.name {
            store::check_name(name)?;
            // Said now rather than after the download. Whether it is still so is seen at the end.
            if self.store.directory(name).exists() {
                return Err(store::already_exists(name));
            }
        }
        let mut shares = Vec::new();
        for folder in &options.shares {
            shares::add(
                &mut shares,
                shares::new(folder, None, false)?,
                options.name.as_deref().unwrap_or(&options.image),
            )?;
        }
        let scripts = options
            .scripts
            .iter()
            .map(|chosen| Script::find(chosen))
            .collect::<Result<Vec<_>>>()?;
        let invalid = |what: &str| Err(Error::new(ErrorCode::InvalidArgument, what));
        let cpus = options.cpus.unwrap_or_else(|| {
            let available = std::thread::available_parallelism().map_or(1, |count| count.get() as u32);
            DEFAULT_CPUS.min(available)
        });
        if cpus == 0 {
            return invalid("A machine needs at least one processor.");
        }
        if options.nested {
            engine::check_nested()?;
        }
        let memory = options.memory.unwrap_or(DEFAULT_MEMORY);
        if memory < SMALLEST_MEMORY {
            return invalid("A machine needs at least 256 MiB of memory.");
        }
        // Whole mebibytes: a disk's size has to be a number of sectors, whatever their size.
        let asked = match options.disk.map(|disk| disk.checked_next_multiple_of(1 << 20)) {
            Some(None) => return invalid("The disk is too large."),
            Some(Some(disk)) => Some(disk),
            None => None,
        };
        engine::prepare(&self.cache, &mut |received, total| {
            progress(Progress::DownloadingEngine { received, total })
        })?;

        // The sealed layer that the machine's disk is built on, and whether the system on it has
        // run before as another machine.
        let (parent, user, disk, automatic, copy) = match &source {
            Source::Saved(image) => {
                if image.arch != ARCH {
                    return Err(Error::new(
                        ErrorCode::Unsupported,
                        format!(
                            "\"{}\" was made for {} processors, and this computer's is {ARCH}.",
                            image.name, image.arch
                        ),
                    ));
                }
                if !scripts.is_empty() {
                    return Err(Error::new(
                        ErrorCode::Unsupported,
                        "Setup scripts run on machines of the catalog's images. Run them in this one with exec once it has started.",
                    ));
                }
                if options.user.as_ref().is_some_and(|user| *user != image.user) {
                    return Err(Error::new(
                        ErrorCode::InvalidArgument,
                        format!(
                            "Machines made from \"{}\" have its account, \"{}\".",
                            image.name, image.user
                        ),
                    ));
                }
                // The note may come from another computer: what it says is checked.
                seed::check_user(&image.user)?;
                let parent = image.hash()?;
                let disk = asked.unwrap_or(image.disk_size);
                (parent, image.user.clone(), disk, None, true)
            }
            Source::Catalog(image) => {
                let user = match &options.user {
                    Some(user) => user.clone(),
                    None => seed::user_from(&std::env::var("USER").unwrap_or_default()),
                };
                seed::check_user(&user)?;
                let disk = asked.unwrap_or(DEFAULT_DISK);
                let file = download::fetch(&self.cache, image, &mut |received, total| {
                    progress(Progress::Downloading { received, total })
                })?;
                let base = layers::base(&self.store, image, &file, &mut |fraction| {
                    progress(Progress::WritingDisk { fraction })
                })?;
                if scripts.is_empty() {
                    (base, user, disk, None, false)
                } else {
                    let key = saved::automatic_key(&base, &scripts, &user);
                    let made = saved::find(&self.store, &saved::automatic_name(&key))?.filter(|made| {
                        made.key.as_deref() == Some(key.as_str())
                            && made.arch == ARCH
                            && made
                                .hash()
                                .is_ok_and(|hash| layers::path(&self.store, &hash).is_file())
                    });
                    match made {
                        Some(made) if made.disk_size <= disk => (made.hash()?, user, disk, None, true),
                        // One for a larger disk: this machine installs, and its disk doesn't
                        // become another of the same kind.
                        Some(_) => (base, user, disk, None, false),
                        None => {
                            let automatic = Automatic {
                                key,
                                scripts: scripts.iter().map(|script| script.name.clone()).collect(),
                            };
                            (base, user, disk, Some(automatic), false)
                        }
                    }
                }
            }
        };
        let parent_path = layers::path(&self.store, &parent);
        let parent_size = disk::Header::read(&parent_path)
            .map_err(|error| {
                Error::new(
                    ErrorCode::NotFound,
                    format!(
                        "The image's layer {} can't be read: {error}.",
                        parent_path.display()
                    ),
                )
            })?
            .capacity;
        if parent_size > disk {
            return Err(Error::new(
                ErrorCode::InvalidArgument,
                format!("The image needs a disk of {parent_size} bytes, and {disk} were asked for."),
            ));
        }
        let mut staged = self.store.stage()?;
        // The guest grows its system into the room that is left when it first starts.
        let layer = staged.directory.join(DISK_FILE);
        disk::Layer::create(&layer, disk, Some(parent)).at("create", &layer)?;
        // An image deleted in the meantime took its layer along. With the store's lock held, a
        // sweep has either removed it already, or sees the new disk that is built on it.
        let gone = {
            let _changing = self.store.lock()?;
            !parent_path.is_file()
        };
        if gone {
            return Err(Error::new(
                ErrorCode::NotFound,
                format!("The image \"{}\" was deleted in the meantime.", options.image),
            ));
        }
        let mut config = Config {
            version: store::CONFIG_VERSION,
            id: random_hex(16)?,
            name: String::new(),
            image: options.image.clone(),
            arch: ARCH.to_string(),
            cpus,
            memory,
            disk: DiskConfig {
                file: DISK_FILE.to_string(),
                format: DiskFormat::Layer,
                size: disk,
            },
            mac_address: mac_address()?,
            user,
            created: now(),
            snapshots: Vec::new(),
            automatic,
            nested: options.nested,
            shares,
        };

        // A machine without a name is called after its image: debian-13, debian-13-2 and so on.
        let base: String = options
            .image
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let mut number = 1;
        loop {
            let name = match &options.name {
                Some(name) => name.clone(),
                None if number == 1 => base.clone(),
                None => format!("{base}-{number}"),
            };
            number += 1;
            if options.name.is_none() && self.store.directory(&name).exists() {
                continue;
            }
            config.name = name.clone();
            let seed = Seed {
                host_name: &name,
                instance_id: &config.id,
                user: &config.user,
                password: options.password.as_deref(),
                mac_address: &config.mac_address,
                agent: AGENT,
                uid: account_id(),
                // A system that has run before has run its scripts.
                scripts: if copy { &[] } else { &scripts },
            };
            seed.write(&staged.directory.join(SEED_FILE))?;
            config.save(&staged.directory)?;
            match self.store.publish(&mut staged, &name) {
                Ok(directory) => return Ok(summary(&self.store, &directory, &name)),
                // Someone else took the name in the meantime. Another one will do.
                Err(error) if error.code == ErrorCode::AlreadyExists && options.name.is_none() => {}
                Err(error) => return Err(error),
            }
        }
    }

    /// Starts the machine and returns once it has started, which is before its system is up. A
    /// machine that is running already is left as it is.
    pub fn start(&self, name: &str) -> Result<Machine> {
        let directory = self.store.find(name)?;
        let config = Config::load(&directory)?;
        if client::is_running(&directory) {
            match client::status(&directory) {
                Ok(_) => return self.machine(name),
                // The lock was someone else's, who has let go of it: a start that failed, or a
                // deletion, after which the machine is no longer found.
                Err(error) if error.code == ErrorCode::NotRunning => {
                    self.store.find(name)?;
                }
                Err(error) => return Err(error),
            }
        }
        if config.arch != ARCH {
            return Err(Error::new(
                ErrorCode::Unsupported,
                format!(
                    "\"{name}\" was made for {} processors, and this computer's is {ARCH}.",
                    config.arch
                ),
            ));
        }
        engine::prepare(&self.cache, &mut |_, _| {})?;
        match client::start(&self.program, &directory) {
            // Someone else was starting it at the same moment, which is as good if they managed.
            Err(error) if error.code == ErrorCode::Running => match client::status(&directory) {
                Ok(_) => {}
                Err(error) if error.code == ErrorCode::NotRunning => {
                    return Err(Error::new(
                        ErrorCode::Engine,
                        format!(
                            "\"{name}\" was being started by someone else, and didn't start. {} may say why.",
                            directory.join("machine.log").display()
                        ),
                    ));
                }
                Err(error) => return Err(error),
            },
            started => started?,
        }
        self.machine(name)
    }

    /// Turns the machine off: asks its guest to shut down, and cuts the power if it hasn't after
    /// `timeout`, or at once with `force`. A machine that isn't running is left as it is.
    pub fn stop(&self, name: &str, force: bool, timeout: Option<Duration>) -> Result<Machine> {
        let directory = self.store.find(name)?;
        if client::is_running(&directory) {
            client::stop(&directory, force, timeout)?;
        }
        self.machine(name)
    }

    /// Deletes the machine and its disk. One that is running is only deleted with `force`, which
    /// cuts its power first.
    pub fn delete(&self, name: &str, force: bool) -> Result<()> {
        let directory = self.store.find(name)?;
        let lock = directory.join(LOCK);
        // Holding the lock keeps the machine from starting while its files go.
        let take = |limit| {
            Lock::take_within(&lock, limit).or_else(|error| {
                // Someone else deleted the machine in the meantime: then it is no longer found.
                self.store.find(name)?;
                Err(error).at("lock", &lock)
            })
        };
        let stopped = match take(Duration::from_millis(200))? {
            Some(stopped) => stopped,
            None if force => {
                client::stop(&directory, true, None)?;
                take(Duration::from_secs(10))?.ok_or_else(|| {
                    Error::new(
                        ErrorCode::Running,
                        format!("\"{name}\" is still running, and can't be deleted."),
                    )
                })?
            }
            None => {
                return Err(Error::new(
                    ErrorCode::Running,
                    format!("\"{name}\" is running. Stop it first, or delete it with --force."),
                ));
            }
        };
        self.remove(&directory, stopped)
    }

    /// Removes the machine in `directory`, which `stopped` keeps from starting, and the layers
    /// that only it used.
    fn remove(&self, directory: &Path, stopped: Lock) -> Result<()> {
        // Sockets that had no room in the machine's folder aren't removed with it.
        if let Some(process) = client::process_file(directory)
            && !process.socket.starts_with(directory)
        {
            _ = std::fs::remove_file(process.socket);
        }
        if let Ok(config) = Config::load(directory) {
            let shared = config
                .shares
                .iter()
                .map(|share| engine::share_socket_name(&share.name));
            let names = ["disk", "vnc", "api", "vsock", "network"].map(String::from);
            for name in names.into_iter().chain(shared) {
                if let Ok(socket) = socket_path(directory, &config.id, &name)
                    && !socket.starts_with(directory)
                {
                    // passt makes one more next to its own, and virtiofsd one that holds its
                    // process ID.
                    for suffix in [".repair", ".pid"] {
                        let mut beside = socket.as_os_str().to_owned();
                        beside.push(suffix);
                        _ = std::fs::remove_file(beside);
                    }
                    _ = std::fs::remove_file(socket);
                }
            }
        }
        self.store.remove(directory, stopped)?;
        // The machine is gone either way. What it leaves goes with the next sweep.
        _ = layers::sweep(&self.store);
        Ok(())
    }

    /// Shares the folder at `folder` with the machine, as `/mnt/shared/<name>` in the guest: as
    /// `share_name`, or under the folder's last part. A running machine gets it at once.
    pub fn share(
        &self,
        name: &str,
        folder: &Path,
        share_name: Option<&str>,
        read_only: bool,
    ) -> Result<ShareInfo> {
        let directory = self.store.find(name)?;
        let share = shares::new(folder, share_name, read_only)?;
        Config::update(&directory, |config| {
            shares::add(&mut config.shares, share.clone(), name)
        })?;
        match self.apply_shares(&directory) {
            Ok(true) => Ok(ShareInfo::of(&share)),
            // Its process can't take it while it runs: it is shared from the next start.
            Ok(false) => Err(started_by_older_build(name)),
            // A machine that can't take it doesn't keep it.
            Err(error) => {
                _ = Config::update(&directory, |config| {
                    config.shares.retain(|taken| taken.name != share.name);
                    Ok(())
                });
                _ = self.apply_shares(&directory);
                Err(error)
            }
        }
    }

    /// Stops sharing the folder that the machine has as `share_name`. A running machine lets
    /// go of it at once.
    pub fn unshare(&self, name: &str, share_name: &str) -> Result<()> {
        let directory = self.store.find(name)?;
        Config::update(&directory, |config| {
            if !config.shares.iter().any(|share| share.name == share_name) {
                return Err(Error::new(
                    ErrorCode::NotFound,
                    format!("\"{name}\" shares no folder called \"{share_name}\"."),
                ));
            }
            config.shares.retain(|share| share.name != share_name);
            Ok(())
        })?;
        if !self.apply_shares(&directory)? {
            return Err(started_by_older_build(name));
        }
        Ok(())
    }

    /// Has the machine's process share what the machine's settings say, if it runs. One that
    /// starts meanwhile reads the settings itself. `false` when its process is too old to.
    fn apply_shares(&self, directory: &Path) -> Result<bool> {
        if !client::is_running(directory) {
            return Ok(true);
        }
        match client::shares(directory) {
            Err(error) if error.code == ErrorCode::NotRunning => Ok(true),
            other => other,
        }
    }

    /// Shuts the machine down if it runs, and holds its lock so that it doesn't start until the
    /// lock is let go of. Says what the machine is made of, and whether it was running.
    fn hold_stopped(&self, name: &str) -> Result<Held> {
        let directory = self.store.find(name)?;
        let config = Config::load(&directory)?;
        if config.disk.format != DiskFormat::Layer {
            return Err(Error::new(
                ErrorCode::Unsupported,
                format!("\"{name}\" has a raw disk, which has no layers."),
            ));
        }
        let was_running = running_process(&directory).is_some();
        if was_running {
            client::stop(&directory, false, None)?;
        }
        let lock = directory.join(LOCK);
        let stopped = Lock::take_within(&lock, Duration::from_secs(10))
            .or_else(|error| {
                self.store.find(name)?;
                Err(error).at("lock", &lock)
            })?
            .ok_or_else(|| {
                Error::new(
                    ErrorCode::Running,
                    format!("\"{name}\" is being started or deleted by someone else."),
                )
            })?;
        // While the lock was waited for, a deletion may have taken the folder away.
        self.store.find(name)?;
        let config = Config::load(&directory)?;
        let top = directory.join(&config.disk.file);
        layers::recover(&self.store, &top)?;
        Ok(Held {
            directory,
            config,
            top,
            lock: stopped,
            was_running,
        })
    }

    /// Lets go of a machine that was held stopped, and starts it again if it was running.
    fn release(&self, name: &str, held: Held) -> Result<()> {
        drop(held.lock);
        if held.was_running {
            self.start(name)?;
        }
        Ok(())
    }

    /// Lets go of a machine that was held stopped for something that came out as `result`, and
    /// starts it again if it was running, whether that worked or not.
    fn release_after<T>(&self, name: &str, held: Held, result: Result<T>) -> Result<T> {
        let released = self.release(name, held);
        let value = result?;
        released?;
        Ok(value)
    }

    /// Seals the machine's disk as it is, as a snapshot it can go back to. A machine that is
    /// running is shut down for it, and started again.
    pub fn snapshot(&self, name: &str, snapshot: &str) -> Result<SnapshotInfo> {
        check_snapshot_name(snapshot)?;
        let taken_already = |config: &Config| {
            if config.snapshots.iter().any(|taken| taken.name == snapshot) {
                return Err(Error::new(
                    ErrorCode::AlreadyExists,
                    format!("\"{name}\" has a snapshot called \"{snapshot}\" already."),
                ));
            }
            Ok(())
        };
        taken_already(&Config::load(&self.store.find(name)?)?)?;
        let held = self.hold_stopped(name)?;
        let taken = (|| {
            taken_already(&held.config)?;
            let hash = layers::seal_top(&self.store, &held.top)?;
            let taken = Snapshot {
                name: snapshot.to_string(),
                layer: disk::layer::to_hex(&hash),
                created: now(),
            };
            Config::update(&held.directory, |config| {
                taken_already(config)?;
                config.snapshots.push(taken.clone());
                Ok(())
            })?;
            Ok(taken)
        })();
        self.release_after(name, held, taken)
            .map(|taken| self.snapshot_info(&taken))
    }

    fn snapshot_info(&self, snapshot: &Snapshot) -> SnapshotInfo {
        SnapshotInfo {
            name: snapshot.name.clone(),
            created: snapshot.created,
            size: disk::layer::from_hex(&snapshot.layer)
                .map_or(0, |hash| allocated(&layers::path(&self.store, &hash))),
        }
    }

    /// The machine's snapshots, oldest first.
    pub fn snapshots(&self, name: &str) -> Result<Vec<SnapshotInfo>> {
        let config = Config::load(&self.store.find(name)?)?;
        Ok(config
            .snapshots
            .iter()
            .map(|snapshot| self.snapshot_info(snapshot))
            .collect())
    }

    fn find_snapshot(&self, name: &str, snapshot: &str) -> Result<Snapshot> {
        let config = Config::load(&self.store.find(name)?)?;
        config
            .snapshots
            .into_iter()
            .find(|taken| taken.name == snapshot)
            .ok_or_else(|| no_snapshot(name, snapshot))
    }

    /// Puts the machine's disk back as it was when the snapshot was taken. What was written since
    /// is lost; the machine's other snapshots stay. A machine that is running is shut down for
    /// it, and started again.
    pub fn revert(&self, name: &str, snapshot: &str) -> Result<()> {
        self.find_snapshot(name, snapshot)?;
        let held = self.hold_stopped(name)?;
        let reverted = (|| {
            // Looked up again, now that nobody else can change the machine's disk: the snapshot
            // may have been deleted in the meantime.
            let taken = held
                .config
                .snapshots
                .iter()
                .find(|taken| taken.name == snapshot)
                .ok_or_else(|| no_snapshot(name, snapshot))?;
            let hash = disk::layer::from_hex(&taken.layer).ok_or_else(|| {
                Error::new(
                    ErrorCode::Io,
                    format!("The snapshot \"{snapshot}\" names no layer it can be found by."),
                )
            })?;
            // With the store's lock held, so that a sweep sees the new layer once its layer is
            // checked, or has removed it before.
            let _changing = self.store.lock()?;
            let sealed = layers::path(&self.store, &hash);
            if !sealed.is_file() {
                return Err(Error::new(
                    ErrorCode::NotFound,
                    format!(
                        "The layer of the snapshot \"{snapshot}\" is missing: {}.",
                        sealed.display()
                    ),
                ));
            }
            let mut next = held.top.as_os_str().to_owned();
            next.push(".new");
            let next = PathBuf::from(next);
            _ = std::fs::remove_file(&next);
            disk::Layer::create(&next, held.config.disk.size, Some(hash)).at("create", &next)?;
            std::fs::rename(&next, &held.top).at("write", &held.top)?;
            layers::sync_folder(&held.directory)
        })();
        self.release_after(name, held, reverted)?;
        _ = layers::sweep(&self.store);
        Ok(())
    }

    /// Forgets a snapshot. Its layer stays for as long as the disk is built on it.
    pub fn delete_snapshot(&self, name: &str, snapshot: &str) -> Result<()> {
        let directory = self.store.find(name)?;
        Config::update(&directory, |config| {
            if !config.snapshots.iter().any(|taken| taken.name == snapshot) {
                return Err(no_snapshot(name, snapshot));
            }
            config.snapshots.retain(|taken| taken.name != snapshot);
            Ok(())
        })?;
        _ = layers::sweep(&self.store);
        Ok(())
    }

    /// Makes an image called `image` of the machine's disk: its layers in one, compressed
    /// harder. A machine that is running is shut down for it, and started again unless `delete`
    /// says to delete it once the image is made. An image that can't be made deletes nothing.
    pub fn save_image(
        &self,
        name: &str,
        image: &str,
        delete: bool,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<ImageSummary> {
        saved::check_new_name(&self.store, image)?;
        let held = self.hold_stopped(name)?;
        let flattened = layers::flatten(&self.store, &held.top, SAVE_LEVEL, &mut |fraction| {
            progress(Progress::Compressing { fraction })
        });
        let partial = match flattened {
            Ok(partial) => partial,
            Err(error) => return self.release_after(name, held, Err(error)),
        };
        let from = match images::find(&held.config.image) {
            Ok(_) => held.config.image.clone(),
            Err(_) => saved::find(&self.store, &held.config.image)
                .ok()
                .flatten()
                .map_or_else(|| held.config.image.clone(), |made| made.from),
        };
        let published = (|| {
            let _changing = self.store.lock()?;
            // Someone may have taken the name while the disk was compressed.
            saved::check_new_name(&self.store, image)?;
            let hash = layers::publish(&self.store, &partial)?;
            saved::write(
                &self.store,
                &SavedImage {
                    name: image.to_string(),
                    kind: ImageKind::Saved,
                    layer: disk::layer::to_hex(&hash),
                    arch: held.config.arch.clone(),
                    user: held.config.user.clone(),
                    disk_size: held.config.disk.size,
                    from,
                    scripts: Vec::new(),
                    key: None,
                    created: now(),
                },
            )
        })();
        if let Err(error) = published {
            _ = std::fs::remove_file(&partial);
            // The machine stays as it was, running again if it was.
            return self.release_after(name, held, Err(error));
        }
        if delete {
            self.remove(&held.directory, held.lock)?;
        } else {
            self.release(name, held)?;
        }
        self.image_summary(image)
    }

    fn image_summary(&self, image: &str) -> Result<ImageSummary> {
        self.images(true)
            .into_iter()
            .find(|summary| summary.id == image)
            .ok_or_else(|| no_image(image))
    }

    /// An image made from a machine, which can be changed.
    fn own_image(&self, image: &str) -> Result<SavedImage> {
        if images::find(image).is_ok() {
            return Err(Error::new(
                ErrorCode::InvalidArgument,
                format!("\"{image}\" is an image of the catalog, which can't be changed."),
            ));
        }
        saved::find(&self.store, image)?.ok_or_else(|| no_image(image))
    }

    /// Deletes an image made from a machine. One that machines are built on is kept.
    pub fn delete_image(&self, image: &str) -> Result<()> {
        let found = self.own_image(image)?;
        let hash = found.hash()?;
        {
            let _changing = self.store.lock()?;
            let machines = layers::dependents(&self.store, &hash);
            if !machines.is_empty() {
                return Err(Error::new(
                    ErrorCode::InUse,
                    format!(
                        "The image \"{image}\" is in use by {}. Delete them first.",
                        machines.join(", ")
                    ),
                ));
            }
            saved::remove(&self.store, image)?;
        }
        _ = layers::sweep(&self.store);
        Ok(())
    }

    /// Gives an image made from a machine another name. An automatic one keeps its own: it is
    /// found by it.
    pub fn rename_image(&self, image: &str, name: &str) -> Result<ImageSummary> {
        let found = self.own_image(image)?;
        if found.kind == ImageKind::Automatic {
            return Err(Error::new(
                ErrorCode::InvalidArgument,
                "An automatic image keeps its name. Save a machine made from it as an image of its own.",
            ));
        }
        {
            let _changing = self.store.lock()?;
            saved::check_new_name(&self.store, name)?;
            saved::rename(&self.store, &found, name)?;
        }
        self.image_summary(name)
    }

    /// Writes an image made from a machine to one file, for another computer to import.
    pub fn export_image(&self, image: &str, file: &Path) -> Result<()> {
        let found = self.own_image(image)?;
        let layer = layers::path(&self.store, &found.hash()?);
        let mut partial = file.as_os_str().to_owned();
        partial.push(".partial");
        let partial = PathBuf::from(partial);
        let written = (|| -> std::io::Result<()> {
            let output = std::fs::File::create(&partial)?;
            let mut builder = tar::Builder::new(std::io::BufWriter::new(output));
            let note = serde_json::to_vec_pretty(&SavedImage {
                kind: ImageKind::Saved,
                key: None,
                ..found.clone()
            })
            .expect("plain data");
            let mut header = tar::Header::new_gnu();
            header.set_size(note.len() as u64);
            header.set_mode(0o644);
            header.set_mtime(found.created);
            header.set_cksum();
            builder.append_data(&mut header, EXPORTED_NOTE, note.as_slice())?;
            let mut source = std::fs::File::open(&layer)?;
            let mut header = tar::Header::new_gnu();
            header.set_size(source.metadata()?.len());
            header.set_mode(0o644);
            header.set_mtime(found.created);
            header.set_cksum();
            builder.append_data(&mut header, EXPORTED_LAYER, &mut source)?;
            builder
                .into_inner()?
                .into_inner()
                .map_err(|error| error.into_error())?
                .sync_all()
        })();
        if let Err(error) = written {
            _ = std::fs::remove_file(&partial);
            return Err(error).at("write", file);
        }
        std::fs::rename(&partial, file).at("write", file)
    }

    /// Reads an image that another computer exported, under `name`, or the name it had there.
    pub fn import_image(
        &self,
        file: &Path,
        name: Option<&str>,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<ImageSummary> {
        let not_an_image = |why: &str| {
            Error::new(
                ErrorCode::Image,
                format!("{} isn't an image that {NAME} exported: {why}.", file.display()),
            )
        };
        let input = std::fs::File::open(file).at("read", file)?;
        let total = input.metadata().at("read", file)?.len();
        let mut archive = tar::Archive::new(std::io::BufReader::new(input));
        let mut entries = archive.entries().at("read", file)?;
        let mut next = |expected: &str| -> Result<tar::Entry<'_, std::io::BufReader<std::fs::File>>> {
            let entry = entries
                .next()
                .ok_or_else(|| not_an_image(&format!("it has no {expected}")))?
                .at("read", file)?;
            if entry.path().ok().as_deref() != Some(Path::new(expected)) {
                return Err(not_an_image(&format!("{expected} isn't where it should be")));
            }
            Ok(entry)
        };
        let mut note_entry = next(EXPORTED_NOTE)?;
        let mut note = Vec::new();
        (&mut note_entry)
            .take(1 << 20)
            .read_to_end(&mut note)
            .at("read", file)?;
        drop(note_entry);
        let note: SavedImage =
            serde_json::from_slice(&note).map_err(|error| not_an_image(&error.to_string()))?;
        if note.arch != ARCH {
            return Err(Error::new(
                ErrorCode::Unsupported,
                format!(
                    "The image was made for {} processors, and this computer's is {ARCH}.",
                    note.arch
                ),
            ));
        }
        // The rest of what the file says is checked too: it comes from someone else.
        let mut note = note;
        seed::check_user(&note.user).map_err(|_| not_an_image("its account's name can't be used"))?;
        if note.disk_size == 0 || !note.disk_size.is_multiple_of(1 << 20) {
            return Err(not_an_image("its disk's size isn't a number of mebibytes"));
        }
        note.from = crate::scripts::line(&note.from, 120);
        note.scripts = note
            .scripts
            .iter()
            .map(|script| crate::scripts::line(script, 120))
            .collect();
        let expected = note.hash()?;
        let name = name.unwrap_or(&note.name).to_string();
        saved::check_new_name(&self.store, &name)?;

        let folder = layers::directory(&self.store);
        std::fs::create_dir_all(&folder).at("create", &folder)?;
        let partial = folder.join(format!(".import-{}.layer", random_hex(8)?));
        let result = (|| {
            let mut layer = next(EXPORTED_LAYER)?;
            let mut output = std::fs::File::create(&partial).at("create", &partial)?;
            let mut buffer = vec![0; 1 << 20];
            let mut copied = 0u64;
            loop {
                let count = layer.read(&mut buffer).at("read", file)?;
                if count == 0 {
                    break;
                }
                output.write_all(&buffer[..count]).at("write", &partial)?;
                copied += count as u64;
                progress(Progress::Importing {
                    fraction: copied as f64 / total.max(1) as f64,
                });
            }
            drop(output);
            let header = disk::Header::read(&partial).map_err(|error| not_an_image(&error.to_string()))?;
            if !header.sealed || header.parent.is_some() {
                return Err(not_an_image("its layer isn't one sealed layer of its own"));
            }
            if header.capacity > note.disk_size {
                return Err(not_an_image("its layer is larger than its disk"));
            }
            if disk::layer::hash_file(&partial).at("read", &partial)? != expected {
                return Err(not_an_image("its layer was damaged on the way"));
            }
            let opened = disk::Layer::open(&partial, &disk::Options::read_only(), None)
                .map_err(|error| not_an_image(&error.to_string()))?;
            let problems = opened.verify().at("read", &partial)?;
            if let Some(problem) = problems.first() {
                return Err(not_an_image(&format!("its layer is damaged: {problem}")));
            }
            drop(opened);
            let _changing = self.store.lock()?;
            saved::check_new_name(&self.store, &name)?;
            let hash = layers::publish(&self.store, &partial)?;
            saved::write(
                &self.store,
                &SavedImage {
                    name: name.clone(),
                    kind: ImageKind::Saved,
                    layer: disk::layer::to_hex(&hash),
                    key: None,
                    ..note
                },
            )
        })();
        if let Err(error) = result {
            _ = std::fs::remove_file(&partial);
            return Err(error);
        }
        progress(Progress::Importing { fraction: 1.0 });
        self.image_summary(&name)
    }

    /// A connection to the machine's serial console: what the guest prints comes out of it, and
    /// what is written to it is typed to the guest.
    pub fn console(&self, name: &str) -> Result<UnixStream> {
        client::console(&self.store.find(name)?)
    }

    /// The folder of the machine called `name`, and the account it was created with.
    fn guest_account(&self, name: &str, user: Option<&str>) -> Result<(PathBuf, String)> {
        let directory = self.store.find(name)?;
        let user = match user {
            Some(user) => user.to_string(),
            None => Config::load(&directory)?.user,
        };
        Ok((directory, user))
    }

    /// Runs a command in the guest and returns how it ended. What it prints goes to `output`
    /// and `errors`. `input` is passed to it from a thread of its own, which is left to itself if
    /// it is still reading when the command ends. Without it, the command's input is empty.
    pub fn exec(
        &self,
        name: &str,
        options: ExecOptions,
        input: Option<Box<dyn Read + Send>>,
        output: &mut dyn Write,
        errors: &mut dyn Write,
    ) -> Result<Ended> {
        let (directory, user) = self.guest_account(name, options.user.as_deref())?;
        let job = Job::Exec(Exec {
            arguments: options.arguments,
            user,
            directory: options.directory,
            environment: options.environment,
            // A part of a second is a second: none at all would mean no limit.
            timeout: options.timeout.map(|timeout| timeout.as_secs_f64().ceil() as u64),
        });
        let (stream, _) = guest::start(&directory, &job)?;
        run_job(&stream, input, output, errors)
    }

    /// Copies the file or folder at `source` on this computer to `destination` in the guest, as
    /// `cp -R` does. A relative destination is taken from the account's home folder. The copy
    /// belongs to the account: the machine's own unless said.
    pub fn copy_to_guest(
        &self,
        name: &str,
        source: &Path,
        destination: &str,
        user: Option<&str>,
    ) -> Result<()> {
        // Said now, rather than by a stream that fails half way.
        std::fs::symlink_metadata(source).at("read", source)?;
        let (directory, user) = self.guest_account(name, user)?;
        let job = Job::CopyIn {
            path: destination.to_string(),
            user,
        };
        let (stream, _) = guest::start(&directory, &job)?;
        send_copy(&stream, source)
    }

    /// Copies the file or folder at `source` in the guest to `destination` on this computer, as
    /// `cp -R` does. A relative source is taken from the account's home folder, and read as the
    /// account: the machine's own unless said.
    pub fn copy_from_guest(
        &self,
        name: &str,
        source: &str,
        destination: &Path,
        user: Option<&str>,
    ) -> Result<()> {
        let (directory, user) = self.guest_account(name, user)?;
        let job = Job::CopyOut {
            path: source.to_string(),
            user,
        };
        let (stream, _) = guest::start(&directory, &job)?;
        receive_copy(&stream, source, destination)
    }

    /// A connection to a TCP port of the guest itself, as a program there would make to
    /// `localhost`.
    pub fn tunnel(&self, name: &str, port: u16) -> Result<UnixStream> {
        let directory = self.store.find(name)?;
        guest::start(&directory, &Job::Tunnel { port }).map(|(stream, _)| stream)
    }

    /// What the guest's agent says about the guest.
    pub fn guest(&self, name: &str) -> Result<GuestInfo> {
        guest::info(&self.store.find(name)?)
    }

    /// Waits until the guest has started, its agent answers and its setup scripts have run, for
    /// `timeout` at most. `progress` hears what the agent says on the way.
    pub fn wait(
        &self,
        name: &str,
        timeout: Option<Duration>,
        progress: &mut dyn FnMut(&GuestInfo),
    ) -> Result<GuestInfo> {
        let directory = self.store.find(name)?;
        let deadline = timeout.map(|timeout| Instant::now() + timeout);
        loop {
            if running_process(&directory).is_none() {
                return Err(Error::new(
                    ErrorCode::NotRunning,
                    format!("\"{name}\" isn't running."),
                ));
            }
            match guest::info(&directory) {
                Ok(info) => {
                    progress(&info);
                    match &info.setup {
                        Some(setup) if setup.state == SetupState::Failed => {
                            let at = setup
                                .step
                                .as_ref()
                                .map_or(String::new(), |step| format!(" at \"{step}\""));
                            return Err(Error::new(
                                ErrorCode::Guest,
                                format!(
                                    "The setup of \"{name}\" failed{at}. /var/log/{NAME}-setup.log in the guest says why."
                                ),
                            ));
                        }
                        Some(setup) if setup.state != SetupState::Finished => {}
                        _ if info.booted => return Ok(info),
                        _ => {}
                    }
                }
                // The guest is starting, or restarting for its setup.
                Err(error) if matches!(error.code, ErrorCode::Guest | ErrorCode::Io) => {}
                Err(error) => return Err(error),
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Err(Error::new(
                    ErrorCode::TimedOut,
                    format!("\"{name}\" wasn't ready in time."),
                ));
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    /// The layer file of a disk: of the machine called `target`, or the file at that path if it
    /// has a `/` in it or ends in `.layer`. Says which machine, and whether it is running.
    fn disk_target(&self, target: &str) -> Result<(Option<String>, PathBuf, bool)> {
        if target.contains('/') || target.ends_with(".layer") {
            let path = PathBuf::from(target);
            if !path.is_file() {
                return Err(Error::new(
                    ErrorCode::NotFound,
                    format!("There is no layer at {target}."),
                ));
            }
            return Ok((None, path, false));
        }
        let directory = self.store.find(target)?;
        let config = Config::load(&directory)?;
        if config.disk.format != DiskFormat::Layer {
            return Err(Error::new(
                ErrorCode::Unsupported,
                format!("\"{target}\" has a raw disk, which has no layers."),
            ));
        }
        let running = running_process(&directory).is_some();
        Ok((
            Some(target.to_string()),
            directory.join(config.disk.file),
            running,
        ))
    }

    /// The layers of a machine's disk, or of the layer file at a path, and what each holds.
    pub fn disk_info(&self, target: &str) -> Result<DiskInfo> {
        let (machine, top, running) = self.disk_target(target)?;
        let users = layers::users(&self.store);
        let mut files = vec![top.clone()];
        files.extend(layers::chain(&self.store, &top)?);
        // A layer that is being written is left alone: what it says is changing as it is read.
        let opened = match (running, files.get(1)) {
            (false, _) => Some(layers::open(&self.store, &top, &disk::Options::read_only())?),
            (true, Some(parent)) => Some(layers::open(&self.store, parent, &disk::Options::read_only())?),
            (true, None) => None,
        };
        let mut open = std::iter::successors(opened, |layer| layer.parent().cloned());
        let mut layers = Vec::new();
        for (index, file) in files.into_iter().enumerate() {
            let header = disk::Header::read(&file).at("read", &file)?;
            let hash = named_hash(&file).filter(|_| header.sealed);
            let contents = if index == 0 && running {
                None
            } else {
                open.next().map(|layer| {
                    let stats = layer.stats();
                    LayerContents {
                        clusters: stats.clusters,
                        stored: stats.stored,
                        zeros: stats.zeros,
                        inherited: stats.inherited,
                        stored_bytes: stats.stored_bytes,
                    }
                })
            };
            layers.push(LayerInfo {
                used_by: hash.and_then(|hash| users.get(&hash)).copied().unwrap_or(0),
                hash: hash.map(|hash| disk::layer::to_hex(&hash)),
                sealed: header.sealed,
                capacity: header.capacity,
                file_bytes: allocated(&file),
                contents,
                path: file,
            });
        }
        Ok(DiskInfo { machine, layers })
    }

    /// Reads every layer of a disk back, and says what is wrong with each. A machine has to be
    /// stopped for it.
    pub fn check_disk(&self, target: &str) -> Result<DiskCheck> {
        let (_, top, running) = self.disk_target(target)?;
        if running {
            return Err(Error::new(
                ErrorCode::Running,
                format!("\"{target}\" is running. Stop it first."),
            ));
        }
        // A layer that can't be opened, its own or one it is built on, is a finding like any other.
        let opened = layers::chain(&self.store, &top).and_then(|chain| {
            let layer = layers::open(&self.store, &top, &disk::Options::read_only())?;
            Ok((chain, layer))
        });
        let mut files = vec![top.clone()];
        let layer = match opened {
            Ok((chain, layer)) => {
                files.extend(chain);
                layer
            }
            Err(error) => {
                return Ok(DiskCheck {
                    layers: vec![LayerCheck {
                        path: top,
                        problems: vec![error.message],
                    }],
                });
            }
        };
        let mut checks = Vec::new();
        let chain = std::iter::successors(Some(layer), |layer| layer.parent().cloned());
        for (file, layer) in files.into_iter().zip(chain) {
            let mut problems = layer.verify().at("read", &file)?;
            if layer.header().sealed
                && let Some(named) = named_hash(&file)
                && disk::layer::hash_file(&file).at("read", &file)? != named
            {
                problems.push("its contents don't match the hash it is named by".into());
            }
            checks.push(LayerCheck { path: file, problems });
        }
        Ok(DiskCheck { layers: checks })
    }
}

#[cfg(test)]
mod tests {
    use disk::Disk;

    use super::*;

    /// A host on a store of its own with a stopped machine `m`, whose disk holds 7s in its first
    /// cluster on top of a sealed layer of 1s.
    fn host(test: &str) -> Host {
        let root = std::env::temp_dir().join(format!("host-{test}-{}", random_hex(4).unwrap()));
        let host = Host {
            store: Store::new(&root),
            cache: root.join("cache"),
            program: PathBuf::from("/nonexistent"),
        };
        let folder = layers::directory(&host.store);
        std::fs::create_dir_all(&folder).unwrap();
        let partial = folder.join(".base.layer");
        disk::Layer::create(&partial, 4 << 20, None).unwrap();
        let layer = disk::Layer::open(&partial, &disk::Options::default(), None).unwrap();
        layer.write(0, &[1; 1 << 20]).unwrap();
        layer.seal().unwrap();
        drop(layer);
        let base = layers::publish(&host.store, &partial).unwrap();
        let mut staged = host.store.stage().unwrap();
        let top = staged.directory.join(DISK_FILE);
        disk::Layer::create(&top, 8 << 20, Some(base)).unwrap();
        let layer = layers::open(&host.store, &top, &disk::Options::default()).unwrap();
        layer.write(0, &[7; 65536]).unwrap();
        layer.close().unwrap();
        Config {
            version: store::CONFIG_VERSION,
            id: random_hex(16).unwrap(),
            name: "m".into(),
            image: "debian-13".into(),
            arch: ARCH.into(),
            cpus: 1,
            memory: 1 << 30,
            disk: DiskConfig {
                file: DISK_FILE.into(),
                format: DiskFormat::Layer,
                size: 8 << 20,
            },
            mac_address: mac_address().unwrap(),
            user: "username".into(),
            created: now(),
            snapshots: Vec::new(),
            automatic: None,
            nested: false,
            shares: Vec::new(),
        }
        .save(&staged.directory)
        .unwrap();
        host.store.publish(&mut staged, "m").unwrap();
        host
    }

    fn contents(host: &Host, machine: &str) -> Vec<u8> {
        let top = host.store.directory(machine).join(DISK_FILE);
        let layer = layers::open(&host.store, &top, &disk::Options::read_only()).unwrap();
        let mut read = vec![0; 8 << 20];
        layer.read(0, &mut read).unwrap();
        read
    }

    fn options(image: &str, name: &str) -> CreateOptions {
        CreateOptions {
            image: image.into(),
            name: Some(name.into()),
            ..CreateOptions::default()
        }
    }

    #[test]
    fn a_machine_saved_as_an_image_gives_machines_with_its_disk() {
        let host = host("save");
        let original = contents(&host, "m");
        let saved = host.save_image("m", "dev", false, &mut |_| {}).unwrap();
        assert_eq!(saved.kind, ImageKind::Saved);
        assert_eq!(saved.user.as_deref(), Some("username"));
        assert_eq!(
            host.save_image("m", "dev", false, &mut |_| {}).unwrap_err().code,
            ErrorCode::AlreadyExists
        );
        assert_eq!(
            host.save_image("m", "debian-13", false, &mut |_| {})
                .unwrap_err()
                .code,
            ErrorCode::InvalidArgument
        );

        let other_user = CreateOptions {
            user: Some("someone".into()),
            ..options("dev", "c")
        };
        assert_eq!(
            host.create(other_user, &mut |_| {}).unwrap_err().code,
            ErrorCode::InvalidArgument
        );
        let scripts = CreateOptions {
            scripts: vec!["docker".into()],
            ..options("dev", "c")
        };
        assert_eq!(
            host.create(scripts, &mut |_| {}).unwrap_err().code,
            ErrorCode::Unsupported
        );
        let small = CreateOptions {
            disk: Some(1 << 20),
            ..options("dev", "c")
        };
        assert_eq!(
            host.create(small, &mut |_| {}).unwrap_err().code,
            ErrorCode::InvalidArgument
        );
        let created = host.create(options("dev", "c"), &mut |_| {}).unwrap();
        assert_eq!(
            (created.image.as_str(), created.user.as_str()),
            ("dev", "username")
        );
        assert_eq!(created.disk_size, 8 << 20);
        assert_eq!(contents(&host, "c"), original);
        // The machine it was made of is as it was.
        assert_eq!(contents(&host, "m"), original);

        assert_eq!(host.delete_image("dev").unwrap_err().code, ErrorCode::InUse);
        assert_eq!(
            host.delete_image("debian-13").unwrap_err().code,
            ErrorCode::InvalidArgument
        );
        assert_eq!(
            host.delete_image("nothing").unwrap_err().code,
            ErrorCode::NotFound
        );
        host.rename_image("dev", "web").unwrap();
        assert!(host.images(false).iter().any(|image| image.id == "web"));
        assert!(!host.images(false).iter().any(|image| image.id == "dev"));

        let file = host.store.root().join("web.image");
        host.export_image("web", &file).unwrap();
        let imported = host.import_image(&file, Some("copy"), &mut |_| {}).unwrap();
        assert_eq!(imported.id, "copy");
        assert_eq!(
            host.import_image(&file, Some("copy"), &mut |_| {})
                .unwrap_err()
                .code,
            ErrorCode::AlreadyExists
        );
        let junk = host.store.root().join("junk.image");
        std::fs::write(&junk, b"not an image").unwrap();
        assert!(host.import_image(&junk, Some("junk"), &mut |_| {}).is_err());

        host.delete("c", false).unwrap();
        host.delete_image("web").unwrap();
        // The imported copy is the same layer, which stays for it.
        host.create(options("copy", "d"), &mut |_| {}).unwrap();
        assert_eq!(contents(&host, "d"), original);
        host.delete("d", false).unwrap();
        host.delete_image("copy").unwrap();
        // Only the layer that m is built on is left.
        let layers: Vec<_> = std::fs::read_dir(layers::directory(&host.store))
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".layer"))
            .collect();
        assert_eq!(layers.len(), 1);
        std::fs::remove_dir_all(host.store.root()).unwrap();
    }

    #[test]
    fn snapshots_are_taken_reverted_to_and_forgotten() {
        let host = host("snapshots");
        let original = contents(&host, "m");
        host.snapshot("m", "first").unwrap();
        assert_eq!(
            host.snapshot("m", "first").unwrap_err().code,
            ErrorCode::AlreadyExists
        );
        assert_eq!(
            host.snapshot("m", "Bad name").unwrap_err().code,
            ErrorCode::InvalidArgument
        );
        let top = host.store.directory("m").join(DISK_FILE);
        let layer = layers::open(&host.store, &top, &disk::Options::default()).unwrap();
        layer.write(0, &[9; 65536]).unwrap();
        layer.close().unwrap();
        assert_ne!(contents(&host, "m"), original);
        host.snapshot("m", "second").unwrap();
        let names: Vec<String> = host
            .snapshots("m")
            .unwrap()
            .into_iter()
            .map(|taken| taken.name)
            .collect();
        assert_eq!(names, ["first", "second"]);

        host.revert("m", "first").unwrap();
        assert_eq!(contents(&host, "m"), original);
        assert_eq!(host.revert("m", "third").unwrap_err().code, ErrorCode::NotFound);
        host.delete_snapshot("m", "second").unwrap();
        assert_eq!(host.snapshots("m").unwrap().len(), 1);
        std::fs::remove_dir_all(host.store.root()).unwrap();
    }
}
