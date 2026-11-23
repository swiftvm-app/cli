//! Downloads images into the cache, which every store on this computer shares.
//!
//! ```text
//! <cache>/images/<file>            an image that was checked against its publisher's checksum
//! <cache>/images/<file>.json       what it was checked against
//! <cache>/images/<file>.partial    a download that isn't finished, to carry on with
//! <cache>/images/<file>.lock       held by whoever is downloading it
//! ```

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::images::{Algorithm, Image};
use crate::lock::Lock;
use crate::{Context, Error, ErrorCode, Result};

/// How many times in a row a download is picked up again without getting any further.
const STALLED_RETRIES: u32 = 4;
/// The pause before a download is picked up again, which grows each time nothing got through.
const RETRY_DELAY: Duration = Duration::from_millis(if cfg!(test) { 10 } else { 2000 });

/// Written next to an image once it has been checked. An image without it is checked again.
#[derive(Serialize, Deserialize)]
struct Checked {
    url: String,
    algorithm: Algorithm,
    digest: String,
    size: u64,
}

fn beside(file: &Path, suffix: &str) -> PathBuf {
    let mut name = file.as_os_str().to_owned();
    name.push(suffix);
    name.into()
}

fn place(cache: &Path, image: &Image) -> PathBuf {
    cache.join("images").join(image.file_name())
}

/// The checked copy of `image` in the cache, if there is one.
pub(crate) fn cached(cache: &Path, image: &Image) -> Option<PathBuf> {
    let file = place(cache, image);
    let checked: Checked = serde_json::from_slice(&std::fs::read(beside(&file, ".json")).ok()?).ok()?;
    let size = std::fs::metadata(&file).ok()?.len();
    (checked.url == image.url && checked.size == size).then_some(file)
}

/// Returns the file of `image` in the cache, downloading and checking it if it isn't there yet.
/// `progress` is told how many bytes have arrived, and of how many if the server said.
pub(crate) fn fetch(
    cache: &Path,
    image: &Image,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<PathBuf> {
    let file = place(cache, image);
    let directory = cache.join("images");
    std::fs::create_dir_all(&directory).at("create", &directory)?;
    // Two creations may want the same image. One downloads it, and the other finds it there.
    let lock = beside(&file, ".lock");
    let _downloading = Lock::take(&lock).at("lock", &lock)?;
    if let Some(found) = cached(cache, image) {
        return Ok(found);
    }

    let agent = agent();
    let expected = published_digest(&agent, image)?;
    let keep = |file: &Path| -> Result<()> {
        let checked = Checked {
            url: image.url.clone(),
            algorithm: image.algorithm,
            digest: expected.clone(),
            size: std::fs::metadata(file).at("look at", file)?.len(),
        };
        let note = beside(file, ".json");
        std::fs::write(&note, serde_json::to_vec(&checked).expect("plain data")).at("write", &note)
    };

    // A file that someone put there is as good as a download, if it is the image.
    if file.exists() {
        if digest(&file, image.algorithm)? == expected {
            keep(&file)?;
            return Ok(file);
        }
        std::fs::remove_file(&file).at("remove", &file)?;
    }

    let partial = beside(&file, ".partial");
    let mut resumed = std::fs::metadata(&partial).is_ok_and(|found| found.len() > 0);
    loop {
        download(&agent, &image.url, &partial, progress)?;
        if digest(&partial, image.algorithm)? == expected {
            break;
        }
        std::fs::remove_file(&partial).at("remove", &partial)?;
        // What an earlier attempt left may be the start of an image that its publisher has
        // replaced since. Once more, from the beginning.
        if !resumed {
            return Err(Error::new(
                ErrorCode::Checksum,
                format!(
                    "{} arrived damaged: its checksum isn't the published one. Try again.",
                    image.file_name()
                ),
            ));
        }
        resumed = false;
    }
    std::fs::rename(&partial, &file).at("write", &file)?;
    keep(&file)?;
    Ok(file)
}

fn agent() -> ureq::Agent {
    let tls = ureq::tls::TlsConfig::builder()
        .unversioned_rustls_crypto_provider(std::sync::Arc::new(rustls_graviola::default_provider()))
        .build();
    ureq::Agent::config_builder()
        .tls_config(tls)
        .timeout_connect(Some(Duration::from_secs(30)))
        .timeout_recv_response(Some(Duration::from_secs(60)))
        // A download may take as long as it needs, but not stand still for this long.
        .timeout_per_read(Some(Duration::from_secs(60)))
        .http_status_as_error(false)
        .build()
        .into()
}

fn failed(url: &str, why: impl std::fmt::Display) -> Error {
    Error::new(ErrorCode::Download, format!("Couldn't download {url}: {why}."))
}

/// The checksum that the image's publisher lists for it.
fn published_digest(agent: &ureq::Agent, image: &Image) -> Result<String> {
    let mut response = agent
        .get(&image.checksums)
        .call()
        .map_err(|error| failed(&image.checksums, error))?;
    if response.status().as_u16() != 200 {
        return Err(failed(
            &image.checksums,
            format_args!(
                "the server answered with HTTP status {}",
                response.status().as_u16()
            ),
        ));
    }
    let listing = response
        .body_mut()
        .read_to_string()
        .map_err(|error| failed(&image.checksums, error))?;
    let length = match image.algorithm {
        Algorithm::Sha256 => 64,
        Algorithm::Sha512 => 128,
    };
    listed_digest(image.file_name(), &listing)
        .filter(|digest| digest.len() == length && digest.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .map(|digest| digest.to_ascii_lowercase())
        .ok_or_else(|| {
            Error::new(
                ErrorCode::Checksum,
                format!("{} lists no checksum for {}.", image.checksums, image.file_name()),
            )
        })
}

/// Finds the checksum of `file_name` in a listing as `sha256sum` writes it (`<hex>  <name>` or
/// `<hex> *<name>`), or as BSD's tools do (`SHA256 (<name>) = <hex>`).
fn listed_digest<'a>(file_name: &str, listing: &'a str) -> Option<&'a str> {
    for line in listing.lines() {
        if let (Some(open), Some(close)) = (line.find('('), line.rfind(") = "))
            && open < close
        {
            if &line[open + 1..close] == file_name {
                return Some(line[close + 4..].trim());
            }
            continue;
        }
        let Some((digest, name)) = line.trim().split_once(' ') else {
            continue;
        };
        let name = name.trim();
        if name.strip_prefix('*').unwrap_or(name) == file_name {
            return Some(digest);
        }
    }
    None
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn digest(file: &Path, algorithm: Algorithm) -> Result<String> {
    use graviola::hashing::sha2::{Sha256Context, Sha512Context};
    let mut source = File::open(file).at("read", file)?;
    let mut buffer = vec![0; 1 << 20];
    let mut each = |take: &mut dyn FnMut(&[u8])| -> Result<()> {
        loop {
            match source.read(&mut buffer).at("read", file)? {
                0 => return Ok(()),
                count => take(&buffer[..count]),
            }
        }
    };
    Ok(match algorithm {
        Algorithm::Sha256 => {
            let mut context = Sha256Context::new();
            each(&mut |bytes| context.update(bytes))?;
            hex(&context.finish())
        }
        Algorithm::Sha512 => {
            let mut context = Sha512Context::new();
            each(&mut |bytes| context.update(bytes))?;
            hex(&context.finish())
        }
    })
}

/// How an attempt at a download ended, when it didn't end well.
enum Trouble {
    /// The connection failed or stood still. What has arrived is kept, and the rest can be asked for.
    Interrupted(String),
    Final(Error),
}

/// Downloads `url` to `partial`, carrying on from what is there already. Images are hundreds of
/// megabytes, so what has arrived is never given up lightly: a connection that drops is picked
/// up from where it stopped, and what a failed download leaves is there for the next one.
fn download(
    agent: &ureq::Agent,
    url: &str,
    partial: &Path,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(partial)
        .at("write", partial)?;
    let size = |file: &File| file.metadata().map(|found| found.len()).at("look at", partial);
    let mut stalled = 0;
    let mut got_anywhere = false;
    loop {
        let offset = size(&file)?;
        let why = match attempt(agent, url, &mut file, offset, progress) {
            Ok(()) => return file.sync_all().at("write", partial),
            Err(Trouble::Final(error)) => return Err(error),
            Err(Trouble::Interrupted(why)) => why,
        };
        let advanced = size(&file)? > offset;
        got_anywhere |= advanced;
        stalled = if advanced { 0 } else { stalled + 1 };
        // A download that never got anywhere fails at once: the computer is offline, most likely.
        if !got_anywhere || stalled > STALLED_RETRIES {
            return Err(failed(url, why));
        }
        std::thread::sleep(RETRY_DELAY * (stalled + 1));
    }
}

fn attempt(
    agent: &ureq::Agent,
    url: &str,
    file: &mut File,
    offset: u64,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> std::result::Result<(), Trouble> {
    let writing =
        |error: std::io::Error| Trouble::Final(failed(url, format_args!("writing it failed: {error}")));
    let mut request = agent.get(url);
    if offset > 0 {
        request = request.header("Range", format!("bytes={offset}-"));
    }
    let response = request.call().map_err(|error| match error {
        ureq::Error::BadUri(_) | ureq::Error::Http(_) | ureq::Error::RedirectFailed => {
            Trouble::Final(failed(url, error))
        }
        _ => Trouble::Interrupted(error.to_string()),
    })?;
    let header = |name: &str| response.headers().get(name).and_then(|value| value.to_str().ok());
    let length: Option<u64> = header("content-length").and_then(|value| value.parse().ok());
    let status = response.status().as_u16();
    let (start, total) = match status {
        // The whole file, also when the rest of it was asked for.
        200 => (0, length),
        // The rest, if the server says that it starts where what is here ends.
        206 if header("content-range")
            .is_some_and(|range| range.starts_with(&format!("bytes {offset}-"))) =>
        {
            (offset, length.map(|length| offset + length))
        }
        // Nothing is left to send, so all of it is here. The checksum tells whether that is so.
        416 if offset > 0 => return Ok(()),
        408 | 429 | 500..=599 => {
            return Err(Trouble::Interrupted(format!(
                "the server answered with HTTP status {status}"
            )));
        }
        _ => {
            return Err(Trouble::Final(failed(
                url,
                format_args!("the server answered with HTTP status {status}"),
            )));
        }
    };
    file.set_len(start).map_err(writing)?;
    file.seek(SeekFrom::Start(start)).map_err(writing)?;

    let mut body = response.into_body().into_reader();
    let mut buffer = vec![0; 256 << 10];
    let mut received = start;
    progress(received, total);
    loop {
        match body.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                file.write_all(&buffer[..count]).map_err(writing)?;
                received += count as u64;
                progress(received, total);
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(Trouble::Interrupted(error.to_string())),
        }
    }
    match total {
        Some(total) if received < total => Err(Trouble::Interrupted("the connection closed early".into())),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    use super::*;

    #[test]
    fn a_checksum_is_found_in_both_kinds_of_listing() {
        let listing = "0a1b  other.img\n2c3d *disk.img\nSHA256 (fedora.qcow2) = 4e5f\n";
        assert_eq!(listed_digest("disk.img", listing), Some("2c3d"));
        assert_eq!(listed_digest("other.img", listing), Some("0a1b"));
        assert_eq!(listed_digest("fedora.qcow2", listing), Some("4e5f"));
        assert_eq!(listed_digest("img", listing), None);
    }

    /// A server that hangs up 100 000 bytes into the image whenever it is asked for all of it,
    /// and sends the rest when asked for the rest. Returns its address and what it was asked.
    fn serve(image: Vec<u8>, listing: String) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let asked = Arc::new(Mutex::new(Vec::new()));
        let log = asked.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut head = Vec::new();
                for line in BufReader::new(&stream).lines() {
                    let line = line.unwrap();
                    if line.is_empty() {
                        break;
                    }
                    head.push(line.to_ascii_lowercase());
                }
                let from = head.iter().find_map(|line| {
                    line.strip_prefix("range: bytes=")?
                        .strip_suffix('-')?
                        .parse::<usize>()
                        .ok()
                });
                log.lock().unwrap().push(format!("{} {from:?}", head[0]));
                let (status, body, length) = match from {
                    _ if head[0].contains("sums") => {
                        ("200 OK".to_string(), listing.as_bytes(), listing.len())
                    }
                    Some(from) => (
                        format!(
                            "206 Partial Content\r\nContent-Range: bytes {from}-{}/{}",
                            image.len() - 1,
                            image.len()
                        ),
                        &image[from..],
                        image.len() - from,
                    ),
                    None => ("200 OK".to_string(), &image[..100_000], image.len()),
                };
                _ = write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n"
                );
                _ = stream.write_all(body);
            }
        });
        (address, asked)
    }

    #[test]
    fn a_download_is_picked_up_where_it_stopped_and_checked() {
        let contents: Vec<u8> = (0..300_000u32).map(|index| (index % 251) as u8).collect();
        let mut context = graviola::hashing::sha2::Sha256Context::new();
        context.update(&contents);
        let sum = hex(&context.finish());
        let (address, asked) = serve(contents.clone(), format!("{sum} *disk.img\n"));
        let image = Image {
            id: "test".into(),
            name: "Test".into(),
            url: format!("{address}/disk.img"),
            checksums: format!("{address}/sums"),
            algorithm: Algorithm::Sha256,
        };
        let cache = std::env::temp_dir().join(format!("download-test-{}", crate::random_hex(4).unwrap()));
        // What an earlier attempt left isn't the start of this image.
        std::fs::create_dir_all(cache.join("images")).unwrap();
        std::fs::write(cache.join("images/disk.img.partial"), vec![0xEE; 50_000]).unwrap();

        let file = fetch(&cache, &image, &mut |_, _| {}).unwrap();
        assert!(std::fs::read(&file).unwrap() == contents);
        // The rest of what was left is asked for and fails the check. Then all of it is, which
        // is cut short, and the rest of that.
        let requests = asked.lock().unwrap().clone();
        let ranges: Vec<&str> = requests[1..]
            .iter()
            .map(|request| request.rsplit(' ').next().unwrap())
            .collect();
        assert_eq!(ranges, ["Some(50000)", "None", "Some(100000)"]);

        // Checked once, the image is used without asking anyone.
        assert_eq!(fetch(&cache, &image, &mut |_, _| {}).unwrap(), file);
        assert_eq!(asked.lock().unwrap().len(), requests.len());
        std::fs::remove_dir_all(&cache).unwrap();
    }
}
