//! Turns a downloaded image into a machine's disk.

use std::fs::File;
use std::path::Path;

use crate::qcow2::Qcow2;
use crate::{Context, Error, ErrorCode, Result};

/// Writes `image`, QCOW2 or raw, as a sparse raw disk of `size` bytes at `destination`. The guest
/// grows its system into the room that is left when it first starts.
pub(crate) fn install(
    image: &Path,
    destination: &Path,
    size: u64,
    progress: &mut dyn FnMut(f64),
) -> Result<()> {
    let source = File::open(image).at("read", image)?;
    let too_small = |needed: u64| {
        Error::new(
            ErrorCode::InvalidArgument,
            format!("The image needs a disk of {needed} bytes, and {size} were asked for."),
        )
    };
    let disk = match Qcow2::open(&source)? {
        Some(qcow2) => {
            if qcow2.virtual_size > size {
                return Err(too_small(qcow2.virtual_size));
            }
            let disk = File::create(destination).at("create", destination)?;
            disk.set_len(size).at("size", destination)?;
            qcow2.write_raw(&disk, progress)?;
            disk
        }
        None => {
            let length = source.metadata().at("look at", image)?.len();
            if length > size {
                return Err(too_small(length));
            }
            // Where the file system can, the copy shares its blocks with the cache and takes no time.
            std::fs::copy(image, destination).at("copy the image to", destination)?;
            let disk = File::options()
                .write(true)
                .open(destination)
                .at("open", destination)?;
            disk.set_len(size).at("size", destination)?;
            progress(1.0);
            disk
        }
    };
    // The cached image can go at any time: what was made of it has to be on the disk by itself.
    disk.sync_all().at("write", destination)
}
