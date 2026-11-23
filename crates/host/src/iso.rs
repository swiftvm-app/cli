//! Writes small ISO 9660 images, such as the one a guest's first start reads its settings from.
//!
//! Files are in the image's root, under their real names in a Joliet directory. That is the one
//! Linux reads. The plain directory, which is limited to names like `USER_D01`, is there because
//! the format asks for it.

use std::collections::BTreeMap;
use std::path::Path;

use crate::{Context, Error, ErrorCode, Result};

const SECTOR: usize = 2048;

// What is where, in sectors. The first 16 are left to the system that boots from the disc. Three
// descriptors follow: the plain one, Joliet's, and one that says that there are no more.
const FIRST_DESCRIPTOR: u32 = 16;
const PRIMARY_PATHS: u32 = 19;
const JOLIET_PATHS: u32 = 21;
const PRIMARY_ROOT: u32 = 23;
const JOLIET_ROOT: u32 = 24;
const FIRST_FILE: u32 = 25;

/// The one entry of a path table: the root.
const PATH_TABLE_LENGTH: u32 = 10;

fn both_u32(value: u32) -> [u8; 8] {
    let mut bytes = [0; 8];
    bytes[..4].copy_from_slice(&value.to_le_bytes());
    bytes[4..].copy_from_slice(&value.to_be_bytes());
    bytes
}

fn both_u16(value: u16) -> [u8; 4] {
    let mut bytes = [0; 4];
    bytes[..2].copy_from_slice(&value.to_le_bytes());
    bytes[2..].copy_from_slice(&value.to_be_bytes());
    bytes
}

fn ucs2(text: &str) -> Vec<u8> {
    text.encode_utf16().flat_map(u16::to_be_bytes).collect()
}

/// An entry of a directory. No date is given: the format lets it be left out.
fn record(identifier: &[u8], sector: u32, length: u32, is_directory: bool) -> Vec<u8> {
    let mut record = vec![0; 33];
    record[2..10].copy_from_slice(&both_u32(sector));
    record[10..18].copy_from_slice(&both_u32(length));
    record[25] = if is_directory { 2 } else { 0 };
    record[28..32].copy_from_slice(&both_u16(1));
    record[32] = identifier.len() as u8;
    record.extend(identifier);
    if record.len() % 2 == 1 {
        record.push(0);
    }
    record[0] = record.len() as u8;
    record
}

struct Entry<'a> {
    plain: Vec<u8>,
    joliet: Vec<u8>,
    sector: u32,
    contents: &'a [u8],
}

fn directory(root: u32, entries: &[Entry], joliet: bool) -> Result<Vec<u8>> {
    // The folder itself and the one above it, which for the root is itself.
    let mut sector = record(&[0], root, SECTOR as u32, true);
    sector.extend(record(&[1], root, SECTOR as u32, true));
    for entry in entries {
        let identifier = if joliet { &entry.joliet } else { &entry.plain };
        sector.extend(record(
            identifier,
            entry.sector,
            entry.contents.len() as u32,
            false,
        ));
    }
    if sector.len() > SECTOR {
        return Err(Error::new(
            ErrorCode::Io,
            "Too many files for the disc that sets a machine up.",
        ));
    }
    sector.resize(SECTOR, 0);
    Ok(sector)
}

/// A volume descriptor: the plain one, or the one that says where the Joliet directory is.
fn descriptor(volume: &str, sectors: u32, joliet: bool) -> Vec<u8> {
    let (paths, root) = if joliet {
        (JOLIET_PATHS, JOLIET_ROOT)
    } else {
        (PRIMARY_PATHS, PRIMARY_ROOT)
    };
    let mut sector = vec![0; SECTOR];
    sector[0] = if joliet { 2 } else { 1 };
    sector[1..6].copy_from_slice(b"CD001");
    sector[6] = 1;
    // The system's and the volume's names, filled up with spaces.
    if joliet {
        sector[8..72].as_chunks_mut().0.fill([0, b' ']);
        let name = ucs2(volume);
        sector[40..40 + name.len()].copy_from_slice(&name);
        // Says that names are UCS-2, which is what makes this descriptor Joliet's.
        sector[88..91].copy_from_slice(b"%/E");
    } else {
        sector[8..72].fill(b' ');
        sector[40..40 + volume.len()].copy_from_slice(volume.as_bytes());
    }
    sector[80..88].copy_from_slice(&both_u32(sectors));
    sector[120..124].copy_from_slice(&both_u16(1));
    sector[124..128].copy_from_slice(&both_u16(1));
    sector[128..132].copy_from_slice(&both_u16(SECTOR as u16));
    sector[132..140].copy_from_slice(&both_u32(PATH_TABLE_LENGTH));
    sector[140..144].copy_from_slice(&paths.to_le_bytes());
    sector[148..152].copy_from_slice(&(paths + 1).to_be_bytes());
    sector[156..190].copy_from_slice(&record(&[0], root, SECTOR as u32, true));
    // Names of the set, the publisher and so on, then four dates: all left out.
    for field in [
        190..318,
        318..446,
        446..574,
        574..702,
        702..739,
        739..776,
        776..813,
    ] {
        if joliet {
            sector[field].as_chunks_mut().0.fill([0, b' ']);
        } else {
            sector[field].fill(b' ');
        }
    }
    for date in sector[813..881].chunks_mut(17) {
        date[..16].fill(b'0');
        date[16] = 0;
    }
    sector[881] = 1;
    sector
}

fn path_table(root: u32, big_endian: bool) -> Vec<u8> {
    let mut sector = vec![0; SECTOR];
    sector[0] = 1;
    sector[2..6].copy_from_slice(&if big_endian {
        root.to_be_bytes()
    } else {
        root.to_le_bytes()
    });
    sector[6..8].copy_from_slice(&if big_endian {
        1u16.to_be_bytes()
    } else {
        1u16.to_le_bytes()
    });
    sector
}

/// Writes an image at `path` whose volume is called `volume` and holds `files`, by their names.
pub(crate) fn write(path: &Path, volume: &str, files: &BTreeMap<&str, Vec<u8>>) -> Result<()> {
    assert!(
        volume.is_ascii() && volume.len() <= 16,
        "a volume's name is short and plain"
    );
    let mut entries = Vec::new();
    let mut next = FIRST_FILE;
    for (index, (name, contents)) in files.iter().enumerate() {
        // Up to six characters of the name, and a number that keeps the names apart.
        let stem: String = name
            .chars()
            .take(6)
            .map(|character| match character.to_ascii_uppercase() {
                kept @ ('A'..='Z' | '0'..='9') => kept,
                _ => '_',
            })
            .collect();
        entries.push(Entry {
            plain: format!("{stem}{index:02}.;1").into_bytes(),
            joliet: ucs2(&format!("{name};1")),
            sector: next,
            contents,
        });
        next += contents.len().div_ceil(SECTOR) as u32;
    }

    let mut image = vec![0; FIRST_DESCRIPTOR as usize * SECTOR];
    image.extend(descriptor(volume, next, false));
    image.extend(descriptor(volume, next, true));
    let mut terminator = vec![0; SECTOR];
    terminator[0] = 255;
    terminator[1..6].copy_from_slice(b"CD001");
    terminator[6] = 1;
    image.extend(terminator);
    debug_assert_eq!(image.len(), PRIMARY_PATHS as usize * SECTOR);
    for (root, big_endian) in [
        (PRIMARY_ROOT, false),
        (PRIMARY_ROOT, true),
        (JOLIET_ROOT, false),
        (JOLIET_ROOT, true),
    ] {
        image.extend(path_table(root, big_endian));
    }
    image.extend(directory(PRIMARY_ROOT, &entries, false)?);
    image.extend(directory(JOLIET_ROOT, &entries, true)?);
    for entry in &entries {
        image.extend(entry.contents);
        image.resize(image.len().next_multiple_of(SECTOR), 0);
    }
    std::fs::write(path, image).at("write", path)
}
