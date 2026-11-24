//! What `cp` carries: one file or folder as a tar stream. Both ends write and read it with this
//! module, so that neither the guest's `tar` nor the Mac's has a say in what is in it.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Component, Path};

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

/// The name that what is at `path` goes by in the stream.
fn top_name(path: &Path) -> io::Result<OsString> {
    if let Some(name) = path.file_name() {
        return Ok(name.to_owned());
    }
    // `.` or `..`: the folder's own name.
    fs::canonicalize(path)?
        .file_name()
        .map(ToOwned::to_owned)
        .ok_or_else(|| invalid("the root folder can't be copied as a whole"))
}

/// Writes what is at `source` to `output`, under its own name. A folder goes with everything in
/// it. Symbolic links are copied as links, not followed.
pub fn pack(source: &Path, output: impl Write) -> io::Result<()> {
    let name = top_name(source)?;
    let metadata = fs::symlink_metadata(source)?;
    let mut builder = tar::Builder::new(output);
    builder.follow_symlinks(false);
    if metadata.is_dir() {
        builder.append_dir_all(&name, source)?;
    } else {
        builder.append_path_with_name(source, &name)?;
    }
    builder.into_inner()?.flush()
}

/// Takes the name that `path` starts with as the stream's one name, or checks that it is that.
fn under_one_name(top: &mut Option<OsString>, path: &Path) -> io::Result<()> {
    let outside = || {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "the stream holds {}, which is outside what is copied",
                path.display()
            ),
        )
    };
    let Some(Component::Normal(first)) = path.components().next() else {
        return Err(outside());
    };
    match top {
        Some(top) if top != first => Err(outside()),
        Some(_) => Ok(()),
        None => {
            *top = Some(first.to_owned());
            Ok(())
        }
    }
}

/// Unpacks the stream into the folder `into`, as the tar crate's own `unpack` does, and returns
/// the one name that everything in it is under. Whoever wrote the stream may be hostile, as a
/// guest can be: anything else that it holds fails the copy, rather than landing next to it.
/// With `expected`, that name has to be it.
fn unpack_into<R: Read>(
    archive: &mut tar::Archive<R>,
    into: &Path,
    expected: Option<&OsStr>,
) -> io::Result<OsString> {
    let into = into.canonicalize()?;
    let mut top = expected.map(ToOwned::to_owned);
    // Folders last, the deepest first, so that one that can't be written to is made so only once
    // what is in it is there.
    let mut folders = Vec::new();
    for entry in archive.entries()? {
        let mut entry = entry?;
        under_one_name(&mut top, &entry.path()?)?;
        let kind = entry.header().entry_type();
        if kind.is_hard_link()
            && let Some(target) = entry.link_name()?
        {
            under_one_name(&mut top, &target)?;
        }
        if kind.is_dir() {
            folders.push(entry);
        } else {
            entry.unpack_in(&into)?;
        }
    }
    folders.sort_by(|a, b| b.path_bytes().cmp(&a.path_bytes()));
    for mut folder in folders {
        folder.unpack_in(&into)?;
    }
    top.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "the stream holds nothing"))
}

/// Puts what [`pack`] wrote at `destination`, as `cp -R` does: into it, under its own name, when
/// it is a folder, and otherwise in its place, under its name.
///
/// `expected` is the name that the stream's writer was asked for, which is all that it may hold.
/// Without it, any one name is taken: the writer is trusted to name what it was asked for.
pub fn unpack(input: impl Read, destination: &Path, expected: Option<&OsStr>) -> io::Result<()> {
    let mut archive = tar::Archive::new(input);
    if destination.is_dir() {
        return unpack_into(&mut archive, destination, expected).map(|_| ());
    }
    let name = destination
        .file_name()
        .ok_or_else(|| invalid("the destination has no name"))?;
    let parent = match destination.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    // Unpacked next to its place, under a name of its own, and then renamed into it: the stream
    // calls what is in it by the source's name.
    let mut staging_name = OsString::from(".");
    staging_name.push(name);
    staging_name.push(format!(".copying-{}", std::process::id()));
    let staging = parent.join(staging_name);
    fs::create_dir(&staging)?;
    let unpacked = unpack_into(&mut archive, &staging, expected)
        .and_then(|top| fs::rename(staging.join(top), destination));
    _ = fs::remove_dir_all(&staging);
    unpacked
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    fn folder(test: &str) -> PathBuf {
        let folder = std::env::temp_dir().join(format!("copy-test-{test}-{}", std::process::id()));
        _ = fs::remove_dir_all(&folder);
        fs::create_dir_all(&folder).unwrap();
        folder
    }

    fn copied(source: &Path, destination: &Path) {
        let mut stream = Vec::new();
        pack(source, &mut stream).unwrap();
        unpack(stream.as_slice(), destination, None).unwrap();
    }

    #[test]
    fn a_folder_goes_into_a_folder_or_takes_a_new_name() {
        let root = folder("folder");
        let source = root.join("source");
        fs::create_dir_all(source.join("inner")).unwrap();
        fs::write(source.join("inner/file"), "contents").unwrap();
        fs::set_permissions(source.join("inner/file"), fs::Permissions::from_mode(0o751)).unwrap();
        std::os::unix::fs::symlink("inner/file", source.join("link")).unwrap();

        let into = root.join("into");
        fs::create_dir(&into).unwrap();
        copied(&source, &into);
        assert_eq!(
            fs::read_to_string(into.join("source/inner/file")).unwrap(),
            "contents"
        );
        let mode = fs::metadata(into.join("source/inner/file"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o751);
        assert_eq!(
            fs::read_link(into.join("source/link")).unwrap(),
            Path::new("inner/file")
        );

        let renamed = root.join("renamed");
        copied(&source.join("."), &renamed);
        assert_eq!(
            fs::read_to_string(renamed.join("inner/file")).unwrap(),
            "contents"
        );
        // Nothing of the staging is left.
        let left: Vec<_> = fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(left.len(), 3, "{left:?}");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_file_replaces_a_file() {
        let root = folder("file");
        fs::write(root.join("new"), "new").unwrap();
        fs::write(root.join("old"), "old").unwrap();
        copied(&root.join("new"), &root.join("old"));
        assert_eq!(fs::read_to_string(root.join("old")).unwrap(), "new");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_stream_cut_short_leaves_nothing() {
        let root = folder("cut");
        fs::write(root.join("file"), vec![7; 100_000]).unwrap();
        let mut stream = Vec::new();
        pack(&root.join("file"), &mut stream).unwrap();
        stream.truncate(20_000);
        assert!(unpack(stream.as_slice(), &root.join("copy"), None).is_err());
        let left: Vec<_> = fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(left, ["file"]);
        fs::remove_dir_all(&root).unwrap();
    }

    /// A stream from a guest that holds more than was asked for.
    fn hostile(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (path, contents) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, path, *contents).unwrap();
        }
        builder.into_inner().unwrap()
    }

    #[test]
    fn a_stream_writes_nothing_but_what_was_asked_for() {
        let root = folder("hostile");
        let asked = Some(OsStr::new("report.txt"));
        // Something next to the copy, in the folder it goes to.
        let stream = hostile(&[("report.txt", b"ok"), (".zshrc", b"evil")]);
        assert!(unpack(stream.as_slice(), &root, asked).is_err());
        assert!(!root.join(".zshrc").exists());
        // What was asked for, which came first, is there.
        fs::remove_file(root.join("report.txt")).unwrap();
        // Something else in the place of what was asked for.
        let stream = hostile(&[(".zshrc", b"evil")]);
        assert!(unpack(stream.as_slice(), &root, asked).is_err());
        assert!(!root.join(".zshrc").exists());
        // A hard link to something next to the copy.
        fs::write(root.join("secret"), "secret").unwrap();
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Link);
        header.set_size(0);
        header.set_cksum();
        builder.append_link(&mut header, "report.txt", "secret").unwrap();
        let stream = builder.into_inner().unwrap();
        assert!(unpack(stream.as_slice(), &root, asked).is_err());
        assert!(!root.join("report.txt").exists());

        let stream = hostile(&[("report.txt", b"ok")]);
        unpack(stream.as_slice(), &root, asked).unwrap();
        assert_eq!(fs::read_to_string(root.join("report.txt")).unwrap(), "ok");
        fs::remove_dir_all(&root).unwrap();
    }
}
