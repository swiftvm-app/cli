//! The catalog: the systems a machine can be created with, and where their images come from.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{Error, ErrorCode, Result};

/// This computer's kind of processor, as the catalog and a machine's settings call it. Guests
/// are of the same kind as their host.
pub(crate) const ARCH: &str = if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    "x86-64"
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Algorithm {
    Sha256,
    Sha512,
}

/// An image of the catalog, for this computer's kind of processor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    /// What `create` is given: `debian-13`.
    pub id: String,
    pub name: String,
    pub url: String,
    /// A listing of checksums by its publisher, in which the image's file is looked up.
    pub checksums: String,
    pub algorithm: Algorithm,
}

impl Image {
    /// The name of the image's file, at its publisher's and in the cache.
    pub fn file_name(&self) -> &str {
        self.url.rsplit('/').next().unwrap_or(&self.url)
    }
}

#[derive(Deserialize)]
struct Catalog {
    images: Vec<Entry>,
}

#[derive(Deserialize)]
struct Entry {
    id: String,
    name: String,
    files: BTreeMap<String, File>,
}

#[derive(Deserialize)]
struct File {
    url: String,
    checksums: String,
    algorithm: Algorithm,
}

/// The images that this computer can run, in the catalog's order.
pub(crate) fn catalog() -> Vec<Image> {
    let catalog: Catalog =
        serde_json::from_str(include_str!("images.json")).expect("the catalog is valid JSON");
    catalog
        .images
        .into_iter()
        .filter_map(|mut entry| {
            let file = entry.files.remove(ARCH)?;
            Some(Image {
                id: entry.id,
                name: entry.name,
                url: file.url,
                checksums: file.checksums,
                algorithm: file.algorithm,
            })
        })
        .collect()
}

pub(crate) fn find(id: &str) -> Result<Image> {
    let catalog = catalog();
    let known: Vec<&str> = catalog.iter().map(|image| image.id.as_str()).collect();
    catalog
        .iter()
        .find(|image| image.id == id)
        .cloned()
        .ok_or_else(|| {
            Error::new(
                ErrorCode::NotFound,
                format!(
                    "There is no image called \"{id}\". The images are: {}.",
                    known.join(", ")
                ),
            )
        })
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_catalog_is_read() {
        let catalog = super::catalog();
        assert!(catalog.iter().any(|image| image.id == "debian-13"));
        assert!(
            catalog
                .iter()
                .all(|image| image.url.starts_with("https://") && !image.file_name().is_empty())
        );
    }
}
