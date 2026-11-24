//! Serves a [`Disk`] as a vhost-user block device, on a Unix socket.
//!
//! cloud-hypervisor takes a disk from a file, or from a vhost-user server: a process that maps
//! the guest's memory and answers the device's queues itself. That is the only way a disk can be
//! this program's own on Linux. The machine connects again each time its guest restarts.

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use vhost::vhost_user::Listener;
use vhost::vhost_user::message::{VhostUserProtocolFeatures, VhostUserVirtioFeatures};
use vhost_user_backend::{ShutdownHandle, VhostUserBackend, VhostUserDaemon, VringRwLock, VringT};
use virtio_queue::QueueT;
use vm_memory::{GuestAddressSpace, GuestMemoryAtomic, GuestMemoryMmap};
use vmm_sys_util::epoll::EventSet;
use vmm_sys_util::event::{EventConsumer, EventFlag, EventNotifier, new_event_consumer_and_notifier};

use crate::Disk;
use crate::virtio_blk;

/// The descriptors of each queue.
pub const QUEUE_SIZE: u16 = 256;
/// Queues, each answered by a thread of its own, at most.
const MOST_QUEUES: u16 = 8;

/// The queues that a machine with `cpus` processors gets: one for each, up to a limit. The
/// machine is told the same number.
pub fn queues(cpus: u32) -> u16 {
    cpus.clamp(1, u32::from(MOST_QUEUES)) as u16
}

type Memory = GuestMemoryAtomic<GuestMemoryMmap>;
type Vring = VringRwLock<Memory>;

struct Device {
    disk: Arc<dyn Disk>,
    queues: u16,
    memory: Memory,
    /// Whether the guest asked to be told of used requests only when it says so.
    event_index: AtomicBool,
}

impl Device {
    /// Answers what is waiting in `vring`. Says whether there was anything.
    fn answer_queue(&self, vring: &Vring) -> io::Result<bool> {
        let memory = self.memory.memory();
        let mut answered = false;
        let mut state = vring.get_mut();
        while let Some(chain) = state.get_queue_mut().pop_descriptor_chain(memory.clone()) {
            let head = chain.head_index();
            let written = virtio_blk::answer(&*self.disk, chain);
            state
                .get_queue_mut()
                .add_used(&*memory, head, written)
                .map_err(io::Error::other)?;
            answered = true;
        }
        let tell = !self.event_index.load(Ordering::SeqCst)
            || state.get_queue_mut().needs_notification(&*memory).unwrap_or(true);
        if answered && tell {
            state.signal_used_queue()?;
        }
        Ok(answered)
    }
}

impl VhostUserBackend for Device {
    type Bitmap = ();
    type Vring = Vring;

    fn num_queues(&self) -> usize {
        self.queues as usize
    }

    fn max_queue_size(&self) -> usize {
        QUEUE_SIZE as usize
    }

    fn features(&self) -> u64 {
        virtio_blk::features(&*self.disk, self.queues) | VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits()
    }

    fn protocol_features(&self) -> VhostUserProtocolFeatures {
        VhostUserProtocolFeatures::CONFIG | VhostUserProtocolFeatures::MQ
    }

    fn set_event_idx(&self, enabled: bool) {
        self.event_index.store(enabled, Ordering::SeqCst);
    }

    fn get_config(&self, offset: u32, size: u32) -> Vec<u8> {
        let config = virtio_blk::config(&*self.disk, self.queues, QUEUE_SIZE);
        let start = offset as usize;
        start
            .checked_add(size as usize)
            .and_then(|end| config.get(start..end))
            .map_or_else(Vec::new, <[u8]>::to_vec)
    }

    fn update_memory(&self, _memory: Memory) -> io::Result<()> {
        // The memory that the daemon was given is the one it updates.
        Ok(())
    }

    /// One thread for each queue.
    fn queues_per_thread(&self) -> Vec<u64> {
        (0..self.queues).map(|queue| 1 << queue).collect()
    }

    fn exit_event(&self, _thread: usize) -> Option<(EventConsumer, EventNotifier)> {
        new_event_consumer_and_notifier(EventFlag::NONBLOCK | EventFlag::CLOEXEC).ok()
    }

    fn handle_event(&self, event: u16, events: EventSet, vrings: &[Vring], _thread: usize) -> io::Result<()> {
        if events != EventSet::IN {
            return Err(io::Error::other("an event other than input on a queue"));
        }
        // Each thread has one queue, the first of those it is given.
        let Some(vring) = vrings.first().filter(|_| event == 0) else {
            return Err(io::Error::other("an event for a queue that isn't there"));
        };
        if !self.event_index.load(Ordering::SeqCst) {
            self.answer_queue(vring)?;
            return Ok(());
        }
        // With event indexes, the queue is looked at again after the guest has been asked to
        // say when there is more, so that nothing it added meanwhile waits.
        loop {
            vring
                .get_mut()
                .get_queue_mut()
                .enable_notification(&*self.memory.memory())
                .map_err(io::Error::other)?;
            if !self.answer_queue(vring)? {
                return Ok(());
            }
        }
    }
}

enum Phase {
    /// Nobody has connected yet.
    Waiting,
    Serving(ShutdownHandle),
    Stopped,
}

/// A disk that a machine can connect to, until this is dropped or [`Server::stop`] is called.
/// A machine that restarts its guest connects again: cloud-hypervisor puts its devices together
/// anew.
pub struct Server {
    path: PathBuf,
    phase: Arc<Mutex<Phase>>,
    connections: Arc<AtomicUsize>,
    thread: Option<JoinHandle<()>>,
}

/// Serves the machines that connect, one after the other, until the server is stopped.
fn serve(
    disk: Arc<dyn Disk>,
    mut listener: Listener,
    queues: u16,
    phase: &Mutex<Phase>,
    connections: &AtomicUsize,
) {
    loop {
        let memory = GuestMemoryAtomic::new(GuestMemoryMmap::new());
        let device = Arc::new(Device {
            disk: disk.clone(),
            queues,
            memory: memory.clone(),
            event_index: AtomicBool::new(false),
        });
        let Ok(mut daemon) = VhostUserDaemon::new("vhost-user-blk".into(), device, memory) else {
            return;
        };
        if daemon.start(&mut listener).is_err() {
            return;
        }
        connections.fetch_add(1, Ordering::SeqCst);
        {
            let mut phase = phase.lock().unwrap();
            match (&*phase, daemon.shutdown_handle()) {
                // Stopped while the machine connected.
                (Phase::Stopped, _) | (_, None) => daemon.request_shutdown(),
                (_, Some(handle)) => *phase = Phase::Serving(handle),
            }
        }
        _ = daemon.wait();
        // Dropping it waits for the queues' threads, after which nothing reads or writes the
        // disk for this connection.
        drop(daemon);
        let mut phase = phase.lock().unwrap();
        if matches!(*phase, Phase::Stopped) {
            return;
        }
        *phase = Phase::Waiting;
    }
}

impl Server {
    /// Listens on a new socket at `path`, which only this user can open, for a machine with
    /// `cpus` processors.
    pub fn start(disk: Arc<dyn Disk>, path: &Path, cpus: u32) -> io::Result<Self> {
        let listener = Listener::new(path, true).map_err(io::Error::other)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        let phase = Arc::new(Mutex::new(Phase::Waiting));
        let connections = Arc::new(AtomicUsize::new(0));
        let thread = {
            let (phase, connections) = (phase.clone(), connections.clone());
            thread::Builder::new()
                .name("vhost-user-blk".into())
                .spawn(move || serve(disk, listener, queues(cpus), &phase, &connections))?
        };
        Ok(Self {
            path: path.to_owned(),
            phase,
            connections,
            thread: Some(thread),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// How many times a machine has connected: once, and again for each restart of its guest.
    pub fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }

    /// Hangs up. Returns once no request is being answered any more, so that the disk can be
    /// closed.
    pub fn stop(&mut self) {
        let previous = std::mem::replace(&mut *self.phase.lock().unwrap(), Phase::Stopped);
        match previous {
            // The thread waits for someone to connect. This is that someone, who hangs up at once.
            Phase::Waiting => drop(UnixStream::connect(&self.path)),
            Phase::Serving(handle) => handle.shutdown(),
            Phase::Stopped => {}
        }
        if let Some(thread) = self.thread.take() {
            _ = thread.join();
        }
        _ = std::fs::remove_file(&self.path);
    }

    /// Whether the server has stopped, by itself when it couldn't go on.
    pub fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(JoinHandle::is_finished)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;
    use crate::RawDisk;

    fn disk(folder: &Path) -> Arc<dyn Disk> {
        let path = folder.join("disk.raw");
        std::fs::File::create(&path).unwrap().set_len(1 << 20).unwrap();
        Arc::new(RawDisk::open(&path, false).unwrap())
    }

    fn folder(test: &str) -> PathBuf {
        let folder = std::env::temp_dir().join(format!("vhost-{test}-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        folder
    }

    #[test]
    fn a_server_that_nobody_connected_to_stops() {
        let folder = folder("unconnected");
        let socket = folder.join("disk.sock");
        let mut server = Server::start(disk(&folder), &socket, 2).unwrap();
        assert!(socket.exists());
        assert!(!server.is_finished());
        server.stop();
        assert!(server.is_finished());
        assert!(!socket.exists());
        std::fs::remove_dir_all(&folder).unwrap();
    }

    /// A machine that restarts its guest connects again. One that only hangs up isn't served,
    /// and the server waits for the next.
    #[test]
    fn a_machine_can_connect_again() {
        let folder = folder("again");
        let socket = folder.join("disk.sock");
        let mut server = Server::start(disk(&folder), &socket, 1).unwrap();
        for connection in 1..=3 {
            drop(UnixStream::connect(&socket).unwrap());
            let deadline = Instant::now() + Duration::from_secs(5);
            while server.connections() < connection
                || !matches!(*server.phase.lock().unwrap(), Phase::Waiting)
            {
                assert!(Instant::now() < deadline, "connection {connection} wasn't taken");
                thread::sleep(Duration::from_millis(10));
            }
        }
        assert!(!server.is_finished());
        server.stop();
        assert!(server.is_finished());
        std::fs::remove_dir_all(&folder).unwrap();
    }

    #[test]
    fn queues_follow_processors_up_to_a_limit() {
        assert_eq!(queues(0), 1);
        assert_eq!(queues(3), 3);
        assert_eq!(queues(64), MOST_QUEUES);
    }
}
