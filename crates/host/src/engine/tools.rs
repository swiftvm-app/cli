//! The programs that run machines on Linux: cloud-hypervisor and its firmware, of versions that
//! are known to work, and passt.
//!
//! Each is taken from next to this program first, where an app that bundles the library puts
//! them. Otherwise cloud-hypervisor and the firmware are downloaded into the cache and checked
//! against the checksums below, and passt is looked for where the system keeps programs:
//! distributions build it, and no build of it is published for every processor.

use std::path::{Path, PathBuf};

use crate::download::{self, Digest, Wanted};
use crate::images::Algorithm;
use crate::{Context, Error, ErrorCode, Result};

/// A file of a release, as it was published.
struct Pinned {
    url: &'static str,
    /// Its name next to this program, and in the cache.
    file_name: &'static str,
    sha256: &'static str,
}

impl Pinned {
    fn wanted(&self) -> Wanted<'static> {
        Wanted {
            url: self.url,
            folder: "engine",
            file_name: self.file_name,
            algorithm: Algorithm::Sha256,
            digest: Digest::Known(self.sha256),
        }
    }
}

const HYPERVISOR: Pinned = if cfg!(target_arch = "aarch64") {
    Pinned {
        url: "https://github.com/cloud-hypervisor/cloud-hypervisor/releases/download/v55.0/cloud-hypervisor-static-aarch64",
        file_name: "cloud-hypervisor",
        sha256: "446de7552dacc83bc86d1137e7ffa7c2d204cdda4850eac4f2d2666d63c0f697",
    }
} else {
    Pinned {
        url: "https://github.com/cloud-hypervisor/cloud-hypervisor/releases/download/v55.0/cloud-hypervisor-static",
        file_name: "cloud-hypervisor",
        sha256: "ce7133bf535ed5a4e2a63ea1f26025445a5b1241f73ddfb81ff2fbcb2e803a59",
    }
};

/// UEFI for cloud-hypervisor, from its own build of EDK II.
const FIRMWARE: Pinned = if cfg!(target_arch = "aarch64") {
    Pinned {
        url: "https://github.com/cloud-hypervisor/edk2/releases/download/ch-97eeb7b09/CLOUDHV_EFI.fd",
        file_name: "CLOUDHV_EFI.fd",
        sha256: "13cf4eddc44f3fac4e223b54bcf958de99539297fee8b61cca2605ab5b5009fb",
    }
} else {
    Pinned {
        url: "https://github.com/cloud-hypervisor/edk2/releases/download/ch-97eeb7b09/CLOUDHV.fd",
        file_name: "CLOUDHV.fd",
        sha256: "dc2fc8f0e43b96712d9fccc52a3a590769606412b3e1bc911d217addd3bef624",
    }
};

const PASST: &str = "passt";
const VIRTIOFSD: &str = "virtiofsd";
/// Where distributions put virtiofsd, which isn't on the `PATH`.
const VIRTIOFSD_PLACES: [&str; 3] = [
    "/usr/libexec/virtiofsd",
    "/usr/lib/qemu/virtiofsd",
    "/usr/lib/virtiofsd",
];

/// Where the engine's programs are.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) struct Tools {
    pub hypervisor: PathBuf,
    pub firmware: PathBuf,
    pub passt: PathBuf,
}

/// A file called `name` next to this program.
fn beside_program(name: &str) -> Option<PathBuf> {
    let program = std::env::current_exe().ok()?;
    let found = program.parent()?.join(name);
    found.is_file().then_some(found)
}

/// Has cloud-hypervisor and the firmware in the cache, unless they are next to this program.
pub(crate) fn fetch(cache: &Path, progress: &mut dyn FnMut(u64, Option<u64>)) -> Result<()> {
    for pinned in [&HYPERVISOR, &FIRMWARE] {
        if beside_program(pinned.file_name).is_none() {
            let file = download::fetch_file(cache, &pinned.wanted(), progress)?;
            if pinned.file_name == HYPERVISOR.file_name {
                make_runnable(&file)?;
            }
        }
    }
    Ok(())
}

fn make_runnable(file: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o755)).at("make runnable", file)
}

fn pinned(cache: &Path, pinned: &Pinned) -> Result<PathBuf> {
    beside_program(pinned.file_name)
        .or_else(|| download::cached_file(cache, &pinned.wanted()))
        .ok_or_else(|| {
            Error::new(
                ErrorCode::NotFound,
                format!(
                    "{} isn't in the cache. Starting the machine again downloads it.",
                    pinned.file_name
                ),
            )
        })
}

/// Finds the engine's programs, without downloading anything.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn find(cache: &Path) -> Result<Tools> {
    Ok(Tools {
        hypervisor: pinned(cache, &HYPERVISOR)?,
        firmware: pinned(cache, &FIRMWARE)?,
        passt: passt()?,
    })
}

/// passt next to this program, or on the `PATH`. It has to be one that serves vhost-user, which
/// came in 2025: Debian 13, Fedora 42 and Ubuntu 25.04 have it, Ubuntu 24.04 doesn't.
fn passt() -> Result<PathBuf> {
    let found = beside_program(PASST).or_else(|| {
        std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|directory| directory.join(PASST))
            .find(|candidate| candidate.is_file())
    });
    let Some(passt) = found else {
        return Err(Error::new(
            ErrorCode::NotFound,
            "passt isn't installed, and machines need it for their network. Install the passt package, or put passt next to this program.",
        ));
    };
    let help = std::process::Command::new(&passt)
        .arg("--help")
        .stdin(std::process::Stdio::null())
        .output()
        .at("run", &passt)?;
    let text = [help.stdout, help.stderr].concat();
    if !String::from_utf8_lossy(&text).contains("--vhost-user") {
        return Err(Error::new(
            ErrorCode::Unsupported,
            format!(
                "{} is too old: machines need a passt that has --vhost-user, from 2025 or later. Put a newer one next to this program.",
                passt.display()
            ),
        ));
    }
    Ok(passt)
}

/// virtiofsd, which serves a shared folder to a machine: next to this program, where the
/// release puts one, or the system's.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn virtiofsd() -> Result<PathBuf> {
    beside_program(VIRTIOFSD)
        .or_else(|| {
            VIRTIOFSD_PLACES
                .iter()
                .map(PathBuf::from)
                .chain(
                    std::env::var_os("PATH")
                        .map(|path| std::env::split_paths(&path).map(|directory| directory.join(VIRTIOFSD)).collect::<Vec<_>>())
                        .unwrap_or_default(),
                )
                .find(|candidate| candidate.is_file())
        })
        .ok_or_else(|| {
            Error::new(
                ErrorCode::NotFound,
                "virtiofsd isn't installed, and shared folders need it. Install the virtiofsd package, or put virtiofsd next to this program.",
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pinned_files_are_for_this_processor() {
        for pinned in [&HYPERVISOR, &FIRMWARE] {
            assert!(pinned.url.starts_with("https://github.com/cloud-hypervisor/"));
            assert_eq!(pinned.sha256.len(), 64);
            assert!(pinned.sha256.bytes().all(|byte| byte.is_ascii_hexdigit()));
        }
        assert_eq!(
            HYPERVISOR.url.ends_with("-aarch64"),
            cfg!(target_arch = "aarch64")
        );
        assert_eq!(
            FIRMWARE.file_name == "CLOUDHV_EFI.fd",
            cfg!(target_arch = "aarch64")
        );
    }
}
