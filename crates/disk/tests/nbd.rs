use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::Arc;

use disk::{RawDisk, nbd::Server};

struct Client(UnixStream);

impl Client {
    /// Connects as Apple's client does: the newer handshake, and "go" for the disk.
    fn connect(path: &std::path::Path) -> (Self, u64) {
        let mut stream = UnixStream::connect(path).unwrap();
        let mut greeting = [0; 18];
        stream.read_exact(&mut greeting).unwrap();
        assert_eq!(&greeting[..8], b"NBDMAGIC");
        stream.write_all(&3u32.to_be_bytes()).unwrap();
        let mut option = Vec::new();
        option.extend(0x4948_4156_454F_5054u64.to_be_bytes());
        option.extend(7u32.to_be_bytes());
        option.extend(6u32.to_be_bytes());
        option.extend([0; 6]);
        stream.write_all(&option).unwrap();
        let mut capacity = 0;
        loop {
            let mut head = [0; 20];
            stream.read_exact(&mut head).unwrap();
            let kind = u32::from_be_bytes(head[12..16].try_into().unwrap());
            let mut data = vec![0; u32::from_be_bytes(head[16..].try_into().unwrap()) as usize];
            stream.read_exact(&mut data).unwrap();
            match kind {
                1 => break,
                3 if data[..2] == [0, 0] => capacity = u64::from_be_bytes(data[2..10].try_into().unwrap()),
                kind => panic!("unexpected reply {kind}"),
            }
        }
        (Self(stream), capacity)
    }

    /// Sends one request and returns its error and, for a read that went well, its data.
    fn ask(&mut self, command: u16, offset: u64, length: u32, data: &[u8]) -> (u32, Vec<u8>) {
        let mut request = Vec::new();
        request.extend(0x2560_9513u32.to_be_bytes());
        request.extend(0u16.to_be_bytes());
        request.extend(command.to_be_bytes());
        request.extend(7u64.to_be_bytes());
        request.extend(offset.to_be_bytes());
        request.extend(length.to_be_bytes());
        request.extend(data);
        self.0.write_all(&request).unwrap();
        let mut head = [0; 16];
        self.0.read_exact(&mut head).unwrap();
        assert_eq!(head[..4], 0x6744_6698u32.to_be_bytes());
        assert_eq!(head[8..], 7u64.to_be_bytes());
        let error = u32::from_be_bytes(head[4..8].try_into().unwrap());
        let mut payload = vec![
            0;
            if command == 0 && error == 0 {
                length as usize
            } else {
                0
            }
        ];
        self.0.read_exact(&mut payload).unwrap();
        (error, payload)
    }
}

#[test]
fn serves_reads_and_writes_and_refuses_what_is_out_of_range() {
    let folder = std::env::temp_dir().join(format!("disk-nbd-{}", std::process::id()));
    std::fs::create_dir_all(&folder).unwrap();
    let image = folder.join("disk.raw");
    std::fs::File::create(&image).unwrap().set_len(1 << 20).unwrap();
    let socket = folder.join("disk.sock");
    let mut server = Server::start(Arc::new(RawDisk::open(&image, false).unwrap()), &socket).unwrap();
    assert!(server.url().starts_with("nbd+unix:///disk?socket=/"));

    let (mut client, capacity) = Client::connect(&socket);
    assert_eq!(capacity, 1 << 20);
    assert_eq!(client.ask(1, 4096, 5, b"hello").0, 0);
    assert_eq!(client.ask(3, 0, 0, &[]).0, 0);
    assert_eq!(client.ask(0, 4094, 9, &[]), (0, b"\0\0hello\0\0".to_vec()));
    assert_eq!(client.ask(6, 4096, 2, &[]).0, 0);
    assert_eq!(client.ask(0, 4096, 5, &[]), (0, b"\0\0llo".to_vec()));
    // Past the end, and an offset that wraps around.
    assert_eq!(client.ask(0, (1 << 20) - 4, 8, &[]).0, 22);
    assert_eq!(client.ask(4, u64::MAX, 8, &[]).0, 22);

    server.stop();
    assert!(!socket.exists());
    assert_eq!(&std::fs::read(&image).unwrap()[4096..4101], b"\0\0llo");
    std::fs::remove_file(&image).unwrap();
    std::fs::remove_dir(&folder).unwrap();
}
