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

use crate::images::{self, ARCH};
use crate::lock::Lock;
use crate::paths::socket_path;
use crate::process::SEED_FILE;
use crate::scripts::Script;
use crate::seed::{self, Seed};
use crate::store::{self, Config, DiskConfig, DiskFormat, LOCK, Store};
use crate::{
    AGENT, Context, Error, ErrorCode, NAME, Result, client, download, guest, layers, now, random_hex,
};

const DISK_FILE: &str = "disk.layer";
const DEFAULT_CPUS: u32 = 4;
const DEFAULT_MEMORY: u64 = 4 << 30;
const DEFAULT_DISK: u64 = 32 << 30;
const SMALLEST_MEMORY: u64 = 256 << 20;

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
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Progress {
    /// The image is on its way: bytes that have arrived, and of how many if the server said.
    Downloading { received: u64, total: Option<u64> },
    /// The image is being made into the layer that its machines' disks are built on, from 0 to
    /// 1. That is done once for each image.
    WritingDisk { fraction: f64 },
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
}

/// An image of the catalog, and whether it has been downloaded.
#[derive(Debug, Clone, Serialize)]
pub struct ImageSummary {
    pub id: String,
    pub name: String,
    pub cached: bool,
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
        }
        Err(error) if !running => {
            machine.state = MachineState::Broken;
            machine.problem = Some(error.message);
        }
        Err(error) => machine.problem = Some(error.message),
    }
    machine
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

    /// The images that a machine can be created with on this computer.
    pub fn images(&self) -> Vec<ImageSummary> {
        images::catalog()
            .into_iter()
            .map(|image| ImageSummary {
                cached: download::cached(&self.cache, &image).is_some(),
                id: image.id,
                name: image.name,
            })
            .collect()
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

    /// Creates a machine, downloading its image unless the cache has it. The machine isn't
    /// started.
    pub fn create(&self, options: CreateOptions, progress: &mut dyn FnMut(Progress)) -> Result<Machine> {
        let image = images::find(&options.image)?;
        if let Some(name) = &options.name {
            store::check_name(name)?;
            // Said now rather than after the download. Whether it is still so is seen at the end.
            if self.store.directory(name).exists() {
                return Err(store::already_exists(name));
            }
        }
        let user = match options.user {
            Some(user) => user,
            None => seed::user_from(&std::env::var("USER").unwrap_or_default()),
        };
        seed::check_user(&user)?;
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
        let memory = options.memory.unwrap_or(DEFAULT_MEMORY);
        if memory < SMALLEST_MEMORY {
            return invalid("A machine needs at least 256 MiB of memory.");
        }
        // Whole mebibytes: a disk's size has to be a number of sectors, whatever their size.
        let Some(disk) = options
            .disk
            .unwrap_or(DEFAULT_DISK)
            .checked_next_multiple_of(1 << 20)
        else {
            return invalid("The disk is too large.");
        };

        let file = download::fetch(&self.cache, &image, &mut |received, total| {
            progress(Progress::Downloading { received, total })
        })?;
        let base = layers::base(&self.store, &image, &file, &mut |fraction| {
            progress(Progress::WritingDisk { fraction })
        })?;
        let base_size = disk::Header::read(&layers::path(&self.store, &base))
            .at("read", &layers::path(&self.store, &base))?
            .capacity;
        if base_size > disk {
            return Err(Error::new(
                ErrorCode::InvalidArgument,
                format!("The image needs a disk of {base_size} bytes, and {disk} were asked for."),
            ));
        }
        let mut staged = self.store.stage()?;
        // The guest grows its system into the room that is left when it first starts.
        let layer = staged.directory.join(DISK_FILE);
        disk::Layer::create(&layer, disk, Some(base)).at("create", &layer)?;
        let mut config = Config {
            version: store::CONFIG_VERSION,
            id: random_hex(16)?,
            name: String::new(),
            image: image.id.clone(),
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
        };

        // A machine without a name is called after its image: debian-13, debian-13-2 and so on.
        let base: String = image
            .id
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
                scripts: &scripts,
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
        // Sockets that had no room in the machine's folder aren't removed with it.
        if let Some(process) = client::process_file(&directory)
            && !process.socket.starts_with(&directory)
        {
            _ = std::fs::remove_file(process.socket);
        }
        if let Ok(config) = Config::load(&directory)
            && let Ok(socket) = socket_path(&directory, &config.id, "disk")
            && !socket.starts_with(&directory)
        {
            _ = std::fs::remove_file(socket);
        }
        self.store.remove(&directory, stopped)
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
