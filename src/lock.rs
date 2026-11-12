//! The directory lock: one open handle per database directory.
//!
//! Two handles on one directory would hand out the same run names and
//! overwrite each other's runs and manifest. [`DirLock`] takes an exclusive
//! lock on a `LOCK` file in the directory when the database opens and holds
//! it until the handle is gone, so a second open, in this process or another,
//! fails with an error instead.
//!
//! How the lock is taken depends on the compiler (the build script detects
//! it):
//!
//! - Rust 1.89 and later, every platform: `File::try_lock`, an advisory lock
//!   (`flock` on Unix, `LockFileEx` on Windows) that the operating system
//!   releases when the process ends, however it ends.
//! - Windows before 1.89: the lock file is opened sharing only delete access,
//!   so the system refuses every other open that reads or writes it until
//!   this handle closes. (Delete is shared so the directory can still be
//!   removed while the store is open, as with the lock of 1.89.)
//! - Unix before 1.89: the standard library has no file lock and the crate
//!   takes no new `unsafe` or dependency for one, so the lock file holds the
//!   owner's process id. A file naming a live process is held; one naming a
//!   process that has ended is taken over (the process is looked up through
//!   `/proc` on Linux and `ps` elsewhere). Two processes taking over the same
//!   stale lock at the same moment are not told apart; builds with 1.89 or
//!   later use the real lock.
//!
//! In every case the lock file also records the owner's process id, for an
//! operator looking at a held directory. The file is never removed except by
//! the process-id fallback, so taking the lock never races with a removal.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;
#[cfg(all(not(lsm_file_lock), unix))]
use std::path::PathBuf;

use crate::error::{Error, Result};

/// Name of the lock file inside the database directory.
pub(crate) const LOCK_FILE: &str = "LOCK";

/// Context of the error a held directory returns.
const HELD: &str = "lock database directory (another handle has it open)";

/// How long a held lock is tried again before it is reported held. A
/// process that starts a child holds a copy of each open file until the
/// child runs its program, and an advisory lock belongs to the open file, so
/// a handle dropped at that moment keeps its lock a little longer; a reopen
/// right after (in this process) waits that out instead of failing.
const HELD_WAIT: std::time::Duration = std::time::Duration::from_millis(200);

/// Exclusive ownership of a database directory, released on drop.
#[derive(Debug)]
pub(crate) struct DirLock {
    _file: File,
    #[cfg(all(not(lsm_file_lock), unix))]
    path: PathBuf,
}

impl DirLock {
    /// Take the lock on `dir`.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] whose source has kind [`io::ErrorKind::WouldBlock`] when
    /// another handle holds the directory; [`Error::Io`] when the lock file
    /// cannot be opened or written.
    pub(crate) fn acquire(dir: &Path) -> Result<DirLock> {
        let path = dir.join(LOCK_FILE);
        let started = std::time::Instant::now();
        let file = loop {
            match lock_file(&path) {
                Err(Error::Io { source, .. })
                    if source.kind() == io::ErrorKind::WouldBlock
                        && started.elapsed() < HELD_WAIT =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                other => break other?,
            }
        };
        Ok(DirLock {
            _file: file,
            #[cfg(all(not(lsm_file_lock), unix))]
            path,
        })
    }
}

/// The error for a directory another handle holds.
fn held() -> Error {
    Error::io(
        HELD,
        io::Error::new(
            io::ErrorKind::WouldBlock,
            "the database directory is open in another handle",
        ),
    )
}

/// Rust 1.89 and later: an advisory lock through the standard library.
#[cfg(lsm_file_lock)]
#[allow(clippy::incompatible_msrv)]
fn lock_file(path: &Path) -> Result<File> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|e| Error::io("open lock file", e))?;
    match file.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Err(held()),
        Err(std::fs::TryLockError::Error(e)) => {
            return Err(Error::io("lock database directory", e));
        }
    }
    write_pid(&mut file).map_err(|e| Error::io("write lock file", e))?;
    Ok(file)
}

/// Windows before Rust 1.89: open the lock file sharing only delete access.
#[cfg(all(not(lsm_file_lock), windows))]
fn lock_file(path: &Path) -> Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    /// `ERROR_SHARING_VIOLATION`.
    const SHARING_VIOLATION: i32 = 32;
    /// `FILE_SHARE_DELETE`.
    const SHARE_DELETE: u32 = 0x4;
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(SHARE_DELETE)
        .open(path)
        .map_err(|e| {
            if e.raw_os_error() == Some(SHARING_VIOLATION) {
                held()
            } else {
                Error::io("open lock file", e)
            }
        })?;
    write_pid(&mut file).map_err(|e| Error::io("write lock file", e))?;
    Ok(file)
}

/// Unix before Rust 1.89: the lock file names its owner's process id.
#[cfg(all(not(lsm_file_lock), unix))]
fn lock_file(path: &Path) -> Result<File> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let pid = std::str::from_utf8(&bytes)
                .ok()
                .and_then(|text| text.trim().parse::<u32>().ok());
            if pid.is_some_and(process_is_alive) {
                return Err(held());
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(Error::io("read lock file", e)),
    }
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .map_err(|e| Error::io("open lock file", e))?;
    write_pid(&mut file).map_err(|e| Error::io("write lock file", e))?;
    Ok(file)
}

/// Whether `pid` names a running process. When that cannot be told, the
/// answer is yes: the directory stays locked.
#[cfg(all(not(lsm_file_lock), unix))]
fn process_is_alive(pid: u32) -> bool {
    if cfg!(target_os = "linux") {
        Path::new("/proc").join(pid.to_string()).exists()
    } else {
        std::process::Command::new("/bin/ps")
            .arg("-p")
            .arg(pid.to_string())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map_or(true, |status| status.success())
    }
}

#[cfg(all(not(lsm_file_lock), unix))]
impl Drop for DirLock {
    fn drop(&mut self) {
        // Only the process-id fallback removes its file, and only while it
        // still names this process.
        if let Ok(bytes) = std::fs::read(&self.path) {
            let own = std::process::id().to_string();
            if std::str::from_utf8(&bytes).map(str::trim) == Ok(own.as_str()) {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }
}

/// Record the owner's process id in the lock file. Not synced: after a
/// crash the owner is gone, whatever the file says.
fn write_pid(file: &mut File) -> io::Result<()> {
    use std::io::{Seek, Write};
    file.set_len(0)?;
    let _ = file.seek(io::SeekFrom::Start(0))?;
    write!(file, "{}", std::process::id())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_second_acquire_is_refused_until_the_first_drops() {
        let dir = tempfile::tempdir().unwrap();
        let first = DirLock::acquire(dir.path()).unwrap();
        match DirLock::acquire(dir.path()) {
            Err(Error::Io { source, .. }) => {
                assert_eq!(source.kind(), io::ErrorKind::WouldBlock);
            }
            other => panic!("expected a held lock, got {other:?}"),
        }
        drop(first);
        let again = DirLock::acquire(dir.path()).unwrap();
        drop(again);
    }

    /// Before 1.4.1 a lock still held through a copy of its file (a child
    /// process between its start and its program, which holds a copy of
    /// every open file) made a reopen right after a drop fail as "held".
    #[cfg(all(lsm_file_lock, unix))]
    #[test]
    fn test_a_lock_held_a_moment_longer_by_a_copy_is_waited_for() {
        let dir = tempfile::tempdir().unwrap();
        let first = DirLock::acquire(dir.path()).unwrap();
        let copy = first._file.try_clone().unwrap();
        drop(first);
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            drop(copy);
        });
        let again = DirLock::acquire(dir.path());
        release.join().unwrap();
        assert!(again.is_ok(), "{again:?}");
    }

    #[test]
    fn test_lock_file_names_the_owner() {
        let dir = tempfile::tempdir().unwrap();
        let lock = DirLock::acquire(dir.path()).unwrap();
        // Windows refuses a read of a locked or unshared file; the content is
        // checked where it can be read.
        if let Ok(text) = std::fs::read_to_string(dir.path().join(LOCK_FILE)) {
            assert_eq!(text, std::process::id().to_string());
        }
        drop(lock);
    }
}
