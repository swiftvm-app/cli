//! Machines on Apple's Virtualization framework.
//!
//! A machine lives on a dispatch queue of its own: the framework wants every call to it made
//! there, and reports back there. Nothing needs the main thread or a run loop.

use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};
use std::path::Path;
use std::sync::{Arc, mpsc};
use std::time::Duration;

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchQueueAttr, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AllocAnyThread, DefinedClass, define_class, msg_send};
use objc2_foundation::{
    NSArray, NSData, NSDictionary, NSError, NSFileHandle, NSObject, NSObjectProtocol, NSString, NSURL,
};
use objc2_virtualization::*;

use super::{DiskAttachment, Engine, SHARES_TAG, Spec, Stopped};
use crate::store::Share;
use crate::{Context, Error, ErrorCode, Result};

/// How long the framework gets to start a machine, or to cut its power.
const LIMIT: Duration = Duration::from_secs(60);
/// How long it gets to connect to a port in the guest. It says nothing when nothing listens
/// there, so this is how long that takes to tell.
const CONNECT_LIMIT: Duration = Duration::from_secs(5);
/// How long the framework waits for the server of the machine's disk to answer a request.
const DISK_TIMEOUT: Duration = Duration::from_secs(60);

pub(crate) struct Machine(Arc<Inner>);

struct Inner {
    queue: DispatchRetained<DispatchQueue>,
    machine: Retained<VZVirtualMachine>,
    /// The machine only points to it, so it is kept here.
    _delegate: Retained<Delegate>,
}

// Safety: the machine is only ever used on its queue, which is what the framework asks for
// rather than one thread. The delegate holds a function that can be called from any thread.
unsafe impl Send for Inner {}
unsafe impl Sync for Inner {}

define_class!(
    // Safety: NSObject asks nothing of its subclasses, and this one has no `Drop`.
    #[unsafe(super(NSObject))]
    #[ivars = Stopped]
    struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl VZVirtualMachineDelegate for Delegate {
        #[unsafe(method(guestDidStopVirtualMachine:))]
        fn guest_did_stop(&self, _machine: &VZVirtualMachine) {
            (self.ivars())(None);
        }

        #[unsafe(method(virtualMachine:didStopWithError:))]
        fn did_stop_with_error(&self, _machine: &VZVirtualMachine, error: &NSError) {
            (self.ivars())(Some(error.localizedDescription().to_string()));
        }
    }
);

fn failed(what: impl std::fmt::Display) -> Error {
    Error::new(ErrorCode::Engine, what.to_string())
}

fn said(error: Retained<NSError>) -> Error {
    failed(error.localizedDescription())
}

fn outcome(error: *mut NSError) -> Result<()> {
    // Safety: the framework passes nil or an error that lives for the length of the call.
    match unsafe { error.as_ref() } {
        None => Ok(()),
        Some(error) => Err(failed(error.localizedDescription())),
    }
}

/// Whether this Mac lets machines run machines of their own.
pub(crate) fn nested_supported() -> bool {
    // Safety: a class method without arguments. Before macOS 15 the class doesn't answer it.
    unsafe {
        let class = <VZGenericPlatformConfiguration as objc2::ClassType>::class();
        let selector = objc2::sel!(isNestedVirtualizationSupported);
        let answers: bool = msg_send![class, respondsToSelector: selector];
        answers && VZGenericPlatformConfiguration::isNestedVirtualizationSupported()
    }
}

fn file_url(path: &Path) -> Retained<NSURL> {
    NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()))
}

/// What tells this machine from every other to its guest. Made once, and kept with the machine.
fn identifier(directory: &Path) -> Result<Retained<VZGenericMachineIdentifier>> {
    let path = directory.join("machine-identifier.bin");
    // Safety: plain calls with objects that live through them.
    unsafe {
        let kept = std::fs::read(&path).ok().and_then(|bytes| {
            VZGenericMachineIdentifier::initWithDataRepresentation(
                VZGenericMachineIdentifier::alloc(),
                &NSData::with_bytes(&bytes),
            )
        });
        if let Some(kept) = kept {
            return Ok(kept);
        }
        let new = VZGenericMachineIdentifier::new();
        std::fs::write(&path, new.dataRepresentation().to_vec()).at("write", &path)?;
        Ok(new)
    }
}

/// Where the guest's firmware keeps its settings, such as what to boot.
fn firmware_variables(directory: &Path) -> Result<Retained<VZEFIVariableStore>> {
    let path = directory.join("efi-vars.fd");
    // Safety: plain calls with objects that live through them.
    unsafe {
        if path.exists() {
            return Ok(VZEFIVariableStore::initWithURL(
                VZEFIVariableStore::alloc(),
                &file_url(&path),
            ));
        }
        VZEFIVariableStore::initCreatingVariableStoreAtURL_options_error(
            VZEFIVariableStore::alloc(),
            &file_url(&path),
            VZEFIVariableStoreInitializationOptions(0),
        )
        .map_err(said)
    }
}

/// The folders of `shares`, as the one device that holds them all serves them: each a folder of
/// its own, under its name.
fn directory_share(shares: &[Share]) -> Retained<VZDirectoryShare> {
    // Safety: plain calls with objects that live through them.
    unsafe {
        let names: Vec<Retained<NSString>> = shares
            .iter()
            .map(|share| NSString::from_str(&share.name))
            .collect();
        let folders: Vec<Retained<VZSharedDirectory>> = shares
            .iter()
            .map(|share| {
                VZSharedDirectory::initWithURL_readOnly(
                    VZSharedDirectory::alloc(),
                    &file_url(&share.path),
                    share.read_only,
                )
            })
            .collect();
        let names: Vec<&NSString> = names.iter().map(|name| &**name).collect();
        let directories = NSDictionary::from_retained_objects(&names, &folders);
        Retained::into_super(VZMultipleDirectoryShare::initWithDirectories(
            VZMultipleDirectoryShare::alloc(),
            &directories,
        ))
    }
}

fn configuration(spec: Spec) -> Result<Retained<VZVirtualMachineConfiguration>> {
    // Safety: setters on objects that nothing else holds yet, with arguments that live through
    // the calls.
    unsafe {
        let configuration = VZVirtualMachineConfiguration::new();
        configuration.setCPUCount((spec.cpus as usize).clamp(
            VZVirtualMachineConfiguration::minimumAllowedCPUCount(),
            VZVirtualMachineConfiguration::maximumAllowedCPUCount(),
        ));
        // Whole mebibytes, which is what the framework takes.
        let memory = spec.memory.clamp(
            VZVirtualMachineConfiguration::minimumAllowedMemorySize(),
            VZVirtualMachineConfiguration::maximumAllowedMemorySize(),
        );
        configuration.setMemorySize(memory >> 20 << 20);

        let platform = VZGenericPlatformConfiguration::new();
        let identifier = identifier(&spec.directory)?;
        platform.setMachineIdentifier(&identifier);
        if spec.nested {
            if !nested_supported() {
                return Err(super::no_nesting());
            }
            platform.setNestedVirtualizationEnabled(true);
        }
        configuration.setPlatform(&platform);
        let loader = VZEFIBootLoader::new();
        let variables = firmware_variables(&spec.directory)?;
        loader.setVariableStore(Some(&variables));
        configuration.setBootLoader(Some(&loader));

        let disk: Retained<VZStorageDeviceAttachment> = match &spec.disk {
            DiskAttachment::File(path) => Retained::into_super(
                VZDiskImageStorageDeviceAttachment::initWithURL_readOnly_cachingMode_synchronizationMode_error(
                    VZDiskImageStorageDeviceAttachment::alloc(),
                    &file_url(path),
                    false,
                    VZDiskImageCachingMode::Automatic,
                    VZDiskImageSynchronizationMode::Full,
                )
                .map_err(said)?,
            ),
            DiskAttachment::Network(address) => {
                let url = NSURL::URLWithString(&NSString::from_str(address))
                    .ok_or_else(|| failed("The address of the machine's disk isn't a URL."))?;
                Retained::into_super(
                    VZNetworkBlockDeviceStorageDeviceAttachment::initWithURL_timeout_forcedReadOnly_synchronizationMode_error(
                        VZNetworkBlockDeviceStorageDeviceAttachment::alloc(),
                        &url,
                        DISK_TIMEOUT.as_secs_f64(),
                        false,
                        VZDiskSynchronizationMode::Full,
                    )
                    .map_err(said)?,
                )
            }
            DiskAttachment::VhostUser(_) => {
                return Err(failed("The framework can't take a disk from a vhost-user device."));
            }
        };
        let mut disks = vec![Retained::into_super(
            VZVirtioBlockDeviceConfiguration::initWithAttachment(
                VZVirtioBlockDeviceConfiguration::alloc(),
                &disk,
            ),
        )];
        if let Some(seed) = &spec.seed {
            let seed = VZDiskImageStorageDeviceAttachment::initWithURL_readOnly_error(
                VZDiskImageStorageDeviceAttachment::alloc(),
                &file_url(seed),
                true,
            )
            .map_err(said)?;
            disks.push(Retained::into_super(
                VZVirtioBlockDeviceConfiguration::initWithAttachment(
                    VZVirtioBlockDeviceConfiguration::alloc(),
                    &seed,
                ),
            ));
        }
        configuration.setStorageDevices(&NSArray::from_retained_slice(&disks));

        // The guest's serial port, `hvc0` in Linux. The handles close their descriptors when the
        // machine lets go of them.
        let handle = |descriptor: std::os::fd::OwnedFd| {
            NSFileHandle::initWithFileDescriptor_closeOnDealloc(
                NSFileHandle::alloc(),
                descriptor.into_raw_fd(),
                true,
            )
        };
        let console = VZVirtioConsoleDeviceSerialPortConfiguration::new();
        console.setAttachment(Some(
            &VZFileHandleSerialPortAttachment::initWithFileHandleForReading_fileHandleForWriting(
                VZFileHandleSerialPortAttachment::alloc(),
                Some(&handle(spec.console.input)),
                Some(&handle(spec.console.output)),
            ),
        ));
        configuration.setSerialPorts(&NSArray::from_retained_slice(&[Retained::into_super(console)]));

        let address =
            VZMACAddress::initWithString(VZMACAddress::alloc(), &NSString::from_str(&spec.mac_address))
                .ok_or_else(|| {
                    failed(format_args!(
                        "\"{}\" isn't the address of a network card.",
                        spec.mac_address
                    ))
                })?;
        let network = VZVirtioNetworkDeviceConfiguration::new();
        network.setMACAddress(&address);
        network.setAttachment(Some(&VZNATNetworkDeviceAttachment::new()));
        configuration.setNetworkDevices(&NSArray::from_retained_slice(&[Retained::into_super(network)]));

        configuration.setEntropyDevices(&NSArray::from_retained_slice(&[Retained::into_super(
            VZVirtioEntropyDeviceConfiguration::new(),
        )]));
        // What the host reaches programs in the guest through, with no network between them.
        configuration.setSocketDevices(&NSArray::from_retained_slice(&[Retained::into_super(
            VZVirtioSocketDeviceConfiguration::new(),
        )]));

        // Always there, also without shares, so that folders can be shared while the machine
        // runs: devices can't be added then.
        let sharing = VZVirtioFileSystemDeviceConfiguration::initWithTag(
            VZVirtioFileSystemDeviceConfiguration::alloc(),
            &NSString::from_str(SHARES_TAG),
        );
        sharing.setShare(Some(&directory_share(&spec.shares)));
        configuration
            .setDirectorySharingDevices(&NSArray::from_retained_slice(&[Retained::into_super(sharing)]));

        configuration.validateWithError().map_err(said)?;
        Ok(configuration)
    }
}

impl Machine {
    pub fn new(spec: Spec, stopped: Stopped) -> Result<Self> {
        let configuration = configuration(spec)?;
        let queue = DispatchQueue::new("machine", DispatchQueueAttr::SERIAL);
        let delegate: Retained<Delegate> = {
            let delegate = Delegate::alloc().set_ivars(stopped);
            // Safety: NSObject's `init` is the one that the class inherits.
            unsafe { msg_send![super(delegate), init] }
        };
        // Safety: a machine is made off its queue and used on it.
        let machine = unsafe {
            VZVirtualMachine::initWithConfiguration_queue(VZVirtualMachine::alloc(), &configuration, &queue)
        };
        let inner = Arc::new(Inner {
            queue,
            machine,
            _delegate: delegate,
        });
        let on_queue = inner.clone();
        inner.queue.exec_sync(move || {
            // Safety: on the machine's queue, with a delegate that lives as long as the machine.
            unsafe {
                on_queue
                    .machine
                    .setDelegate(Some(ProtocolObject::from_ref(&*on_queue._delegate)))
            };
        });
        Ok(Self(inner))
    }

    /// Runs `work` on the machine's queue and waits for what it reports, for `limit` at most.
    fn ask<T: Send + 'static>(
        &self,
        limit: Duration,
        work: impl FnOnce(&VZVirtualMachine, mpsc::Sender<T>) + Send + 'static,
    ) -> Result<T> {
        let (sender, receiver) = mpsc::channel();
        let inner = self.0.clone();
        self.0.queue.exec_async(move || work(&inner.machine, sender));
        receiver
            .recv_timeout(limit)
            .map_err(|_| failed("The virtualization framework didn't answer in time."))
    }
}

impl Engine for Machine {
    fn start(&self) -> Result<()> {
        self.ask(LIMIT, |machine, done| {
            let handler = RcBlock::new(move |error: *mut NSError| _ = done.send(outcome(error)));
            // Safety: on the machine's queue.
            unsafe { machine.startWithCompletionHandler(&handler) };
        })?
    }

    fn request_stop(&self) -> Result<()> {
        self.ask(LIMIT, |machine, done| {
            // Safety: on the machine's queue.
            _ = done.send(unsafe { machine.requestStopWithError() }.map_err(said));
        })?
    }

    fn stop(&self) -> Result<()> {
        self.ask(LIMIT, |machine, done| {
            // Safety: on the machine's queue.
            unsafe {
                // A machine that is off already can't be stopped, and needn't be.
                if !machine.canStop() {
                    _ = done.send(Ok(()));
                    return;
                }
                let handler = RcBlock::new(move |error: *mut NSError| _ = done.send(outcome(error)));
                machine.stopWithCompletionHandler(&handler);
            }
        })?
    }

    fn share(&self, shares: &[Share]) -> Result<()> {
        let shares = shares.to_vec();
        self.ask(LIMIT, move |machine, done| {
            // Safety: on the machine's queue.
            unsafe {
                let device = machine
                    .directorySharingDevices()
                    .firstObject()
                    .and_then(|device| device.downcast::<VZVirtioFileSystemDevice>().ok());
                let Some(device) = device else {
                    _ = done.send(Err(failed(
                        "The machine has no device to share folders with. Stop it and start it again.",
                    )));
                    return;
                };
                device.setShare(Some(&directory_share(&shares)));
            }
            _ = done.send(Ok(()));
        })?
    }

    fn connect(&self, port: u32) -> Result<OwnedFd> {
        let unanswered = || {
            Error::new(
                ErrorCode::Guest,
                format!("Nothing in the guest answers on port {port} of its socket device."),
            )
        };
        let connected = self.ask(CONNECT_LIMIT, move |machine, done| {
            // Safety: on the machine's queue, with a handler that the framework calls there.
            unsafe {
                let devices = machine.socketDevices();
                let Some(device) = devices
                    .firstObject()
                    .and_then(|device| device.downcast::<VZVirtioSocketDevice>().ok())
                else {
                    _ = done.send(Err(failed("The machine has no socket device.")));
                    return;
                };
                let handler = RcBlock::new(
                    move |connection: *mut VZVirtioSocketConnection, error: *mut NSError| {
                        let result = match connection.as_ref() {
                            // The connection closes its descriptor when it is let go of, so the
                            // caller gets one of its own.
                            Some(connection) => match libc::dup(connection.fileDescriptor()) {
                                -1 => Err(failed(std::io::Error::last_os_error())),
                                descriptor => Ok(OwnedFd::from_raw_fd(descriptor)),
                            },
                            None => Err(match error.as_ref() {
                                Some(error) => {
                                    Error::new(ErrorCode::Guest, error.localizedDescription().to_string())
                                }
                                None => failed("The framework didn't connect, and didn't say why."),
                            }),
                        };
                        _ = done.send(result);
                    },
                );
                device.connectToPort_completionHandler(port, &handler);
            }
        });
        match connected {
            Ok(result) => result,
            // A guest that doesn't listen gets no answer at all.
            Err(_) => Err(unanswered()),
        }
    }
}
