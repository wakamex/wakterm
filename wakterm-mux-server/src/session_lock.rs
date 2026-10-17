//! Keeps a second mux server from serving the same saved session.
//!
//! A server restores `session.json` and resumes every agent in it, and
//! records agent events in the shared agent store, all under the data
//! directory. Two servers on the same data directory would resume each
//! agent twice. Each server therefore holds an exclusive lock on a file in
//! the data directory for as long as it runs. The lock does not depend on
//! the runtime directory, which holds the socket and can differ between
//! two environments of the same user.

use anyhow::Context;
use std::fs::File;
use std::io::{Read, Seek, Write};
use std::path::PathBuf;

fn lock_path() -> PathBuf {
    config::DATA_DIR.join("mux-server.lock")
}

/// Locks the session for this server, or explains which server holds it.
/// The lock lasts until the returned file is dropped.
pub fn acquire() -> anyhow::Result<File> {
    let path = lock_path();
    std::fs::create_dir_all(&*config::DATA_DIR)
        .with_context(|| format!("creating {}", config::DATA_DIR.display()))?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    if !try_lock(&file)? {
        let mut holder = String::new();
        file.read_to_string(&mut holder).ok();
        anyhow::bail!(
            "another wakterm-mux-server (pid {}) is serving the saved session in {}; \
             not starting a second one",
            holder
                .trim()
                .parse::<u32>()
                .map_or("unknown".to_string(), |pid| pid.to_string()),
            config::DATA_DIR.display()
        );
    }
    file.set_len(0)?;
    file.rewind()?;
    writeln!(file, "{}", std::process::id())?;
    Ok(file)
}

#[cfg(unix)]
fn try_lock(file: &File) -> anyhow::Result<bool> {
    use std::os::unix::io::AsRawFd;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(true);
    }
    let err = std::io::Error::last_os_error();
    if err.kind() == std::io::ErrorKind::WouldBlock {
        Ok(false)
    } else {
        Err(err).context("locking the mux server's session lock")
    }
}

#[cfg(windows)]
fn try_lock(file: &File) -> anyhow::Result<bool> {
    use std::os::windows::io::AsRawHandle;
    use winapi::shared::winerror::ERROR_LOCK_VIOLATION;
    use winapi::um::fileapi::LockFileEx;
    use winapi::um::minwinbase::{LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, OVERLAPPED};
    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    let locked = unsafe {
        LockFileEx(
            file.as_raw_handle() as _,
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            !0,
            !0,
            &mut overlapped,
        )
    };
    if locked != 0 {
        return Ok(true);
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32) {
        Ok(false)
    } else {
        Err(err).context("locking the mux server's session lock")
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn a_held_lock_refuses_a_second_holder_until_it_is_released() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mux-server.lock");
        let open = || {
            std::fs::OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .open(&path)
                .unwrap()
        };
        let first = open();
        assert!(try_lock(&first).unwrap());
        assert!(!try_lock(&open()).unwrap());
        drop(first);
        assert!(try_lock(&open()).unwrap());
    }
}
