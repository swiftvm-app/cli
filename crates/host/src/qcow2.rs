//! Reads QCOW2 disk images, the format most cloud images come in, to write them out raw.
//!
//! Versions 2 and 3 with zlib-compressed clusters are read. An image that depends on a backing
//! file, an external data file, encryption, extended L2 entries or zstd compression is refused.

use std::fs::File;
use std::os::unix::fs::FileExt;

use crate::{Error, ErrorCode, Result};

/// "QFI\xfb".
const MAGIC: u32 = 0x5146_49FB;
/// More than any real image needs: at 64 KiB clusters, 32 MiB of first-level table maps 2 PiB.
const LARGEST_L1_TABLE: u64 = 32 << 20;

pub(crate) struct Qcow2<'a> {
    file: &'a File,
    /// The size of the disk that the image holds, in bytes.
    pub virtual_size: u64,
    cluster_bits: u32,
    l1_entries: u64,
    l1_offset: u64,
}

fn unsupported(feature: &str) -> Error {
    Error::new(
        ErrorCode::Image,
        format!("The image uses {feature}, which isn't supported."),
    )
}

fn damaged(why: &str) -> Error {
    Error::new(ErrorCode::Image, format!("The image is damaged: {why}."))
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap())
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_be_bytes(bytes[at..at + 8].try_into().unwrap())
}

/// Reads until `buffer` is full or the file ends, and says how much that was.
fn read_some(file: &File, buffer: &mut [u8], offset: u64) -> std::io::Result<usize> {
    let mut done = 0;
    while done < buffer.len() {
        match file.read_at(&mut buffer[done..], offset + done as u64) {
            Ok(0) => break,
            Ok(count) => done += count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(done)
}

impl<'a> Qcow2<'a> {
    /// `None` if the file isn't a QCOW2 image at all.
    pub fn open(file: &'a File) -> Result<Option<Self>> {
        let mut header = [0; 112];
        let length = read_some(file, &mut header, 0).map_err(|error| damaged(&error.to_string()))?;
        if length < 4 || u32_at(&header, 0) != MAGIC {
            return Ok(None);
        }
        if length < 72 {
            return Err(damaged("its header is cut short"));
        }
        let version = u32_at(&header, 4);
        if version != 2 && version != 3 {
            return Err(unsupported(&format!("version {version} of QCOW2")));
        }
        if u64_at(&header, 8) != 0 {
            return Err(unsupported("a backing file"));
        }
        let cluster_bits = u32_at(&header, 20);
        if !(9..=21).contains(&cluster_bits) {
            return Err(damaged("its cluster size is invalid"));
        }
        if u32_at(&header, 32) != 0 {
            return Err(unsupported("encryption"));
        }
        if version == 3 {
            if length < 104 {
                return Err(damaged("its header is cut short"));
            }
            let incompatible = u64_at(&header, 72);
            if incompatible & 1 << 2 != 0 {
                return Err(unsupported("an external data file"));
            }
            if incompatible & 1 << 4 != 0 {
                return Err(unsupported("extended L2 entries"));
            }
            // The header says how clusters are compressed only when it isn't the usual way.
            if incompatible & 1 << 3 != 0 && u32_at(&header, 100) > 104 && length > 104 && header[104] != 0 {
                return Err(unsupported("zstd compression"));
            }
            if incompatible & !0b11111 != 0 {
                return Err(unsupported("a feature that this program doesn't know"));
            }
        }
        let l1_entries = u32_at(&header, 36) as u64;
        if l1_entries * 8 > LARGEST_L1_TABLE {
            return Err(damaged("its table is too large"));
        }
        Ok(Some(Self {
            file,
            virtual_size: u64_at(&header, 24),
            cluster_bits,
            l1_entries,
            l1_offset: u64_at(&header, 40),
        }))
    }

    fn table(&self, offset: u64, bytes: usize) -> Result<Vec<u8>> {
        let mut table = vec![0; bytes];
        match read_some(self.file, &mut table, offset) {
            Ok(length) if length == bytes => Ok(table),
            _ => Err(damaged("a table is cut short")),
        }
    }

    /// Writes the disk to `output`, which is as large as the disk and all zeros. Clusters of
    /// zeros aren't written, so that the file stays sparse. `progress` is told how far along
    /// this is, from 0 to 1.
    pub fn write_raw(&self, output: &File, progress: &mut dyn FnMut(f64)) -> Result<()> {
        let cluster_size = 1usize << self.cluster_bits;
        let l2_entries = (cluster_size / 8) as u64;
        const OFFSET: u64 = 0x00FF_FFFF_FFFF_FE00;
        const COMPRESSED: u64 = 1 << 62;
        const ZERO: u64 = 1;
        // Where a compressed cluster is and how many sectors it takes share one field, split at
        // a bit that depends on the cluster size.
        let offset_bits = 62 - (self.cluster_bits - 8);
        let compressed_offset = (1u64 << offset_bits) - 1;
        let sector_count = (1u64 << (self.cluster_bits - 8)) - 1;

        let l1 = self.table(self.l1_offset, self.l1_entries as usize * 8)?;
        let mut cluster = vec![0; cluster_size];
        let mut compressed = vec![0; cluster_size + 1024];

        for l1_index in 0..self.l1_entries {
            progress(l1_index as f64 / self.l1_entries as f64);
            let l2_offset = u64_at(&l1, l1_index as usize * 8) & OFFSET;
            if l2_offset == 0 {
                continue;
            }
            let l2 = self.table(l2_offset, cluster_size)?;
            for l2_index in 0..l2_entries {
                let entry = u64_at(&l2, l2_index as usize * 8);
                let Some(guest_offset) = (l1_index * l2_entries + l2_index).checked_mul(cluster_size as u64)
                else {
                    break;
                };
                if entry == 0 || guest_offset >= self.virtual_size {
                    continue;
                }
                if entry & COMPRESSED != 0 {
                    let host_offset = entry & compressed_offset;
                    let sectors = entry >> offset_bits & sector_count;
                    let length = ((sectors + 1) * 512 - (host_offset & 511)) as usize;
                    if length > compressed.len() {
                        return Err(damaged("a compressed cluster is too large"));
                    }
                    // The last one may claim more sectors than the file has left.
                    let length = read_some(self.file, &mut compressed[..length], host_offset)
                        .map_err(|error| damaged(&error.to_string()))?;
                    let decoded = miniz_oxide::inflate::decompress_slice_iter_to_slice(
                        &mut cluster,
                        std::iter::once(&compressed[..length]),
                        false,
                        true,
                    );
                    if decoded != Ok(cluster_size) {
                        return Err(damaged("a compressed cluster can't be read"));
                    }
                } else {
                    let host_offset = entry & OFFSET;
                    if entry & ZERO != 0 || host_offset == 0 {
                        continue;
                    }
                    if !matches!(read_some(self.file, &mut cluster, host_offset), Ok(length) if length == cluster_size)
                    {
                        return Err(damaged("a cluster is cut short"));
                    }
                }
                if cluster.iter().all(|&byte| byte == 0) {
                    continue;
                }
                let length = (self.virtual_size - guest_offset).min(cluster_size as u64) as usize;
                output
                    .write_all_at(&cluster[..length], guest_offset)
                    .map_err(|error| {
                        Error::new(ErrorCode::Io, format!("Couldn't write the disk: {error}."))
                    })?;
            }
        }
        progress(1.0);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Seek, SeekFrom, Write};

    use super::*;

    /// An image with 4 KiB clusters and a disk of five of them: one stored as it is, one
    /// compressed, one marked as zeros, one never written, and half a one at the end.
    #[test]
    fn an_image_is_written_out_raw() {
        const CLUSTER: usize = 4096;
        let plain: Vec<u8> = (0..CLUSTER).map(|index| (index % 251) as u8).collect();
        let squeezed = vec![7u8; CLUSTER];
        let last: Vec<u8> = (0..CLUSTER).map(|index| (index % 13) as u8 + 1).collect();
        let deflated = miniz_oxide::deflate::compress_to_vec(&squeezed, 6);
        let virtual_size = 4 * CLUSTER as u64 + 2048;

        // Clusters of the file: header, L1, L2, the plain one, the compressed one, the last one.
        let mut image = vec![0u8; 6 * CLUSTER];
        image[0..4].copy_from_slice(&MAGIC.to_be_bytes());
        image[4..8].copy_from_slice(&3u32.to_be_bytes());
        image[20..24].copy_from_slice(&12u32.to_be_bytes());
        image[24..32].copy_from_slice(&virtual_size.to_be_bytes());
        image[36..40].copy_from_slice(&1u32.to_be_bytes());
        image[40..48].copy_from_slice(&(CLUSTER as u64).to_be_bytes());
        image[100..104].copy_from_slice(&104u32.to_be_bytes());
        image[CLUSTER..CLUSTER + 8].copy_from_slice(&(2 * CLUSTER as u64).to_be_bytes());
        // With 4 KiB clusters a compressed entry keeps its sector count from bit 58 up.
        let sectors = (deflated.len() as u64).div_ceil(512) - 1;
        let entries = [
            (3 * CLUSTER as u64) | (1 << 63),
            COMPRESSED_FLAG | (sectors << 58) | (4 * CLUSTER as u64),
            1,
            0,
            (5 * CLUSTER as u64) | (1 << 63),
        ];
        for (index, entry) in entries.iter().enumerate() {
            image[2 * CLUSTER + index * 8..][..8].copy_from_slice(&entry.to_be_bytes());
        }
        image[3 * CLUSTER..][..CLUSTER].copy_from_slice(&plain);
        image[4 * CLUSTER..][..deflated.len()].copy_from_slice(&deflated);
        image[5 * CLUSTER..][..CLUSTER].copy_from_slice(&last);

        let directory = std::env::temp_dir().join(format!("qcow2-test-{}", crate::random_hex(4).unwrap()));
        std::fs::create_dir_all(&directory).unwrap();
        let mut source = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(directory.join("image.qcow2"))
            .unwrap();
        source.write_all(&image).unwrap();
        let mut output = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(directory.join("disk.raw"))
            .unwrap();
        output.set_len(virtual_size).unwrap();

        let qcow2 = Qcow2::open(&source).unwrap().expect("a QCOW2 image");
        assert_eq!(qcow2.virtual_size, virtual_size);
        qcow2.write_raw(&output, &mut |_| {}).unwrap();

        let mut raw = Vec::new();
        output.seek(SeekFrom::Start(0)).unwrap();
        output.read_to_end(&mut raw).unwrap();
        let mut expected = [plain, squeezed, vec![0; 2 * CLUSTER], last].concat();
        expected.truncate(virtual_size as usize);
        assert!(raw == expected);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    const COMPRESSED_FLAG: u64 = 1 << 62;
}
