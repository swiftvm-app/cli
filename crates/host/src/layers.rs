//! The sealed layers that machines' disks are built on, kept in the store and shared.
//!
//! ```text
//! <store>/layers/<hash>.layer           a sealed layer, named by the SHA-256 of its file
//! <store>/layers/images/<file>.json     which layer an image of the catalog became
//! <store>/layers/images/<file>.lock     held while it is being made
//! <store>/layers/.import-<file>.layer   one that is being made
//! ```
//!
//! A layer is never changed once it has its name. Nothing counts who uses one: that is worked out
//! from the headers of the machines' disks and of the other layers when it is asked.

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use disk::layer::{from_hex, hash_file, to_hex};
use disk::{Header, Layer, Options};
use serde::{Deserialize, Serialize};

use crate::images::Image;
use crate::lock::Lock;
use crate::store::{Config, DiskFormat, Store};
use crate::{Context, Error, ErrorCode, Result, download, import};

/// A chain longer than this is taken to be damaged.
const DEEPEST: usize = 64;

/// Which layer an image became.
#[derive(Serialize, Deserialize)]
struct Made {
    url: String,
    /// The image's checksum, as its publisher gave it.
    digest: String,
    layer: String,
}

pub(crate) fn directory(store: &Store) -> PathBuf {
    store.root().join("layers")
}

/// Where the sealed layer with this hash is.
pub(crate) fn path(store: &Store, hash: &[u8; 32]) -> PathBuf {
    directory(store).join(format!("{}.layer", to_hex(hash)))
}

/// The hash of the sealed layer that `image` became, made of `file`, its checked copy in the
/// cache, unless that has been done before. `progress` hears how far along that is, from 0 to 1.
pub(crate) fn base(
    store: &Store,
    image: &Image,
    file: &Path,
    progress: &mut dyn FnMut(f64),
) -> Result<[u8; 32]> {
    let notes = directory(store).join("images");
    std::fs::create_dir_all(&notes).at("create", &notes)?;
    let name = image.file_name();
    let lock = notes.join(format!("{name}.lock"));
    // One process makes it, and anyone else who needs it at the same time waits for that one.
    let _making = Lock::take(&lock).at("lock", &lock)?;
    let note = notes.join(format!("{name}.json"));
    let digest = download::digest_of(file).unwrap_or_default();
    // Without a checksum to go by, nothing says the layer was made of this image.
    let made = std::fs::read(&note)
        .ok()
        .and_then(|text| serde_json::from_slice::<Made>(&text).ok())
        .filter(|made| !digest.is_empty() && made.url == image.url && made.digest == digest)
        .and_then(|made| from_hex(&made.layer))
        .filter(|hash| path(store, hash).is_file());
    if let Some(hash) = made {
        return Ok(hash);
    }

    let partial = directory(store).join(format!(".import-{name}.layer"));
    // Left by an import that was cut short, which held the lock this one holds now.
    _ = std::fs::remove_file(&partial);
    let result = (|| {
        Layer::create(&partial, import::virtual_size(file)?, None).at("create", &partial)?;
        let options = Options {
            commit_interval: None,
            ..Options::default()
        };
        let layer = Layer::open(&partial, &options, None).at("open", &partial)?;
        import::write(file, &layer, progress)?;
        layer.seal().at("write", &partial)?;
        drop(layer);
        // Closing gave room back. The file is named by what it holds, so that has to be on the
        // disk before it is hashed.
        std::fs::File::open(&partial)
            .and_then(|file| file.sync_all())
            .at("write", &partial)?;
        let hash = hash_file(&partial).at("read", &partial)?;
        std::fs::set_permissions(&partial, std::fs::Permissions::from_mode(0o444)).at("protect", &partial)?;
        let destination = path(store, &hash);
        std::fs::rename(&partial, &destination).at("create", &destination)?;
        let made = Made {
            url: image.url.clone(),
            digest,
            layer: to_hex(&hash),
        };
        let written = notes.join(format!("{name}.json.new"));
        std::fs::write(&written, serde_json::to_vec_pretty(&made).expect("plain data"))
            .at("write", &written)?;
        std::fs::rename(&written, &note).at("write", &note)?;
        // The renames are kept by the folders that hold them.
        for folder in [directory(store), notes.clone()] {
            std::fs::File::open(&folder)
                .and_then(|folder| folder.sync_all())
                .at("write", &folder)?;
        }
        Ok(hash)
    })();
    if result.is_err() {
        _ = std::fs::remove_file(&partial);
    }
    result
}

fn damaged(path: &Path, why: impl std::fmt::Display) -> Error {
    Error::new(ErrorCode::Io, format!("{} can't be used: {why}.", path.display()))
}

/// Opens the layer at `path` as `options` say, and the layers it is built on for reading.
pub(crate) fn open(store: &Store, path: &Path, options: &Options) -> Result<Arc<Layer>> {
    open_at_depth(store, path, options, 0)
}

fn open_at_depth(store: &Store, path: &Path, options: &Options, depth: usize) -> Result<Arc<Layer>> {
    if depth > DEEPEST {
        return Err(damaged(path, "it is built on too many layers"));
    }
    let header = Header::read(path).map_err(|error| damaged(path, error))?;
    let parent = match header.parent {
        None => None,
        Some(hash) => Some(open_parent(store, path, &hash, depth)?),
    };
    Layer::open(path, options, parent)
        .map(Arc::new)
        .map_err(|error| damaged(path, error))
}

fn open_parent(store: &Store, child: &Path, hash: &[u8; 32], depth: usize) -> Result<Arc<Layer>> {
    let path = path(store, hash);
    if !path.is_file() {
        return Err(Error::new(
            ErrorCode::NotFound,
            format!(
                "{} is built on a layer that isn't there: {}.",
                child.display(),
                path.display()
            ),
        ));
    }
    let parent = open_at_depth(store, &path, &Options::read_only(), depth + 1)?;
    if !parent.header().sealed {
        return Err(damaged(&path, "it isn't sealed"));
    }
    Ok(parent)
}

/// The files of the layers that the layer at `path` is built on, nearest first.
pub(crate) fn chain(store: &Store, path: &Path) -> Result<Vec<PathBuf>> {
    let mut chain = Vec::new();
    let mut next = Header::read(path).map_err(|error| damaged(path, error))?.parent;
    while let Some(hash) = next {
        if chain.len() > DEEPEST {
            return Err(damaged(path, "it is built on too many layers"));
        }
        let parent = self::path(store, &hash);
        next = Header::read(&parent)
            .map_err(|error| damaged(&parent, error))?
            .parent;
        chain.push(parent);
    }
    Ok(chain)
}

/// How many layers are built on each sealed layer: other sealed layers, and machines' disks.
pub(crate) fn users(store: &Store) -> HashMap<[u8; 32], usize> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(directory(store))
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension().is_some_and(|extension| extension == "layer")
                && path
                    .file_stem()
                    .and_then(|stem| from_hex(&stem.to_string_lossy()))
                    .is_some()
        })
        .collect();
    for name in store.names().unwrap_or_default() {
        let directory = store.directory(&name);
        if let Ok(config) = Config::load(&directory)
            && config.disk.format == DiskFormat::Layer
        {
            files.push(directory.join(&config.disk.file));
        }
    }
    let mut users = HashMap::new();
    for file in files {
        if let Ok(Header {
            parent: Some(parent), ..
        }) = Header::read(&file)
        {
            *users.entry(parent).or_default() += 1;
        }
    }
    users
}

#[cfg(test)]
mod tests {
    use disk::Disk;

    use super::*;
    use crate::images::Algorithm;

    #[test]
    fn an_image_becomes_one_sealed_layer_that_disks_are_built_on() {
        let root = std::env::temp_dir().join(format!("layers-test-{}", crate::random_hex(4).unwrap()));
        std::fs::create_dir_all(&root).unwrap();
        let store = Store::new(&root);
        // A raw image with a cluster of zeros in it, that doesn't end on a cluster's edge.
        let mut contents: Vec<u8> = (0..300_000).map(|index| (index % 251) as u8).collect();
        contents[65536..131072].fill(0);
        let file = root.join("image.raw");
        std::fs::write(&file, &contents).unwrap();
        // What the cache notes about an image it has checked, which the layer is found by.
        let checked = serde_json::json!({
            "url": "https://example.invalid/image.raw",
            "algorithm": "sha256",
            "digest": "00",
            "size": contents.len(),
        });
        std::fs::write(root.join("image.raw.json"), checked.to_string()).unwrap();
        let image = Image {
            id: "test".into(),
            name: "Test".into(),
            url: "https://example.invalid/image.raw".into(),
            checksums: String::new(),
            algorithm: Algorithm::Sha256,
        };

        let hash = base(&store, &image, &file, &mut |_| {}).unwrap();
        let sealed = path(&store, &hash);
        let header = Header::read(&sealed).unwrap();
        assert!(header.sealed);
        assert_eq!(header.capacity, contents.len() as u64);
        let made = std::fs::metadata(&sealed).unwrap().modified().unwrap();
        // The second time it is found, not made again.
        assert_eq!(
            base(&store, &image, &file, &mut |_| panic!("made again")).unwrap(),
            hash
        );
        assert_eq!(std::fs::metadata(&sealed).unwrap().modified().unwrap(), made);

        let disk = root.join("disk.layer");
        Layer::create(&disk, 1 << 20, Some(hash)).unwrap();
        assert_eq!(chain(&store, &disk).unwrap(), std::slice::from_ref(&sealed));
        let layer = open(&store, &disk, &Options::default()).unwrap();
        let mut read = vec![0xAA; 1 << 20];
        layer.read(0, &mut read).unwrap();
        contents.resize(1 << 20, 0);
        assert!(read == contents);
        layer.close().unwrap();

        // Without the layer underneath, the disk can't be opened.
        std::fs::remove_file(&sealed).unwrap();
        assert_eq!(
            open(&store, &disk, &Options::default()).err().unwrap().code,
            ErrorCode::NotFound
        );
        std::fs::remove_dir_all(&root).unwrap();
    }
}
