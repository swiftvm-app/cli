//! Spike: boots a Linux cloud image on Apple's Virtualization framework from Rust, without a window,
//! and talks to a program in the guest over the virtio socket device.
//!
//! It answers what the engine for macOS needs to know before it is built:
//!
//! - the bindings in `objc2-virtualization` are enough to configure, start and stop a machine;
//! - a machine can live on a dispatch queue of its own, with no run loop on the main thread;
//! - an ad-hoc signature with the virtualization entitlement is enough to run it;
//! - the disk can be a file, or a network block device that this program's own code serves.
//!
//! ```text
//! vz-boot --disk <raw image> | --serve <raw image>  --seed <cidata image>  --state <folder>
//!         [--port <n>] [--cpus <n>] [--memory <GiB>] [--nested yes] [--keep <socket>]
//! ```
//!
//! With `--serve` the image is not given to the framework as a file: this program serves it as a
//! network block device, the way a compressed disk will be.
//!
//! ```text
//! ```

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("This spike runs on macOS only.");
}

#[cfg(target_os = "macos")]
fn main() -> std::process::ExitCode {
    match mac::run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("vz-boot: {message}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use std::fs::File;
    use std::io::{BufRead, BufReader, Write};
    use std::os::fd::BorrowedFd;
    use std::sync::{Arc, mpsc};
    use std::time::{Duration, Instant};

    use block2::RcBlock;
    use dispatch2::{DispatchQueue, DispatchQueueAttr, DispatchRetained};
    use objc2::AllocAnyThread;
    use objc2::rc::Retained;
    use objc2_foundation::{NSArray, NSError, NSFileHandle, NSString, NSURL};
    use objc2_virtualization::*;

    /// A machine, which is only ever touched on its queue.
    struct Machine {
        queue: DispatchRetained<DispatchQueue>,
        machine: Retained<VZVirtualMachine>,
    }

    // The framework asks for every call to be made on the machine's queue, not on one thread.
    unsafe impl Send for Machine {}
    unsafe impl Sync for Machine {}

    struct Options {
        disk: Option<String>,
        serve: Option<String>,
        /// Where the served disk is reached, once it is.
        nbd: Option<String>,
        seed: String,
        state: String,
        port: u32,
        /// Lets the guest run virtual machines of its own.
        nested: bool,
        cpus: usize,
        /// In GiB.
        memory: u64,
        /// A socket to take commands for the guest on, instead of stopping once it has answered.
        keep: Option<String>,
    }

    fn options() -> Result<Options, String> {
        let mut options = Options {
            disk: None,
            serve: None,
            nbd: None,
            seed: String::new(),
            state: String::new(),
            port: 5000,
            nested: false,
            cpus: 4,
            memory: 4,
            keep: None,
        };
        let mut arguments = std::env::args().skip(1);
        while let Some(flag) = arguments.next() {
            let value = arguments.next().ok_or(format!("{flag} needs a value"))?;
            match flag.as_str() {
                "--disk" => options.disk = Some(value),
                "--serve" => options.serve = Some(value),
                "--seed" => options.seed = value,
                "--state" => options.state = value,
                "--nested" => options.nested = value == "yes",
                "--cpus" => options.cpus = value.parse().map_err(|_| "--cpus takes a number")?,
                "--memory" => options.memory = value.parse().map_err(|_| "--memory takes a number")?,
                "--keep" => options.keep = Some(value),
                "--port" => options.port = value.parse().map_err(|_| "--port takes a number")?,
                _ => return Err(format!("unknown option {flag}")),
            }
        }
        if options.disk.is_some() == options.serve.is_some() {
            return Err("give either --disk or --serve".into());
        }
        if options.seed.is_empty() || options.state.is_empty() {
            return Err("--seed and --state are required".into());
        }
        Ok(options)
    }

    fn file_url(path: &str) -> Retained<NSURL> {
        NSURL::fileURLWithPath(&NSString::from_str(path))
    }

    fn said(error: Retained<NSError>) -> String {
        error.localizedDescription().to_string()
    }

    fn outcome(error: *mut NSError) -> Result<(), String> {
        // Safety: the framework passes nil or an error that lives for the length of the call.
        match unsafe { error.as_ref() } {
            None => Ok(()),
            Some(error) => Err(error.localizedDescription().to_string()),
        }
    }

    fn configuration(options: &Options) -> Result<Retained<VZVirtualMachineConfiguration>, String> {
        // Safety: these are plain setters on objects nothing else holds yet.
        unsafe {
            let configuration = VZVirtualMachineConfiguration::new();
            let platform = VZGenericPlatformConfiguration::new();
            if options.nested {
                if !VZGenericPlatformConfiguration::isNestedVirtualizationSupported() {
                    return Err("this Mac can't run virtual machines inside a guest".into());
                }
                platform.setNestedVirtualizationEnabled(true);
            }
            configuration.setPlatform(&platform);
            configuration.setCPUCount(options.cpus);
            configuration.setMemorySize(options.memory << 30);

            let variables = VZEFIVariableStore::initCreatingVariableStoreAtURL_options_error(
                VZEFIVariableStore::alloc(),
                &file_url(&format!("{}/efi-vars.fd", options.state)),
                VZEFIVariableStoreInitializationOptions::AllowOverwrite,
            )
            .map_err(said)?;
            let loader = VZEFIBootLoader::new();
            loader.setVariableStore(Some(&variables));
            configuration.setBootLoader(Some(&loader));

            let system: Retained<VZStorageDeviceAttachment> = match (&options.disk, &options.nbd) {
                (Some(path), _) => Retained::into_super(
                    VZDiskImageStorageDeviceAttachment::initWithURL_readOnly_cachingMode_synchronizationMode_error(
                        VZDiskImageStorageDeviceAttachment::alloc(),
                        &file_url(path),
                        false,
                        VZDiskImageCachingMode::Automatic,
                        VZDiskImageSynchronizationMode::Full,
                    )
                    .map_err(said)?,
                ),
                (_, Some(address)) => {
                    let url = NSURL::URLWithString(&NSString::from_str(address)).ok_or("the NBD address isn't a URL")?;
                    Retained::into_super(
                        VZNetworkBlockDeviceStorageDeviceAttachment::initWithURL_timeout_forcedReadOnly_synchronizationMode_error(
                            VZNetworkBlockDeviceStorageDeviceAttachment::alloc(),
                            &url,
                            60.0,
                            false,
                            VZDiskSynchronizationMode::Full,
                        )
                        .map_err(said)?,
                    )
                }
                _ => unreachable!(),
            };
            let seed = VZDiskImageStorageDeviceAttachment::initWithURL_readOnly_error(
                VZDiskImageStorageDeviceAttachment::alloc(),
                &file_url(&options.seed),
                true,
            )
            .map_err(said)?;
            let disks = [
                Retained::into_super(VZVirtioBlockDeviceConfiguration::initWithAttachment(
                    VZVirtioBlockDeviceConfiguration::alloc(),
                    &system,
                )),
                Retained::into_super(VZVirtioBlockDeviceConfiguration::initWithAttachment(
                    VZVirtioBlockDeviceConfiguration::alloc(),
                    &seed,
                )),
            ];
            configuration.setStorageDevices(&NSArray::from_retained_slice(&disks));

            // What the guest writes to its console goes to a file. Nothing is typed into it.
            let log =
                File::create(format!("{}/console.log", options.state)).map_err(|error| error.to_string())?;
            let log = NSFileHandle::initWithFileDescriptor_closeOnDealloc(
                NSFileHandle::alloc(),
                std::os::fd::IntoRawFd::into_raw_fd(log),
                true,
            );
            let console = VZVirtioConsoleDeviceSerialPortConfiguration::new();
            console.setAttachment(Some(
                &VZFileHandleSerialPortAttachment::initWithFileHandleForReading_fileHandleForWriting(
                    VZFileHandleSerialPortAttachment::alloc(),
                    None,
                    Some(&log),
                ),
            ));
            configuration.setSerialPorts(&NSArray::from_retained_slice(&[Retained::into_super(console)]));

            let network = VZVirtioNetworkDeviceConfiguration::new();
            network.setAttachment(Some(&VZNATNetworkDeviceAttachment::new()));
            configuration.setNetworkDevices(&NSArray::from_retained_slice(&[Retained::into_super(network)]));

            configuration.setEntropyDevices(&NSArray::from_retained_slice(&[Retained::into_super(
                VZVirtioEntropyDeviceConfiguration::new(),
            )]));
            configuration.setSocketDevices(&NSArray::from_retained_slice(&[Retained::into_super(
                VZVirtioSocketDeviceConfiguration::new(),
            )]));

            configuration.validateWithError().map_err(said)?;
            Ok(configuration)
        }
    }

    impl Machine {
        /// Runs `work` on the machine's queue and waits for what it reports, for `limit` at most.
        fn ask<T: Send + 'static>(
            self: &Arc<Self>,
            limit: Duration,
            work: impl FnOnce(&VZVirtualMachine, mpsc::Sender<T>) + Send + 'static,
        ) -> Result<T, String> {
            let (sender, receiver) = mpsc::channel();
            let this = self.clone();
            self.queue.exec_async(move || work(&this.machine, sender));
            receiver
                .recv_timeout(limit)
                .map_err(|_| "the machine didn't answer in time".to_string())
        }

        fn start(self: &Arc<Self>) -> Result<(), String> {
            self.ask(Duration::from_secs(60), |machine, done| {
                let handler = RcBlock::new(move |error: *mut NSError| _ = done.send(outcome(error)));
                unsafe { machine.startWithCompletionHandler(&handler) };
            })?
        }

        fn stop(self: &Arc<Self>) -> Result<(), String> {
            self.ask(Duration::from_secs(60), |machine, done| {
                let handler = RcBlock::new(move |error: *mut NSError| _ = done.send(outcome(error)));
                unsafe { machine.stopWithCompletionHandler(&handler) };
            })?
        }

        /// Connects to a port that a program in the guest listens on.
        fn connect(self: &Arc<Self>, port: u32) -> Result<File, String> {
            self.ask(Duration::from_secs(10), move |machine, done| unsafe {
                let Some(device) = machine.socketDevices().firstObject() else {
                    _ = done.send(Err("the machine has no socket device".to_string()));
                    return;
                };
                let device: Retained<VZVirtioSocketDevice> = Retained::cast_unchecked(device);
                let handler = RcBlock::new(
                    move |connection: *mut VZVirtioSocketConnection, error: *mut NSError| {
                        let result = match connection.as_ref() {
                            // The connection closes its descriptor when it goes away, so this keeps one of its own.
                            Some(connection) => BorrowedFd::borrow_raw(connection.fileDescriptor())
                                .try_clone_to_owned()
                                .map(File::from)
                                .map_err(|error| error.to_string()),
                            None => outcome(error).and(Err("no connection".to_string())),
                        };
                        _ = done.send(result);
                    },
                );
                device.connectToPort_completionHandler(port, &handler);
            })?
        }
    }

    pub fn run() -> Result<(), String> {
        let mut options = options()?;
        std::fs::create_dir_all(&options.state).map_err(|error| error.to_string())?;
        let mut server = None;
        if let Some(image) = &options.serve {
            let disk = disk::RawDisk::open(image.as_ref(), false).map_err(|error| error.to_string())?;
            let socket = std::env::temp_dir().join(format!("vz-boot-{}.sock", std::process::id()));
            let started =
                disk::nbd::Server::start(Arc::new(disk), &socket).map_err(|error| error.to_string())?;
            options.nbd = Some(started.url());
            server = Some(started);
        }
        let configuration = configuration(&options)?;
        let queue = DispatchQueue::new("vz-boot.machine", DispatchQueueAttr::SERIAL);
        let machine = unsafe {
            VZVirtualMachine::initWithConfiguration_queue(VZVirtualMachine::alloc(), &configuration, &queue)
        };
        let machine = Arc::new(Machine { queue, machine });

        let began = Instant::now();
        machine.start()?;
        println!("started after {:.1} s", began.elapsed().as_secs_f32());

        // The program in the guest runs the line it is sent with `sh -c` and sends back what that prints.
        let mut last = String::new();
        let greeting = loop {
            if began.elapsed() > Duration::from_secs(300) {
                _ = machine.stop();
                return Err(format!(
                    "nothing listened on port {} within 300 s: {last}",
                    options.port
                ));
            }
            std::thread::sleep(Duration::from_millis(500));
            match machine.connect(options.port) {
                Ok(mut connection) => {
                    connection
                        .write_all(b"echo hello from Linux $(uname -rm)\n")
                        .map_err(|error| error.to_string())?;
                    let mut line = String::new();
                    BufReader::new(connection)
                        .read_line(&mut line)
                        .map_err(|error| error.to_string())?;
                    break line.trim().to_string();
                }
                Err(error) => last = error,
            }
        };
        println!(
            "guest answered after {:.1} s: {greeting}",
            began.elapsed().as_secs_f32()
        );

        // Kept running, the machine takes commands on a socket: one line in, what it prints out.
        // The line `__stop` ends it.
        if let Some(path) = &options.keep {
            _ = std::fs::remove_file(path);
            let listener = std::os::unix::net::UnixListener::bind(path).map_err(|error| error.to_string())?;
            println!("listening on {path}");
            for client in listener.incoming() {
                let Ok(mut client) = client else { continue };
                let mut line = String::new();
                if BufReader::new(&client).read_line(&mut line).is_err() || line.trim() == "__stop" {
                    break;
                }
                let machine = machine.clone();
                let port = options.port;
                std::thread::spawn(move || match machine.connect(port) {
                    Ok(mut guest) => {
                        if guest.write_all(line.as_bytes()).is_ok() {
                            _ = std::io::copy(&mut guest, &mut client);
                        }
                    }
                    Err(error) => _ = writeln!(client, "vz-boot: {error}"),
                });
            }
        }

        machine.stop()?;
        drop(server);
        println!("stopped");
        Ok(())
    }
}
