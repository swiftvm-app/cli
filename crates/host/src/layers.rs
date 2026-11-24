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

/// A layer that is being made and hasn't been written to for this long was left by a program
/// that ended before it was done.
const ABANDONED: std::time::Duration = std::time::Duration::from_secs(3600);

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
        let _changing = store.lock()?;
        let hash = publish(store, &partial)?;
        let made = Made {
            url: image.url.clone(),
            digest,
            layer: to_hex(&hash),
        };
        let written = notes.join(format!("{name}.json.new"));
        std::fs::write(&written, serde_json::to_vec_pretty(&made).expect("plain data"))
            .at("write", &written)?;
        std::fs::rename(&written, &note).at("write", &note)?;
        sync_folder(&notes)?;
        Ok(hash)
    })();
    if result.is_err() {
        _ = std::fs::remove_file(&partial);
    }
    result
}

/// Makes a rename in `folder` survive a crash of the host.
pub(crate) fn sync_folder(folder: &Path) -> Result<()> {
    std::fs::File::open(folder)
        .and_then(|folder| folder.sync_all())
        .at("write", folder)
}

/// Names the sealed layer at `partial` by its hash and moves it among the others. Called with the
/// store's lock held: until whatever uses it is written down, a sweep would take it for unused.
pub(crate) fn publish(store: &Store, partial: &Path) -> Result<[u8; 32]> {
    // Closing gave room back. The file is named by what it holds, so that has to be on the disk
    // before it is hashed.
    std::fs::File::open(partial)
        .and_then(|file| file.sync_all())
        .at("write", partial)?;
    let hash = hash_file(partial).at("read", partial)?;
    std::fs::set_permissions(partial, std::fs::Permissions::from_mode(0o444)).at("protect", partial)?;
    let destination = path(store, &hash);
    std::fs::rename(partial, &destination).at("create", &destination)?;
    sync_folder(&directory(store))?;
    Ok(hash)
}

/// Makes a machine's layer at `top`, which has been sealed, a layer of the store, and puts a new
/// empty layer on top of it in its place. Returns the sealed layer's hash.
///
/// Each step leaves a disk that can be used: until the new layer is renamed into place, `top` is
/// the sealed layer, and [`recover`] does the rest.
pub(crate) fn finish_seal(store: &Store, top: &Path) -> Result<[u8; 32]> {
    let header = Header::read(top).map_err(|error| damaged(top, error))?;
    if !header.sealed {
        return Err(damaged(top, "it isn't sealed"));
    }
    std::fs::File::open(top)
        .and_then(|file| file.sync_all())
        .at("write", top)?;
    let hash = hash_file(top).at("read", top)?;
    let _changing = store.lock()?;
    std::fs::create_dir_all(directory(store)).at("create", &directory(store))?;
    let sealed = path(store, &hash);
    // Another layer with the same contents is the same layer.
    if !sealed.is_file() {
        std::fs::hard_link(top, &sealed).at("create", &sealed)?;
        std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o444)).at("protect", &sealed)?;
        sync_folder(&directory(store))?;
    }
    let mut name = top.as_os_str().to_owned();
    name.push(".new");
    let next = PathBuf::from(name);
    _ = std::fs::remove_file(&next);
    Layer::create(&next, header.capacity, Some(hash)).at("create", &next)?;
    std::fs::rename(&next, top).at("create", top)?;
    if let Some(folder) = top.parent() {
        sync_folder(folder)?;
    }
    Ok(hash)
}

/// Finishes the sealing of a machine's layer that was cut short. A machine's own layer is only
/// ever sealed on its way to becoming a layer of the store.
pub(crate) fn recover(store: &Store, top: &Path) -> Result<()> {
    let header = Header::read(top).map_err(|error| damaged(top, error))?;
    if header.sealed {
        finish_seal(store, top)?;
    }
    Ok(())
}

/// Seals the layer of a machine that is stopped, and puts a new one on top of it: what a
/// snapshot is. Returns the sealed layer's hash.
pub(crate) fn seal_top(store: &Store, top: &Path) -> Result<[u8; 32]> {
    recover(store, top)?;
    let options = Options {
        commit_interval: None,
        ..Options::default()
    };
    let layer = open(store, top, &options)?;
    layer.seal().at("write", top)?;
    drop(layer);
    finish_seal(store, top)
}

/// Writes what the disk whose layer is `top` holds into one new sealed layer that is built on
/// nothing, compressed at `level`, and returns where it is: a file among the layers that is
/// [`publish`]ed next, or removed. `progress` hears how far along it is, from 0 to 1.
pub(crate) fn flatten(
    store: &Store,
    top: &Path,
    level: i32,
    progress: &mut dyn FnMut(f64),
) -> Result<PathBuf> {
    use std::sync::atomic::{AtomicU64, Ordering};

    use disk::Disk;

    let source = open(store, top, &Options::read_only())?;
    let capacity = source.capacity();
    let partial = directory(store).join(format!(".save-{}.layer", crate::random_hex(8)?));
    std::fs::create_dir_all(directory(store)).at("create", &directory(store))?;
    let result = (|| {
        Layer::create(&partial, capacity, None).at("create", &partial)?;
        let options = Options {
            commit_interval: None,
            level,
            ..Options::default()
        };
        let target = Layer::open(&partial, &options, None).at("open", &partial)?;
        let cluster = disk::layer::CLUSTER as u64;
        let clusters = capacity.div_ceil(cluster);
        let next = AtomicU64::new(0);
        let done = AtomicU64::new(0);
        let threads = std::thread::available_parallelism().map_or(1, |count| count.get());
        std::thread::scope(|scope| -> Result<()> {
            let workers: Vec<_> = (0..threads)
                .map(|_| {
                    scope.spawn(|| -> std::io::Result<()> {
                        let mut buffer = vec![0; cluster as usize];
                        loop {
                            let index = next.fetch_add(1, Ordering::SeqCst);
                            if index >= clusters {
                                return Ok(());
                            }
                            let offset = index * cluster;
                            let length = cluster.min(capacity - offset) as usize;
                            let data = &mut buffer[..length];
                            let read = source.read(offset, data);
                            // Nothing is left to do for the others either.
                            if let Err(error) = read.and_then(|()| {
                                if data.iter().any(|&byte| byte != 0) {
                                    target.write(offset, data)?;
                                }
                                Ok(())
                            }) {
                                next.store(clusters, Ordering::SeqCst);
                                return Err(error);
                            }
                            done.fetch_add(1, Ordering::Relaxed);
                        }
                    })
                })
                .collect();
            while !workers.iter().all(|worker| worker.is_finished()) {
                progress(done.load(Ordering::Relaxed) as f64 / clusters.max(1) as f64);
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            for worker in workers {
                worker.join().expect("a worker panicked").at("write", &partial)?;
            }
            Ok(())
        })?;
        progress(1.0);
        target.seal().at("write", &partial)?;
        Ok(())
    })();
    match result {
        Ok(()) => Ok(partial),
        Err(error) => {
            _ = std::fs::remove_file(&partial);
            Err(error)
        }
    }
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

/// The sealed layers that each machine's disk and snapshots are built on, by the machine's name:
/// `None` for one that is being created.
fn machines_chains(store: &Store) -> Vec<(Option<String>, Vec<[u8; 32]>)> {
    let mut found = Vec::new();
    for (folder, name) in store.folders() {
        let mut roots = Vec::new();
        // Its disk, and a new one that a seal that was cut short may have left.
        for entry in std::fs::read_dir(&folder).into_iter().flatten().flatten() {
            let file = entry.path();
            let is_layer = file
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".layer") || name.ends_with(".layer.new"));
            if is_layer && let Ok(header) = Header::read(&file) {
                roots.extend(header.parent);
                // A machine's layer that was sealed on its way into the store, which a hard link
                // may already have put there.
                if header.sealed
                    && let Ok(hash) = hash_file(&file)
                {
                    roots.push(hash);
                }
            }
        }
        if let Ok(config) = Config::load(&folder) {
            roots.extend(
                config
                    .snapshots
                    .iter()
                    .filter_map(|snapshot| from_hex(&snapshot.layer)),
            );
        }
        found.push((name, closure(store, roots)));
    }
    found
}

/// `roots`, and every layer that they are built on.
fn closure(store: &Store, roots: Vec<[u8; 32]>) -> Vec<[u8; 32]> {
    let mut seen = Vec::new();
    let mut waiting = roots;
    while let Some(hash) = waiting.pop() {
        if seen.contains(&hash) || seen.len() > DEEPEST * 64 {
            continue;
        }
        seen.push(hash);
        if let Ok(Header {
            parent: Some(parent), ..
        }) = Header::read(&path(store, &hash))
        {
            waiting.push(parent);
        }
    }
    seen
}

/// The machines whose disk or snapshots are built on the sealed layer `hash`. One that is being
/// created is called so.
pub(crate) fn dependents(store: &Store, hash: &[u8; 32]) -> Vec<String> {
    let mut names: Vec<String> = machines_chains(store)
        .into_iter()
        .filter(|(_, chain)| chain.contains(hash))
        .map(|(name, _)| name.unwrap_or_else(|| "a machine that is being created".into()))
        .collect();
    names.sort();
    names
}

/// Removes the sealed layers that nothing uses: no machine's disk or snapshot, no image, and no
/// other layer that is used. Takes the store's lock.
pub(crate) fn sweep(store: &Store) -> Result<()> {
    let _changing = store.lock()?;
    let mut roots: Vec<[u8; 32]> = machines_chains(store)
        .into_iter()
        .flat_map(|(_, chain)| chain)
        .collect();
    roots.extend(
        crate::saved::all(store)
            .iter()
            .filter_map(|image| from_hex(&image.layer)),
    );
    let notes = directory(store).join("images");
    for entry in std::fs::read_dir(&notes).into_iter().flatten().flatten() {
        if let Ok(text) = std::fs::read(entry.path())
            && let Ok(made) = serde_json::from_slice::<Made>(&text)
            && let Some(hash) = from_hex(&made.layer)
        {
            roots.push(hash);
        }
    }
    // A note that can't be read may still name a layer: nothing is removed rather than too much.
    let unreadable = std::fs::read_dir(store.root().join("images"))
        .into_iter()
        .flatten()
        .flatten()
        .any(|entry| {
            let path = entry.path();
            path.extension().is_some_and(|extension| extension == "json")
                && !path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with('.'))
                && std::fs::read(&path)
                    .ok()
                    .and_then(|text| serde_json::from_slice::<crate::saved::SavedImage>(&text).ok())
                    .is_none()
        });
    let unreadable = unreadable
        || store
            .folders()
            .iter()
            .any(|(folder, _)| folder.join(Config::FILE).exists() && Config::load(folder).is_err());
    if unreadable {
        return Ok(());
    }
    let used = closure(store, roots);
    for entry in std::fs::read_dir(directory(store))
        .into_iter()
        .flatten()
        .flatten()
    {
        let file = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        // What a save or an import that was cut short left: one under way writes it all along.
        if (name.starts_with(".save-") || name.starts_with(".import-")) && name.ends_with(".layer") {
            let idle = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .ok()
                .and_then(|modified| modified.elapsed().ok());
            if idle.is_some_and(|idle| idle > ABANDONED) {
                _ = std::fs::remove_file(&file);
            }
            continue;
        }
        let Some(hash) = name.strip_suffix(".layer").and_then(from_hex) else {
            continue;
        };
        if !used.contains(&hash) {
            std::fs::remove_file(&file).at("delete", &file)?;
        }
    }
    Ok(())
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

    /// A store with a machine called `m` whose disk is built on a sealed base layer. The base
    /// holds 1s in its first cluster and the machine 2s in its second.
    struct Fixture {
        store: Store,
        machine: PathBuf,
        top: PathBuf,
        base: [u8; 32],
    }

    const SIZE: u64 = 1 << 20;

    impl Fixture {
        fn new(test: &str) -> Self {
            let root = std::env::temp_dir().join(format!("layers-{test}-{}", crate::random_hex(4).unwrap()));
            let store = Store::new(&root);
            std::fs::create_dir_all(directory(&store)).unwrap();
            let partial = directory(&store).join(".base.layer");
            Layer::create(&partial, SIZE, None).unwrap();
            let layer = Layer::open(&partial, &Options::default(), None).unwrap();
            layer.write(0, &[1; 65536]).unwrap();
            layer.seal().unwrap();
            drop(layer);
            let base = publish(&store, &partial).unwrap();
            let machine = store.directory("m");
            std::fs::create_dir_all(&machine).unwrap();
            let top = machine.join("disk.layer");
            Layer::create(&top, SIZE, Some(base)).unwrap();
            let fixture = Self {
                store,
                machine,
                top,
                base,
            };
            fixture.write(65536, 2);
            fixture.save_config(Vec::new());
            fixture
        }

        fn write(&self, offset: u64, byte: u8) {
            let layer = open(&self.store, &self.top, &Options::default()).unwrap();
            layer.write(offset, &[byte; 65536]).unwrap();
            layer.close().unwrap();
        }

        fn read(&self, path: &Path) -> Vec<u8> {
            let layer = open(&self.store, path, &Options::read_only()).unwrap();
            let mut contents = vec![0; SIZE as usize];
            layer.read(0, &mut contents).unwrap();
            contents
        }

        fn save_config(&self, snapshots: Vec<crate::store::Snapshot>) {
            Config {
                version: crate::store::CONFIG_VERSION,
                id: "0".repeat(32),
                name: "m".into(),
                image: "debian-13".into(),
                arch: crate::images::ARCH.into(),
                cpus: 1,
                memory: 1 << 30,
                disk: crate::store::DiskConfig {
                    file: "disk.layer".into(),
                    format: DiskFormat::Layer,
                    size: SIZE,
                },
                mac_address: "02:00:00:00:00:01".into(),
                user: "username".into(),
                created: 0,
                snapshots,
                automatic: None,
            }
            .save(&self.machine)
            .unwrap();
        }

        fn layers(&self) -> Vec<String> {
            let mut names: Vec<String> = std::fs::read_dir(directory(&self.store))
                .unwrap()
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| name.ends_with(".layer") && !name.starts_with('.'))
                .collect();
            names.sort();
            names
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            _ = std::fs::remove_dir_all(self.store.root());
        }
    }

    fn expected(clusters: &[u8]) -> Vec<u8> {
        let mut contents = vec![0; SIZE as usize];
        for (index, &byte) in clusters.iter().enumerate() {
            contents[index * 65536..(index + 1) * 65536].fill(byte);
        }
        contents
    }

    #[test]
    fn a_machine_s_layer_is_sealed_into_the_store_with_a_new_one_on_top() {
        let fixture = Fixture::new("seal");
        let before = fixture.read(&fixture.top);
        let sealed = seal_top(&fixture.store, &fixture.top).unwrap();
        let header = Header::read(&fixture.top).unwrap();
        assert!(!header.sealed);
        assert_eq!(header.parent, Some(sealed));
        assert_eq!(
            Header::read(&path(&fixture.store, &sealed)).unwrap().parent,
            Some(fixture.base)
        );
        assert_eq!(fixture.read(&fixture.top), before);
        assert_eq!(hash_file(&path(&fixture.store, &sealed)).unwrap(), sealed);

        // Sealed in place, as a crash between the steps leaves it: the next open finishes it.
        fixture.write(2 * 65536, 3);
        let layer = open(&fixture.store, &fixture.top, &Options::default()).unwrap();
        layer.seal().unwrap();
        drop(layer);
        recover(&fixture.store, &fixture.top).unwrap();
        let header = Header::read(&fixture.top).unwrap();
        assert!(!header.sealed && header.parent.is_some() && header.parent != Some(sealed));
        assert_eq!(fixture.read(&fixture.top), expected(&[1, 2, 3]));
        assert_eq!(chain(&fixture.store, &fixture.top).unwrap().len(), 3);
        // Recovering a layer that isn't sealed does nothing.
        recover(&fixture.store, &fixture.top).unwrap();
        assert_eq!(chain(&fixture.store, &fixture.top).unwrap().len(), 3);
    }

    #[test]
    fn a_disk_is_flattened_into_one_layer_of_its_own() {
        let fixture = Fixture::new("flatten");
        let mut seen = 0.0;
        let partial = flatten(&fixture.store, &fixture.top, 19, &mut |fraction| seen = fraction).unwrap();
        assert_eq!(seen, 1.0);
        let flat = {
            let _changing = fixture.store.lock().unwrap();
            publish(&fixture.store, &partial).unwrap()
        };
        let header = Header::read(&path(&fixture.store, &flat)).unwrap();
        assert!(header.sealed && header.parent.is_none() && header.capacity == SIZE);
        assert_eq!(fixture.read(&path(&fixture.store, &flat)), expected(&[1, 2]));
        let stats = open(
            &fixture.store,
            &path(&fixture.store, &flat),
            &Options::read_only(),
        )
        .unwrap()
        .stats();
        // The clusters of zeros aren't stored.
        assert_eq!(stats.stored, 2);
    }

    #[test]
    fn only_layers_that_nothing_uses_are_swept() {
        let fixture = Fixture::new("sweep");
        let snapshot = seal_top(&fixture.store, &fixture.top).unwrap();
        let unused = seal_top(&fixture.store, &fixture.top).unwrap();
        // The disk goes back to the snapshot: the layer sealed after it is used by nothing.
        std::fs::remove_file(&fixture.top).unwrap();
        Layer::create(&fixture.top, SIZE, Some(snapshot)).unwrap();
        let mut orphan_folder = directory(&fixture.store);
        orphan_folder.push(".orphan.layer");
        Layer::create(&orphan_folder, SIZE, None).unwrap();
        let orphan_layer = Layer::open(&orphan_folder, &Options::default(), None).unwrap();
        orphan_layer.seal().unwrap();
        drop(orphan_layer);
        let orphan = {
            let _changing = fixture.store.lock().unwrap();
            publish(&fixture.store, &orphan_folder).unwrap()
        };
        assert_eq!(dependents(&fixture.store, &fixture.base), ["m"]);
        assert!(dependents(&fixture.store, &orphan).is_empty());

        sweep(&fixture.store).unwrap();
        let kept = fixture.layers();
        assert_eq!(kept.len(), 2);
        assert!(kept.contains(&format!("{}.layer", to_hex(&fixture.base))));
        assert!(kept.contains(&format!("{}.layer", to_hex(&snapshot))));
        assert!(!path(&fixture.store, &unused).exists() && !path(&fixture.store, &orphan).exists());

        // A snapshot keeps its layer after the disk has moved on from it.
        fixture.save_config(vec![crate::store::Snapshot {
            name: "s".into(),
            layer: to_hex(&snapshot),
            created: 0,
        }]);
        std::fs::remove_file(&fixture.top).unwrap();
        Layer::create(&fixture.top, SIZE, Some(fixture.base)).unwrap();
        sweep(&fixture.store).unwrap();
        assert_eq!(fixture.layers().len(), 2);
        assert_eq!(dependents(&fixture.store, &snapshot), ["m"]);
        // Forgotten, it goes.
        fixture.save_config(Vec::new());
        sweep(&fixture.store).unwrap();
        assert_eq!(fixture.layers(), [format!("{}.layer", to_hex(&fixture.base))]);
        // A note that can't be read stops the sweep: it may name a layer.
        let notes = fixture.store.root().join("images");
        std::fs::create_dir_all(&notes).unwrap();
        std::fs::write(notes.join("broken.json"), "{").unwrap();
        std::fs::remove_file(&fixture.top).unwrap();
        Layer::create(&fixture.top, SIZE, None).unwrap();
        sweep(&fixture.store).unwrap();
        assert_eq!(fixture.layers().len(), 1);
    }
}
