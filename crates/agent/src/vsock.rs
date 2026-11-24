//! Listening on the machine's socket device (virtio-vsock).

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;

pub struct Listener(OwnedFd);

fn checked(result: libc::c_int) -> io::Result<libc::c_int> {
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result)
    }
}

impl Listener {
    /// Listens on `port`, for connections from anywhere.
    pub fn bind(port: u32) -> io::Result<Self> {
        // Safety: plain system calls, with an address that lives through them.
        unsafe {
            let socket = OwnedFd::from_raw_fd(checked(libc::socket(
                libc::AF_VSOCK,
                libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
                0,
            ))?);
            let mut address: libc::sockaddr_vm = std::mem::zeroed();
            address.svm_family = libc::AF_VSOCK as libc::sa_family_t;
            address.svm_cid = libc::VMADDR_CID_ANY;
            address.svm_port = port;
            checked(libc::bind(
                socket.as_raw_fd(),
                (&raw const address).cast(),
                size_of::<libc::sockaddr_vm>() as libc::socklen_t,
            ))?;
            checked(libc::listen(socket.as_raw_fd(), 64))?;
            Ok(Self(socket))
        }
    }

    /// The next connection, and the machine it comes from: `VMADDR_CID_HOST` for the host.
    pub fn accept(&self) -> io::Result<(UnixStream, u32)> {
        // Safety: an address and its length that live through the call.
        unsafe {
            let mut address: libc::sockaddr_vm = std::mem::zeroed();
            let mut length = size_of::<libc::sockaddr_vm>() as libc::socklen_t;
            let connection = loop {
                match checked(libc::accept4(
                    self.0.as_raw_fd(),
                    (&raw mut address).cast(),
                    &mut length,
                    libc::SOCK_CLOEXEC,
                )) {
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    other => break other?,
                }
            };
            // A stream socket like any other, for reading, writing and shutting down.
            Ok((
                UnixStream::from(OwnedFd::from_raw_fd(connection)),
                address.svm_cid,
            ))
        }
    }
}
