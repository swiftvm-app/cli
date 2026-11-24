//! Images made from machines: those saved by name, and the automatic ones that the first machine
//! of a catalog image and its setup scripts leaves behind. Each is a note that names a sealed
//! layer.
//!
//! ```text
//! <store>/images/<name>.json    an image
//! ```
//!
//! Notes are added, renamed and removed with the store's lock held, which a sweep of unused
//! layers holds too.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::scripts::Script;
use crate::store::{Store, check_name};
use crate::{Context, Error, ErrorCode, Result, images, layers};

/// What a machine can be created from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ImageKind {
    /// Downloaded from its publisher.
    Catalog,
    /// Made from a machine, by name.
    Saved,
    /// Made from the first machine of a catalog image and its setup scripts.
    Automatic,
}

/// An image made from a machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SavedImage {
    pub name: String,
    pub kind: ImageKind,
    /// The SHA-256 of its sealed layer, in hexadecimal.
    pub layer: String,
    /// `arm64` or `x86-64`.
    pub arch: String,
    /// The account that the machine was created with, which machines made from it have.
    pub user: String,
    /// The size of the disk it was made of, in bytes: the least that a machine made from it gets.
    pub disk_size: u64,
    /// The image of the catalog that it goes back to.
    pub from: String,
    /// The setup scripts that an automatic image ran, by their names.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scripts: Vec<String>,
    /// What an automatic image is found by.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Seconds since 1970.
    pub created: u64,
}

impl SavedImage {
    pub fn hash(&self) -> Result<[u8; 32]> {
        disk::layer::from_hex(&self.layer).ok_or_else(|| {
            Error::new(
                ErrorCode::Io,
                format!("The image \"{}\" names no layer it can be found by.", self.name),
            )
        })
    }

    /// What it holds, as the list of images shows it.
    pub fn description(&self) -> String {
        match self.kind {
            ImageKind::Automatic => format!("{} + {} ({})", self.from, self.scripts.join(", "), self.user),
            _ => format!("From {} ({})", self.from, self.user),
        }
    }
}

fn directory(store: &Store) -> PathBuf {
    store.root().join("images")
}

fn path(store: &Store, name: &str) -> PathBuf {
    directory(store).join(format!("{name}.json"))
}

/// What is wrong with `name` as the name of a new image.
pub(crate) fn check_new_name(store: &Store, name: &str) -> Result<()> {
    check_name(name).map_err(|_| {
        Error::new(
            ErrorCode::InvalidArgument,
            format!(
                "\"{name}\" can't be an image's name. Use 1 to 63 lowercase letters, digits and \"-\", \
                 with a letter or a digit first and last."
            ),
        )
    })?;
    if images::find(name).is_ok() || name.starts_with(AUTOMATIC_PREFIX) {
        return Err(Error::new(
            ErrorCode::InvalidArgument,
            format!("\"{name}\" is the name of an image of the catalog, or kept for automatic ones."),
        ));
    }
    if path(store, name).exists() {
        return Err(Error::new(
            ErrorCode::AlreadyExists,
            format!("An image called \"{name}\" exists already."),
        ));
    }
    Ok(())
}

/// The image called `name`, if there is one.
pub(crate) fn find(store: &Store, name: &str) -> Result<Option<SavedImage>> {
    if check_name(name).is_err() {
        return Ok(None);
    }
    read(&path(store, name))
}

fn read(path: &Path) -> Result<Option<SavedImage>> {
    match std::fs::read(path) {
        Ok(text) => serde_json::from_slice(&text).map(Some).map_err(|error| {
            Error::new(
                ErrorCode::Io,
                format!("{} can't be used: {error}.", path.display()),
            )
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).at("read", path),
    }
}

/// Every image made from a machine, sorted by name. One whose note can't be read is left out.
pub(crate) fn all(store: &Store) -> Vec<SavedImage> {
    let mut found: Vec<SavedImage> = std::fs::read_dir(directory(store))
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "json"))
        .filter_map(|path| read(&path).ok().flatten())
        .collect();
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found
}

/// Writes the image's note, whole or not at all. Called with the store's lock held.
pub(crate) fn write(store: &Store, image: &SavedImage) -> Result<()> {
    let folder = directory(store);
    std::fs::create_dir_all(&folder).at("create", &folder)?;
    let destination = path(store, &image.name);
    let partial = folder.join(format!(".{}.json.new", image.name));
    let mut text = serde_json::to_vec_pretty(image).expect("plain data");
    text.push(b'\n');
    // On the disk before it is renamed: a machine may be deleted once the image is noted.
    std::fs::File::create(&partial)
        .and_then(|mut file| {
            use std::io::Write;
            file.write_all(&text)?;
            file.sync_all()
        })
        .at("write", &partial)?;
    std::fs::rename(&partial, &destination).at("write", &destination)?;
    layers::sync_folder(&folder)
}

/// Removes the image's note. Called with the store's lock held.
pub(crate) fn remove(store: &Store, name: &str) -> Result<()> {
    let note = path(store, name);
    std::fs::remove_file(&note).at("delete", &note)?;
    layers::sync_folder(&directory(store))
}

/// Gives the image another name. Called with the store's lock held.
pub(crate) fn rename(store: &Store, image: &SavedImage, name: &str) -> Result<()> {
    let renamed = SavedImage {
        name: name.to_string(),
        ..image.clone()
    };
    write(store, &renamed)?;
    remove(store, &image.name)
}

/// What the names of automatic images start with.
pub(crate) const AUTOMATIC_PREFIX: &str = "auto-";

/// What the automatic image of the catalog image whose layer is `base`, set up by `scripts` for
/// the account `user`, is found by. A machine's password isn't part of it: each machine sets its
/// own when it first starts.
pub(crate) fn automatic_key(base: &[u8; 32], scripts: &[Script], user: &str) -> String {
    let mut context = graviola::hashing::sha2::Sha256Context::new();
    let mut add = |part: &[u8]| {
        // Each part with its length first, so that no two lists of parts read the same.
        context.update(&(part.len() as u64).to_le_bytes());
        context.update(part);
    };
    add(b"automatic image 1");
    add(base);
    add(images::ARCH.as_bytes());
    add(user.as_bytes());
    for script in scripts {
        add(&script.source);
    }
    disk::layer::to_hex(&context.finish())
}

/// The name of the automatic image that `key` finds.
pub(crate) fn automatic_name(key: &str) -> String {
    format!("{AUTOMATIC_PREFIX}{}", &key[..8])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(test: &str) -> Store {
        let root = std::env::temp_dir().join(format!("saved-test-{test}-{}", crate::random_hex(4).unwrap()));
        Store::new(root)
    }

    fn image(name: &str) -> SavedImage {
        SavedImage {
            name: name.into(),
            kind: ImageKind::Saved,
            layer: "00".repeat(32),
            arch: images::ARCH.into(),
            user: "username".into(),
            disk_size: 1 << 30,
            from: "debian-13".into(),
            scripts: Vec::new(),
            key: None,
            created: 1,
        }
    }

    #[test]
    fn notes_are_written_found_renamed_and_removed() {
        let store = store("notes");
        assert_eq!(find(&store, "dev").unwrap(), None);
        write(&store, &image("dev")).unwrap();
        assert_eq!(find(&store, "dev").unwrap(), Some(image("dev")));
        assert_eq!(
            check_new_name(&store, "dev").unwrap_err().code,
            ErrorCode::AlreadyExists
        );
        assert_eq!(
            check_new_name(&store, "debian-13").unwrap_err().code,
            ErrorCode::InvalidArgument
        );
        assert_eq!(
            check_new_name(&store, "auto-1234").unwrap_err().code,
            ErrorCode::InvalidArgument
        );
        assert_eq!(
            check_new_name(&store, "../x").unwrap_err().code,
            ErrorCode::InvalidArgument
        );

        rename(&store, &image("dev"), "web").unwrap();
        assert_eq!(find(&store, "dev").unwrap(), None);
        assert_eq!(find(&store, "web").unwrap().unwrap().name, "web");
        write(&store, &image("app")).unwrap();
        let names: Vec<String> = all(&store).into_iter().map(|image| image.name).collect();
        assert_eq!(names, ["app", "web"]);
        remove(&store, "web").unwrap();
        assert_eq!(find(&store, "web").unwrap(), None);
        std::fs::remove_dir_all(store.root()).unwrap();
    }

    #[test]
    fn an_automatic_image_is_found_by_its_base_scripts_and_account() {
        let docker = Script::find("docker").unwrap();
        let updates = Script::find("updates").unwrap();
        let base = [1; 32];
        let key = automatic_key(&base, std::slice::from_ref(&docker), "alice");
        assert_eq!(key, automatic_key(&base, std::slice::from_ref(&docker), "alice"));
        assert_ne!(
            key,
            automatic_key(&[2; 32], std::slice::from_ref(&docker), "alice")
        );
        assert_ne!(key, automatic_key(&base, std::slice::from_ref(&docker), "bob"));
        assert_ne!(
            key,
            automatic_key(&base, &[docker.clone(), updates.clone()], "alice")
        );
        assert_ne!(
            automatic_key(&base, &[docker.clone(), updates.clone()], "alice"),
            automatic_key(&base, &[updates, docker], "alice")
        );
        assert!(automatic_name(&key).starts_with("auto-") && automatic_name(&key).len() == 13);
    }
}
