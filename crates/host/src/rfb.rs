//! A VNC client (the remote framebuffer protocol, RFC 6143) for the desktop in a guest: what its
//! screen shows, and its keyboard and pointer.
//!
//! It asks for the whole screen when a picture is wanted and reads until it has it, rather than
//! following every change: the guest's screen is looked at now and then, not watched.

use std::io::{self, Read, Write};

use proto::{Input, ScreenSize};

/// The largest screen side that is believed. Anything larger is a server that says nonsense.
const LARGEST_SIDE: u32 = 16384;
/// The longest text of the server's clipboard that is read past.
const LONGEST_CUT_TEXT: usize = 16 << 20;

/// The kinds of rectangles this client reads.
const RAW: i32 = 0;
const COPY_RECT: i32 = 1;
const DESKTOP_SIZE: i32 = -223;
const LAST_RECT: i32 = -224;

fn malformed(what: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("the guest's VNC server {what}"),
    )
}

pub(crate) struct Rfb<S> {
    stream: S,
    width: u32,
    height: u32,
    /// Four bytes a pixel, red first, from the top left.
    pixels: Vec<u8>,
    /// Some of the last input went out before it failed.
    partly_sent: bool,
}

fn read_u8(stream: &mut impl Read) -> io::Result<u8> {
    let mut byte = [0];
    stream.read_exact(&mut byte)?;
    Ok(byte[0])
}

fn read_u16(stream: &mut impl Read) -> io::Result<u16> {
    let mut bytes = [0; 2];
    stream.read_exact(&mut bytes)?;
    Ok(u16::from_be_bytes(bytes))
}

fn read_u32(stream: &mut impl Read) -> io::Result<u32> {
    let mut bytes = [0; 4];
    stream.read_exact(&mut bytes)?;
    Ok(u32::from_be_bytes(bytes))
}

fn skip(stream: &mut impl Read, count: u64) -> io::Result<()> {
    let skipped = io::copy(&mut stream.take(count), &mut io::sink())?;
    if skipped < count {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    Ok(())
}

/// The server's reason for turning the client away.
fn reason(stream: &mut impl Read) -> io::Error {
    let text = read_u32(stream).ok().and_then(|length| {
        let mut text = vec![0; length.min(4096) as usize];
        stream.read_exact(&mut text).ok()?;
        Some(String::from_utf8_lossy(&text).into_owned())
    });
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!(
            "the guest's VNC server turned this client away: {}",
            text.unwrap_or_default()
        ),
    )
}

impl<S: Read + Write> Rfb<S> {
    /// Says hello as the protocol has it, and agrees on how pixels and rectangles are sent.
    pub fn connect(mut stream: S) -> io::Result<Self> {
        let mut version = [0; 12];
        stream.read_exact(&mut version)?;
        if !version.starts_with(b"RFB 003.") {
            return Err(malformed("doesn't speak VNC"));
        }
        // 3.8 is the newest there is. A server of 3.3 would choose the security itself.
        if version[8..11] < b"008"[..] {
            return Err(malformed("speaks a version of VNC older than 3.8"));
        }
        stream.write_all(b"RFB 003.008\n")?;
        let count = read_u8(&mut stream)?;
        if count == 0 {
            return Err(reason(&mut stream));
        }
        let mut types = vec![0; count as usize];
        stream.read_exact(&mut types)?;
        // None: only the machine's process and the guest itself can reach the server.
        if !types.contains(&1) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "the guest's VNC server wants a password",
            ));
        }
        stream.write_all(&[1])?;
        if read_u32(&mut stream)? != 0 {
            return Err(reason(&mut stream));
        }
        // Shared: a viewer and this client see the same screen at the same time.
        stream.write_all(&[1])?;
        let width = read_u16(&mut stream)? as u32;
        let height = read_u16(&mut stream)? as u32;
        skip(&mut stream, 16)?;
        let name_length = read_u32(&mut stream)?;
        if name_length > 4096 {
            return Err(malformed("sent a name that is too long"));
        }
        skip(&mut stream, name_length as u64)?;

        // Four bytes a pixel, little-endian, red in the lowest: red, green, blue, nothing.
        let mut format = vec![0, 0, 0, 0, 32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 0, 8, 16];
        format.extend_from_slice(&[0, 0, 0]);
        let encodings = [COPY_RECT, RAW, DESKTOP_SIZE, LAST_RECT];
        let mut set_encodings = vec![2, 0];
        set_encodings.extend_from_slice(&(encodings.len() as u16).to_be_bytes());
        for encoding in encodings {
            set_encodings.extend_from_slice(&encoding.to_be_bytes());
        }
        format.extend_from_slice(&set_encodings);
        stream.write_all(&format)?;
        let mut client = Self {
            stream,
            width: 0,
            height: 0,
            pixels: Vec::new(),
            partly_sent: false,
        };
        client.resize(width, height)?;
        Ok(client)
    }

    fn resize(&mut self, width: u32, height: u32) -> io::Result<()> {
        if width == 0 || height == 0 || width > LARGEST_SIDE || height > LARGEST_SIDE {
            return Err(malformed(&format!("says that its screen is {width}x{height}")));
        }
        self.width = width;
        self.height = height;
        self.pixels = vec![0; width as usize * height as usize * 4];
        Ok(())
    }

    pub fn size(&self) -> ScreenSize {
        ScreenSize {
            width: self.width,
            height: self.height,
        }
    }

    fn request(&mut self, incremental: bool) -> io::Result<()> {
        let mut message = vec![3, incremental as u8, 0, 0, 0, 0];
        message.extend_from_slice(&(self.width as u16).to_be_bytes());
        message.extend_from_slice(&(self.height as u16).to_be_bytes());
        self.stream.write_all(&message)
    }

    /// Brings the picture up to date: the whole screen as it is now.
    pub fn capture(&mut self) -> io::Result<()> {
        self.request(false)?;
        loop {
            match read_u8(&mut self.stream)? {
                0 => {
                    let (pixels, resized) = self.read_update()?;
                    if resized {
                        // The rectangles that came with it were of the old size.
                        self.request(false)?;
                    } else if pixels {
                        return Ok(());
                    } else {
                        // A server may answer with a change of size alone and send the pixels
                        // it owes later, once there is a request for them. Another whole one
                        // would only get another change of size.
                        self.request(true)?;
                    }
                }
                // Colours of a palette, which a client of true colour never asked for.
                1 => {
                    skip(&mut self.stream, 3)?;
                    let count = read_u16(&mut self.stream)?;
                    skip(&mut self.stream, count as u64 * 6)?;
                }
                // The bell.
                2 => {}
                // Text the guest cut.
                3 => {
                    skip(&mut self.stream, 3)?;
                    let length = read_u32(&mut self.stream)? as usize;
                    if length > LONGEST_CUT_TEXT {
                        return Err(malformed("sent too long a text"));
                    }
                    skip(&mut self.stream, length as u64)?;
                }
                other => return Err(malformed(&format!("sent a message of type {other}"))),
            }
        }
    }

    /// Reads one update into the picture. Says whether it had pixels, and whether the screen
    /// changed its size.
    fn read_update(&mut self) -> io::Result<(bool, bool)> {
        skip(&mut self.stream, 1)?;
        let count = read_u16(&mut self.stream)?;
        let (mut pixels, mut resized) = (false, false);
        for _ in 0..count {
            let x = read_u16(&mut self.stream)? as u32;
            let y = read_u16(&mut self.stream)? as u32;
            let width = read_u16(&mut self.stream)? as u32;
            let height = read_u16(&mut self.stream)? as u32;
            match read_u32(&mut self.stream)? as i32 {
                RAW => {
                    self.read_raw(x, y, width, height)?;
                    pixels = true;
                }
                COPY_RECT => {
                    let from_x = read_u16(&mut self.stream)? as u32;
                    let from_y = read_u16(&mut self.stream)? as u32;
                    self.copy(from_x, from_y, x, y, width, height);
                    pixels = true;
                }
                DESKTOP_SIZE => {
                    self.resize(width, height)?;
                    resized = true;
                }
                LAST_RECT => break,
                // Its length is unknown, so nothing after it can be read.
                other => return Err(malformed(&format!("sent a rectangle of kind {other}"))),
            }
        }
        Ok((pixels, resized))
    }

    fn read_raw(&mut self, x: u32, y: u32, width: u32, height: u32) -> io::Result<()> {
        let row = width as usize * 4;
        if x + width > self.width || y + height > self.height {
            // Of a size that has gone.
            return skip(&mut self.stream, row as u64 * height as u64);
        }
        let stride = self.width as usize * 4;
        for line in 0..height as usize {
            let start = (y as usize + line) * stride + x as usize * 4;
            self.stream.read_exact(&mut self.pixels[start..start + row])?;
        }
        Ok(())
    }

    fn copy(&mut self, from_x: u32, from_y: u32, x: u32, y: u32, width: u32, height: u32) {
        if from_x + width > self.width
            || from_y + height > self.height
            || x + width > self.width
            || y + height > self.height
        {
            return;
        }
        let stride = self.width as usize * 4;
        let row = width as usize * 4;
        // From the bottom up when the rectangle moves down, so that no row is overwritten
        // before it has been copied.
        for index in 0..height as usize {
            let line = if y > from_y {
                height as usize - 1 - index
            } else {
                index
            };
            let source = (from_y as usize + line) * stride + from_x as usize * 4;
            let destination = (y as usize + line) * stride + x as usize * 4;
            self.pixels.copy_within(source..source + row, destination);
        }
    }

    /// The picture as red, green and blue bytes.
    pub fn rgb(&self) -> Vec<u8> {
        let mut rgb = Vec::with_capacity(self.pixels.len() / 4 * 3);
        for pixel in self.pixels.as_chunks::<4>().0 {
            rgb.extend_from_slice(&pixel[..3]);
        }
        rgb
    }

    /// Whether some of the input that last failed had gone out, so that sending it again would
    /// do it twice.
    pub fn partly_sent(&self) -> bool {
        self.partly_sent
    }

    /// Sends the events, waiting where they say so. A place off the screen is taken to its edge.
    pub fn input(&mut self, events: &[Input]) -> io::Result<()> {
        self.partly_sent = false;
        let mut message = Vec::new();
        for event in events {
            match *event {
                Input::Pointer { x, y, buttons } => {
                    let x = x.min(self.width.saturating_sub(1)) as u16;
                    let y = y.min(self.height.saturating_sub(1)) as u16;
                    message.extend_from_slice(&[5, buttons]);
                    message.extend_from_slice(&x.to_be_bytes());
                    message.extend_from_slice(&y.to_be_bytes());
                }
                Input::Key { keysym, down } => {
                    message.extend_from_slice(&[4, down as u8, 0, 0]);
                    message.extend_from_slice(&keysym.to_be_bytes());
                }
                Input::Pause { milliseconds } => {
                    self.stream.write_all(&message)?;
                    self.stream.flush()?;
                    self.partly_sent |= !message.is_empty();
                    message.clear();
                    std::thread::sleep(std::time::Duration::from_millis(milliseconds.min(10_000) as u64));
                }
            }
        }
        self.stream.write_all(&message)?;
        self.stream.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    /// The server's side of a handshake, for a screen of `width` by `height`. Returns what the
    /// client said after it.
    fn greet(server: &mut UnixStream, width: u16, height: u16) -> Vec<u8> {
        server.write_all(b"RFB 003.008\n").unwrap();
        let mut version = [0; 12];
        server.read_exact(&mut version).unwrap();
        assert_eq!(&version, b"RFB 003.008\n");
        server.write_all(&[1, 1]).unwrap();
        assert_eq!(read_u8(server).unwrap(), 1);
        server.write_all(&0u32.to_be_bytes()).unwrap();
        assert_eq!(read_u8(server).unwrap(), 1, "shared");
        let mut init = Vec::new();
        init.extend_from_slice(&width.to_be_bytes());
        init.extend_from_slice(&height.to_be_bytes());
        init.extend_from_slice(&[0; 16]);
        init.extend_from_slice(&4u32.to_be_bytes());
        init.extend_from_slice(b"test");
        server.write_all(&init).unwrap();
        // Pixel format and four encodings.
        let mut after = vec![0; 20 + 4 + 16];
        server.read_exact(&mut after).unwrap();
        after
    }

    fn rect(x: u16, y: u16, width: u16, height: u16, encoding: i32) -> Vec<u8> {
        let mut bytes = Vec::new();
        for value in [x, y, width, height] {
            bytes.extend_from_slice(&value.to_be_bytes());
        }
        bytes.extend_from_slice(&encoding.to_be_bytes());
        bytes
    }

    fn update(rects: &[Vec<u8>]) -> Vec<u8> {
        let mut bytes = vec![0, 0];
        bytes.extend_from_slice(&(rects.len() as u16).to_be_bytes());
        for rect in rects {
            bytes.extend_from_slice(rect);
        }
        bytes
    }

    fn request(server: &mut UnixStream) -> (bool, u16, u16) {
        let mut bytes = [0; 10];
        server.read_exact(&mut bytes).unwrap();
        assert_eq!(bytes[0], 3);
        (
            bytes[1] == 1,
            u16::from_be_bytes([bytes[6], bytes[7]]),
            u16::from_be_bytes([bytes[8], bytes[9]]),
        )
    }

    #[test]
    fn a_picture_is_read_across_a_change_of_size_and_what_else_the_server_says() {
        let (client, mut server) = UnixStream::pair().unwrap();
        let fake = std::thread::spawn(move || {
            let after = greet(&mut server, 4, 2);
            assert_eq!(&after[..8], &[0, 0, 0, 0, 32, 24, 0, 1]);
            assert_eq!(request(&mut server), (false, 4, 2));
            // A bell and a cut text first, then a new size alone.
            server
                .write_all(&[2, 3, 0, 0, 0, 0, 0, 0, 2, b'h', b'i'])
                .unwrap();
            server
                .write_all(&update(&[rect(0, 0, 2, 1, DESKTOP_SIZE)]))
                .unwrap();
            assert_eq!(request(&mut server), (false, 2, 1));
            // Only a change of size again: the client asks for what is owed without asking for
            // the whole screen once more.
            server.write_all(&update(&[rect(0, 0, 0, 0, LAST_RECT)])).unwrap();
            assert_eq!(request(&mut server), (true, 2, 1));
            let mut pixels = rect(0, 0, 2, 1, RAW);
            pixels.extend_from_slice(&[1, 2, 3, 0, 4, 5, 6, 0]);
            server.write_all(&update(&[pixels])).unwrap();
            // Then input.
            let mut events = [0; 6 + 8];
            server.read_exact(&mut events).unwrap();
            events
        });
        let mut rfb = Rfb::connect(client).unwrap();
        assert_eq!(rfb.size(), ScreenSize { width: 4, height: 2 });
        rfb.capture().unwrap();
        assert_eq!(rfb.size(), ScreenSize { width: 2, height: 1 });
        assert_eq!(rfb.rgb(), [1, 2, 3, 4, 5, 6]);
        rfb.input(&[
            Input::Pointer {
                x: 99,
                y: 0,
                buttons: 1,
            },
            Input::Pause { milliseconds: 1 },
            Input::Key {
                keysym: 0xff0d,
                down: true,
            },
        ])
        .unwrap();
        let events = fake.join().unwrap();
        // The pointer is kept on the screen.
        assert_eq!(&events[..6], &[5, 1, 0, 1, 0, 0]);
        assert_eq!(&events[6..], &[4, 1, 0, 0, 0, 0, 0xff, 0x0d]);
    }

    #[test]
    fn a_copy_moves_pixels_within_the_picture() {
        let (client, mut server) = UnixStream::pair().unwrap();
        let fake = std::thread::spawn(move || {
            greet(&mut server, 2, 2);
            request(&mut server);
            let mut first = rect(0, 0, 2, 2, RAW);
            first.extend_from_slice(&[1, 1, 1, 0, 2, 2, 2, 0, 3, 3, 3, 0, 4, 4, 4, 0]);
            server.write_all(&update(&[first])).unwrap();
            request(&mut server);
            let mut copy = rect(0, 1, 2, 1, COPY_RECT);
            copy.extend_from_slice(&[0, 0, 0, 0]);
            // Off the screen: read past.
            let mut outside = rect(1, 1, 2, 2, RAW);
            outside.extend_from_slice(&[9; 16]);
            server.write_all(&update(&[copy, outside])).unwrap();
        });
        let mut rfb = Rfb::connect(client).unwrap();
        rfb.capture().unwrap();
        rfb.capture().unwrap();
        fake.join().unwrap();
        assert_eq!(rfb.rgb(), [1, 1, 1, 2, 2, 2, 1, 1, 1, 2, 2, 2]);
    }

    #[test]
    fn input_that_failed_after_some_went_out_says_so() {
        let (client, mut server) = UnixStream::pair().unwrap();
        let fake = std::thread::spawn(move || {
            greet(&mut server, 2, 2);
            // The pointer before the pause, and then the server goes.
            let mut first = [0; 6];
            server.read_exact(&mut first).unwrap();
        });
        let mut rfb = Rfb::connect(client).unwrap();
        let pointer = Input::Pointer {
            x: 1,
            y: 1,
            buttons: 0,
        };
        let failed = rfb.input(&[pointer, Input::Pause { milliseconds: 300 }, pointer]);
        fake.join().unwrap();
        assert!(failed.is_err());
        assert!(rfb.partly_sent());
        // Nothing of this went out.
        assert!(rfb.input(&[pointer]).is_err());
        assert!(!rfb.partly_sent());
    }

    #[test]
    fn a_server_that_wants_a_password_is_refused() {
        let (client, mut server) = UnixStream::pair().unwrap();
        std::thread::spawn(move || {
            server.write_all(b"RFB 003.008\n").unwrap();
            let mut version = [0; 12];
            server.read_exact(&mut version).unwrap();
            server.write_all(&[1, 2]).unwrap();
        });
        let error = Rfb::connect(client).err().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }
}
