//! Durability steps that can fail, named so tests can make each one fail.
//!
//! In a normal build [`check`] does nothing and [`sync_dir`] syncs the
//! directory. In the crate's own tests a point can be armed on the current
//! thread with [`arm`]: the next [`check`] of that point on that thread
//! fails once with an I/O error, which is how the fault-injection tests
//! drive a flush through every failure deterministically.

use std::path::Path;

use crate::error::Result;

#[cfg(test)]
thread_local! {
    static ARMED: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) };
    static DIR_SYNCS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Make the next [`check`] of `point` on this thread fail.
#[cfg(test)]
pub(crate) fn arm(point: &'static str) {
    ARMED.with(|armed| armed.set(Some(point)));
}

/// How many directory syncs this thread has made.
#[cfg(test)]
pub(crate) fn dir_syncs() -> usize {
    DIR_SYNCS.with(std::cell::Cell::get)
}

/// Fail here when a test armed `point`; otherwise nothing.
#[inline]
pub(crate) fn check(point: &'static str) -> Result<()> {
    #[cfg(test)]
    if ARMED.with(std::cell::Cell::get) == Some(point) {
        ARMED.with(|armed| armed.set(None));
        return Err(crate::error::Error::io(
            point,
            std::io::Error::other("injected fault"),
        ));
    }
    let _ = point;
    Ok(())
}

/// Sync the directory `dir`, so the files created, renamed, or removed in
/// it survive a power loss with the names they now have. On Unix this is an
/// `fsync` of the directory; on Windows, which keeps directory entries with
/// the file system's own journal and cannot open a directory as a file
/// through the standard library, nothing.
///
/// # Errors
///
/// [`Error::Io`] when the directory cannot be opened or synced.
pub(crate) fn sync_dir(dir: &Path) -> Result<()> {
    check("sync directory")?;
    #[cfg(test)]
    DIR_SYNCS.with(|n| n.set(n.get() + 1));
    #[cfg(unix)]
    std::fs::File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(|e| crate::error::Error::io("sync directory", e))?;
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}
