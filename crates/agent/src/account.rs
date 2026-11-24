//! The guest's accounts, and running a program as one of them.

use std::ffi::{CStr, CString};
use std::io;
use std::path::PathBuf;

pub struct Account {
    pub name: String,
    pub uid: libc::uid_t,
    pub gid: libc::gid_t,
    pub home: PathBuf,
    pub shell: String,
}

/// The account called `name`, if there is one.
pub fn find(name: &str) -> Option<Account> {
    let c_name = CString::new(name).ok()?;
    let mut buffer = vec![0 as libc::c_char; 16 * 1024];
    // Safety: an all-zero passwd is a valid value for getpwnam_r to fill in.
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut found = std::ptr::null_mut();
    // Safety: the buffers live through the call, and their lengths are theirs.
    let result = unsafe {
        libc::getpwnam_r(
            c_name.as_ptr(),
            &mut entry,
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut found,
        )
    };
    if result != 0 || found.is_null() {
        return None;
    }
    // Safety: getpwnam_r filled these in with strings in the buffer.
    let text = |pointer: *const libc::c_char| unsafe {
        if pointer.is_null() {
            String::new()
        } else {
            CStr::from_ptr(pointer).to_string_lossy().into_owned()
        }
    };
    Some(Account {
        name: name.to_string(),
        uid: entry.pw_uid,
        gid: entry.pw_gid,
        home: PathBuf::from(text(entry.pw_dir)),
        shell: text(entry.pw_shell),
    })
}

/// The groups that `name` is in, its own `gid` among them.
#[cfg(target_os = "linux")]
fn groups(name: &CStr, gid: libc::gid_t) -> Vec<libc::gid_t> {
    let mut count: libc::c_int = 64;
    loop {
        let mut groups = vec![0; count as usize];
        let wanted = count;
        // Safety: the list has room for `count` groups, which the call is told.
        let result = unsafe { libc::getgrouplist(name.as_ptr(), gid, groups.as_mut_ptr(), &mut count) };
        if result >= 0 {
            groups.truncate(count as usize);
            return groups;
        }
        if count <= wanted {
            count = wanted * 2;
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn groups(_name: &CStr, gid: libc::gid_t) -> Vec<libc::gid_t> {
    vec![gid]
}

impl Account {
    /// What a new process calls, between fork and exec, to become this account. Everything it
    /// needs is looked up before, here: only system calls are safe to make there.
    pub fn switch(&self) -> io::Result<impl Fn() -> io::Result<()> + Send + Sync + 'static> {
        let name = CString::new(self.name.as_str()).map_err(|_| io::ErrorKind::InvalidInput)?;
        let groups = groups(&name, self.gid);
        let (uid, gid) = (self.uid, self.gid);
        Ok(move || {
            // Safety: system calls with a list that lives through them.
            unsafe {
                if libc::setgroups(groups.len() as _, groups.as_ptr()) != 0
                    || libc::setgid(gid) != 0
                    || libc::setuid(uid) != 0
                {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        })
    }
}
