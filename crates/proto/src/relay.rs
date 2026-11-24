//! Passes bytes between two connections, both ways, until both directions are done.

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::os::unix::net::UnixStream;

/// One end of a connection that bytes are relayed to and from.
pub trait Connection: Read + Write + Send + Sized + 'static {
    fn duplicate(&self) -> io::Result<Self>;
    fn shut(&self, how: Shutdown);
}

impl Connection for UnixStream {
    fn duplicate(&self) -> io::Result<Self> {
        self.try_clone()
    }

    fn shut(&self, how: Shutdown) {
        _ = self.shutdown(how);
    }
}

impl Connection for TcpStream {
    fn duplicate(&self) -> io::Result<Self> {
        self.try_clone()
    }

    fn shut(&self, how: Shutdown) {
        _ = self.shutdown(how);
    }
}

/// Relays between `a` and `b` until each has said all it has to say, or one of them fails.
/// What one side ends, by shutting its writing down, is ended on the other.
pub fn relay<A: Connection, B: Connection>(a: A, b: B) {
    let (Ok(to_a), Ok(to_b)) = (a.duplicate(), b.duplicate()) else {
        return;
    };
    let forward = std::thread::spawn(move || pass(a, to_b));
    pass(b, to_a);
    _ = forward.join();
}

fn pass(mut from: impl Connection, mut to: impl Connection) {
    let mut buffer = vec![0; 64 * 1024];
    loop {
        match from.read(&mut buffer) {
            Ok(0) => {
                to.shut(Shutdown::Write);
                return;
            }
            Ok(count) => {
                if to.write_all(&buffer[..count]).is_err() {
                    break;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    // One side failed: the other direction has nowhere to go either.
    from.shut(Shutdown::Both);
    to.shut(Shutdown::Both);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_side_hears_the_other_end() {
        let (a, a_far) = UnixStream::pair().unwrap();
        let (b, b_far) = UnixStream::pair().unwrap();
        let relaying = std::thread::spawn(move || relay(a_far, b_far));
        let (mut a, mut b) = (a, b);
        a.write_all(b"ping").unwrap();
        a.shutdown(Shutdown::Write).unwrap();
        let mut heard = String::new();
        b.read_to_string(&mut heard).unwrap();
        assert_eq!(heard, "ping");
        b.write_all(b"pong").unwrap();
        b.shutdown(Shutdown::Write).unwrap();
        heard.clear();
        a.read_to_string(&mut heard).unwrap();
        assert_eq!(heard, "pong");
        relaying.join().unwrap();
    }
}
