//! Folders of this computer that a guest sees in `/mnt/shared`, read and written in place.

use std::path::{Path, PathBuf};

use proto::agent::SHARED;
use serde::{Deserialize, Serialize};

use crate::store::Share;
use crate::{Context, Error, ErrorCode, Result};

/// The longest a share's name is: it is the tag of a virtio-fs device on Linux, which has room
/// for 36 bytes.
const LONGEST_NAME: usize = 36;

/// A share as the command shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareInfo {
    /// The folder's name in the guest's `/mnt/shared`.
    pub name: String,
    /// The folder on this computer.
    pub path: PathBuf,
    pub read_only: bool,
    /// Why the guest doesn't see it: the folder isn't there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

impl ShareInfo {
    pub(crate) fn of(share: &Share) -> Self {
        Self {
            name: share.name.clone(),
            path: share.path.clone(),
            read_only: share.read_only,
            problem: (!share.path.is_dir()).then(|| {
                format!(
                    "{} isn't a folder on this computer, so the guest doesn't see it.",
                    share.path.display()
                )
            }),
        }
    }
}

/// Where the guest sees a share called `name`.
pub fn guest_path(name: &str) -> String {
    format!("{SHARED}/{name}")
}

/// What is wrong with `name` as a share's name.
pub(crate) fn check_name(name: &str) -> Result<()> {
    let allowed = |byte: u8| byte.is_ascii_alphanumeric() || b"._-".contains(&byte);
    if (1..=LONGEST_NAME).contains(&name.len()) && name.bytes().all(allowed) && !name.starts_with('.') {
        return Ok(());
    }
    Err(Error::new(
        ErrorCode::InvalidArgument,
        format!(
            "\"{name}\" can't be a share's name. Use 1 to {LONGEST_NAME} letters, digits, \".\", \"_\" and \"-\", not starting with \".\"."
        ),
    ))
}

/// The name a folder is shared under unless another is given: its last part, with what a name
/// can't hold turned into `-`.
fn default_name(folder: &Path) -> String {
    let last = folder
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default();
    let name: String = last
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || "._-".contains(character) {
                character
            } else {
                '-'
            }
        })
        .collect();
    let name = name.trim_start_matches('.');
    let name: String = name.chars().take(LONGEST_NAME).collect();
    if name.is_empty() { "shared".into() } else { name }
}

/// The share of the folder at `folder`, which has to be there, called `name` or after the folder.
pub(crate) fn new(folder: &Path, name: Option<&str>, read_only: bool) -> Result<Share> {
    let path = match folder.canonicalize() {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(Error::new(
                ErrorCode::NotFound,
                format!("There is no folder at {}.", folder.display()),
            ));
        }
        Err(error) => return Err(error).at("find", folder),
    };
    if !path.is_dir() {
        return Err(Error::new(
            ErrorCode::InvalidArgument,
            format!("{} isn't a folder. Only folders can be shared.", folder.display()),
        ));
    }
    let name = match name {
        Some(name) => name.to_string(),
        None => default_name(&path),
    };
    check_name(&name)?;
    Ok(Share {
        name,
        path,
        read_only,
    })
}

/// Adds `share` to `shares`, whose names are unique.
pub(crate) fn add(shares: &mut Vec<Share>, share: Share, machine: &str) -> Result<()> {
    if shares.iter().any(|taken| taken.name == share.name) {
        return Err(Error::new(
            ErrorCode::AlreadyExists,
            format!(
                "\"{machine}\" shares a folder called \"{}\" already. Give this one another name with --name.",
                share.name
            ),
        ));
    }
    shares.push(share);
    Ok(())
}

/// The shares whose folders are there, which are those the guest gets.
pub(crate) fn available(shares: &[Share]) -> Vec<Share> {
    shares
        .iter()
        .filter(|share| share.path.is_dir())
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_share_is_named_after_its_folder() {
        assert_eq!(default_name(Path::new("/home/a/Projects")), "Projects");
        assert_eq!(default_name(Path::new("/home/a/My Docs (old)")), "My-Docs--old-");
        assert_eq!(default_name(Path::new("/home/a/.config")), "config");
        assert_eq!(default_name(Path::new("/home/a/héllo")), "h-llo");
        assert_eq!(default_name(Path::new("/")), "shared");
        assert_eq!(default_name(Path::new(&format!("/{}", "a".repeat(50)))).len(), 36);
    }

    #[test]
    fn names_are_checked() {
        for name in ["Projects", "a", "web.site_2-x", &"a".repeat(36)] {
            assert!(check_name(name).is_ok(), "{name}");
        }
        for name in ["", ".hidden", "a/b", "a b", "..", &"a".repeat(37), "é"] {
            assert!(check_name(name).is_err(), "{name}");
        }
    }

    #[test]
    fn a_folder_is_shared_once_under_a_name() {
        let folder = std::env::temp_dir().join(format!("share-test-{}", std::process::id()));
        std::fs::create_dir_all(folder.join("Projects")).unwrap();
        std::fs::write(folder.join("file"), "").unwrap();
        let share = new(&folder.join("Projects/../Projects"), None, true).unwrap();
        assert_eq!(share.name, "Projects");
        assert_eq!(share.path, folder.join("Projects").canonicalize().unwrap());
        assert!(share.read_only);
        assert_eq!(
            new(&folder.join("file"), None, false).unwrap_err().code,
            ErrorCode::InvalidArgument
        );
        assert_eq!(
            new(&folder.join("none"), None, false).unwrap_err().code,
            ErrorCode::NotFound
        );
        assert_eq!(
            new(&folder, Some("a b"), false).unwrap_err().code,
            ErrorCode::InvalidArgument
        );

        let mut shares = Vec::new();
        add(&mut shares, share.clone(), "m").unwrap();
        assert_eq!(
            add(&mut shares, share.clone(), "m").unwrap_err().code,
            ErrorCode::AlreadyExists
        );
        let gone = Share {
            name: "gone".into(),
            path: folder.join("none"),
            read_only: false,
        };
        add(&mut shares, gone.clone(), "m").unwrap();
        assert_eq!(available(&shares), [share]);
        assert!(ShareInfo::of(&gone).problem.is_some());
        std::fs::remove_dir_all(&folder).unwrap();
    }
}
