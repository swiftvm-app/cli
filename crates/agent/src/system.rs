//! What the guest is and how it is doing, and its clock.

use std::ffi::CStr;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::Path;
use std::process::Command;

use proto::agent::{Address, GuestInfo, Setup, SetupState};
use proto::{Error, ErrorCode, NAME};

/// The longest a name or a step from a setup script is kept at.
const LONGEST_TEXT: usize = 120;

/// The guest's clock, in nanoseconds since 1970.
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos() as u64)
}

#[cfg(target_os = "linux")]
pub fn set_clock(nanoseconds: u64) -> Result<(), Error> {
    let time = libc::timespec {
        tv_sec: (nanoseconds / 1_000_000_000) as _,
        tv_nsec: (nanoseconds % 1_000_000_000) as _,
    };
    // Safety: a valid time that lives through the call.
    if unsafe { libc::clock_settime(libc::CLOCK_REALTIME, &time) } != 0 {
        let error = std::io::Error::last_os_error();
        return Err(Error::new(
            ErrorCode::Io,
            format!("Couldn't set the clock: {error}."),
        ));
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn set_clock(_nanoseconds: u64) -> Result<(), Error> {
    Err(Error::new(
        ErrorCode::Unsupported,
        "The agent sets the clock in Linux only.",
    ))
}

pub fn info() -> GuestInfo {
    // Safety: an all-zero utsname is a valid value for uname to fill in.
    let mut names: libc::utsname = unsafe { std::mem::zeroed() };
    // Safety: a valid pointer to one.
    unsafe { libc::uname(&mut names) };
    // Safety: uname fills each in with a string that ends in a zero.
    let text =
        |field: &[libc::c_char]| unsafe { CStr::from_ptr(field.as_ptr()).to_string_lossy().into_owned() };
    GuestInfo {
        agent: env!("CARGO_PKG_VERSION").to_string(),
        host_name: text(&names.nodename),
        system: system_name(),
        kernel: text(&names.release),
        arch: text(&names.machine),
        addresses: addresses(),
        booted: booted(),
        setup: setup(Path::new(&format!("/var/lib/{NAME}/setup"))),
    }
}

/// `PRETTY_NAME` of os-release.
fn system_name() -> Option<String> {
    let text = std::fs::read_to_string("/etc/os-release")
        .or_else(|_| std::fs::read_to_string("/usr/lib/os-release"))
        .ok()?;
    text.lines().find_map(|line| {
        let value = line.strip_prefix("PRETTY_NAME=")?;
        let value = value.trim().trim_matches('"').trim_matches('\'');
        (!value.is_empty()).then(|| value.to_string())
    })
}

/// Whether the system has finished starting: systemd has run everything it was to start, the
/// first-start setup included. A system without systemd has nothing that says.
fn booted() -> bool {
    match Command::new("systemctl").arg("is-system-running").output() {
        Ok(output) => matches!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "running" | "degraded"
        ),
        Err(_) => true,
    }
}

/// An address that is of use beyond its own link.
fn reachable(address: &IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => !address.is_loopback() && !address.is_link_local(),
        IpAddr::V6(address) => !address.is_loopback() && address.segments()[0] & 0xffc0 != 0xfe80,
    }
}

fn addresses() -> Vec<Address> {
    let mut found = Vec::new();
    let mut list = std::ptr::null_mut();
    // Safety: getifaddrs fills in a list that is freed below, and nothing else holds it.
    unsafe {
        if libc::getifaddrs(&mut list) != 0 {
            return found;
        }
        let mut entry = list;
        while let Some(interface) = entry.as_ref() {
            entry = interface.ifa_next;
            let Some(address) = interface.ifa_addr.as_ref() else {
                continue;
            };
            let address = match address.sa_family as libc::c_int {
                libc::AF_INET => {
                    let address = &*(address as *const libc::sockaddr).cast::<libc::sockaddr_in>();
                    IpAddr::V4(Ipv4Addr::from(u32::from_be(address.sin_addr.s_addr)))
                }
                libc::AF_INET6 => {
                    let address = &*(address as *const libc::sockaddr).cast::<libc::sockaddr_in6>();
                    IpAddr::V6(Ipv6Addr::from(address.sin6_addr.s6_addr))
                }
                _ => continue,
            };
            if reachable(&address) {
                found.push(Address {
                    interface: CStr::from_ptr(interface.ifa_name).to_string_lossy().into_owned(),
                    address: address.to_string(),
                });
            }
        }
        libc::freeifaddrs(list);
    }
    found
}

/// `text` as one short line, without the characters that could make it show as something else.
fn shown(text: &str) -> Option<String> {
    let line: String = text
        .chars()
        .filter(|character| !character.is_control() && !matches!(character, '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'))
        .take(LONGEST_TEXT)
        .collect();
    let line = line.trim();
    (!line.is_empty()).then(|| line.to_string())
}

/// How the setup scripts are doing, as their runner notes it in `directory`.
fn setup(directory: &Path) -> Option<Setup> {
    let status = std::fs::read_to_string(directory.join("status")).ok()?;
    let mut state = SetupState::Unknown;
    let mut name = None;
    for line in status.lines() {
        match line.split_once('=') {
            Some(("state", value)) => {
                state = serde_json::from_value(serde_json::Value::String(value.into()))
                    .unwrap_or(SetupState::Unknown);
            }
            Some(("name", value)) => name = shown(value),
            _ => {}
        }
    }
    let step = match state {
        SetupState::Running | SetupState::Failed => std::fs::read_to_string(directory.join("step"))
            .ok()
            .and_then(|step| step.lines().next().and_then(shown)),
        _ => None,
    };
    Some(Setup { state, name, step })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_runner_s_notes_are_read() {
        let directory = std::env::temp_dir().join(format!("setup-test-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        assert_eq!(setup(&directory), None);
        std::fs::write(directory.join("status"), "state=running\nname=Docker (1 of 2)\n").unwrap();
        std::fs::write(directory.join("step"), "Installing \u{202e}Docker\n").unwrap();
        assert_eq!(
            setup(&directory),
            Some(Setup {
                state: SetupState::Running,
                name: Some("Docker (1 of 2)".into()),
                step: Some("Installing Docker".into()),
            })
        );
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn link_local_addresses_are_left_out() {
        assert!(reachable(&"192.168.64.3".parse().unwrap()));
        assert!(reachable(&"fd00::3".parse().unwrap()));
        assert!(!reachable(&"169.254.1.1".parse().unwrap()));
        assert!(!reachable(&"fe80::1".parse().unwrap()));
        assert!(!reachable(&"::1".parse().unwrap()));
    }
}
