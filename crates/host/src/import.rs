//! Reads a downloaded image, QCOW2 or raw, into a layer.

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::Path;

use disk::Disk;

use crate::qcow2::Qcow2;
use crate::{Context, Result};

/// The size of the disk that `image` holds, in bytes.
pub(crate) fn virtual_size(image: &Path) -> Result<u64> {
    let source = File::open(image).at("read", image)?;
    match Qcow2::open(&source)? {
        Some(qcow2) => Ok(qcow2.virtual_size),
        None => source
            .metadata()
            .at("look at", image)
            .map(|metadata| metadata.len()),
    }
}

/// Writes what `image` holds into `disk`, which is at least as large and all zeros. Zeros aren't
/// written. `progress` is told how far along this is, from 0 to 1.
pub(crate) fn write(image: &Path, disk: &dyn Disk, progress: &mut dyn FnMut(f64)) -> Result<()> {
    let source = File::open(image).at("read", image)?;
    let mut output = |offset: u64, bytes: &[u8]| -> Result<()> {
        if bytes.iter().all(|&byte| byte == 0) {
            return Ok(());
        }
        disk.write(offset, bytes).context("write the disk")
    };
    if let Some(qcow2) = Qcow2::open(&source)? {
        return qcow2.read_into(&mut output, progress);
    }
    let length = source.metadata().at("look at", image)?.len();
    let mut chunk = vec![0; 1 << 20];
    let mut offset = 0;
    while offset < length {
        let count = (length - offset).min(chunk.len() as u64) as usize;
        source
            .read_exact_at(&mut chunk[..count], offset)
            .at("read", image)?;
        // A cluster at a time, so that what is zeros stays out of the disk.
        for (index, cluster) in chunk[..count].chunks(disk::layer::CLUSTER).enumerate() {
            output(offset + (index * disk::layer::CLUSTER) as u64, cluster)?;
        }
        offset += count as u64;
        progress(offset as f64 / length as f64);
    }
    progress(1.0);
    Ok(())
}
