//! The on-disk manifest: which runs are live, and in what order.
//!
//! A store with several runs cannot recover its state from the run files
//! alone with certainty: a run's
//! filename does not say whether it is still live or how recent it is relative
//! to the others, and a crash mid-compaction can leave both the inputs and the
//! output on disk. The manifest is the single source of truth: it lists the live
//! runs in recency order (newest first) and the next sequence number to hand out.
//!
//! It is rewritten atomically on every flush and compaction — written to a
//! temporary file, `fsync`ed, then renamed over the old one. A crash therefore
//! leaves either the old manifest or the new one, never a torn list; any run
//! file not named by the surviving manifest is an orphan the next open reclaims.
//!
//! ## Format (text, line-oriented)
//!
//! ```text
//! LSMDB-MANIFEST v1
//! next_seq=<u64>
//! <run filename>      # newest
//! <run filename>
//! …                   # oldest
//! ```
//!
//! Since 1.4 every run line must be a run filename the engine writes
//! (`run-<seq>.sst`, the sequence number zero-padded to ten digits): a path,
//! a parent reference, or any other name makes the manifest corrupt, so a
//! manifest can never make the engine open or delete a file outside its
//! directory.

use std::fs;
use std::path::Path;

use crate::error::{Error, Result};

/// Name of the manifest file inside the database directory.
pub(crate) const MANIFEST: &str = "MANIFEST";
/// Name of the temporary manifest written before the atomic rename.
const MANIFEST_TMP: &str = "MANIFEST.tmp";
/// First line identifying the manifest and its version.
const HEADER: &str = "LSMDB-MANIFEST v1";

/// The live run list and sequence counter, as recorded on disk.
#[derive(Debug, Clone, Default)]
pub(crate) struct Manifest {
    /// The next run sequence number to allocate.
    pub(crate) next_seq: u64,
    /// Live run filenames, newest first.
    pub(crate) runs: Vec<String>,
}

impl Manifest {
    /// Load the manifest from `dir`, or `None` if there is none.
    ///
    /// Only a manifest that does not exist (`NotFound`) is `None`; any other
    /// failure to read it is an error, so a permission or sharing problem is
    /// never mistaken for a missing manifest.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] when the file exists but cannot be read, and
    /// [`Error::Corruption`] when it is malformed or names a file that is not
    /// a run (a path, or a name the engine does not write).
    pub(crate) fn load(dir: &Path) -> Result<Option<Manifest>> {
        let path = dir.join(MANIFEST);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(Error::io("read manifest", e)),
        };
        let mut lines = text.lines();
        match lines.next() {
            Some(HEADER) => {}
            _ => return Err(Error::corruption("manifest header missing or unrecognised")),
        }
        let next_seq = match lines.next() {
            Some(line) => line
                .strip_prefix("next_seq=")
                .and_then(|n| n.parse::<u64>().ok())
                .ok_or_else(|| Error::corruption("manifest next_seq line malformed"))?,
            None => return Err(Error::corruption("manifest truncated")),
        };
        let mut runs = Vec::new();
        for line in lines.filter(|l| !l.is_empty()) {
            // Only the names the engine writes: no path separators, no
            // parent references, nothing outside the directory.
            if !is_run_name(line) {
                return Err(Error::corruption("manifest names a file that is not a run"));
            }
            runs.push(line.to_owned());
        }
        Ok(Some(Manifest { next_seq, runs }))
    }

    /// Atomically write the manifest to `dir`, then sync the directory.
    pub(crate) fn store(dir: &Path, next_seq: u64, runs: &[String]) -> Result<()> {
        Self::install(dir, next_seq, runs)?;
        // The rename is durable only once the directory is.
        crate::fault::sync_dir(dir)
    }

    /// Atomically replace the manifest in `dir`: write a temporary file, sync
    /// it, rename it over the old one. The directory is not synced; the
    /// caller does that, and knows on an error from that sync that the new
    /// manifest is already in place, so the files it names must stay.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] when the manifest could not be installed. The old one
    /// is then still in place.
    pub(crate) fn install(dir: &Path, next_seq: u64, runs: &[String]) -> Result<()> {
        use std::io::Write;

        let mut body = String::with_capacity(64 + runs.len() * 24);
        body.push_str(HEADER);
        body.push('\n');
        body.push_str("next_seq=");
        body.push_str(&next_seq.to_string());
        body.push('\n');
        for run in runs {
            body.push_str(run);
            body.push('\n');
        }

        let tmp = dir.join(MANIFEST_TMP);
        let final_path = dir.join(MANIFEST);
        {
            let mut file = fs::File::create(&tmp).map_err(|e| Error::io("create manifest", e))?;
            file.write_all(body.as_bytes())
                .map_err(|e| Error::io("write manifest", e))?;
            file.sync_all()
                .map_err(|e| Error::io("flush manifest to stable storage", e))?;
        }
        let installed = crate::fault::check("install manifest").and_then(|()| {
            fs::rename(&tmp, &final_path).map_err(|e| Error::io("install manifest", e))
        });
        if let Err(err) = installed {
            let _ = fs::remove_file(&tmp);
            return Err(err);
        }
        Ok(())
    }
}

/// The filename for the run with sequence number `seq`.
pub(crate) fn run_filename(seq: u64) -> String {
    format!("run-{seq:010}.sst")
}

/// Extract the sequence number from a run filename, if it is one: exactly
/// the name [`run_filename`] gives that number.
pub(crate) fn seq_of(name: &str) -> Option<u64> {
    let digits = name.strip_prefix("run-")?.strip_suffix(".sst")?;
    if digits.len() < 10 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let seq = digits.parse::<u64>().ok()?;
    (run_filename(seq) == name).then_some(seq)
}

/// Whether `name` is a run filename the engine writes.
pub(crate) fn is_run_name(name: &str) -> bool {
    seq_of(name).is_some()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_filename_and_seq_roundtrip() {
        assert_eq!(run_filename(42), "run-0000000042.sst");
        assert_eq!(seq_of("run-0000000042.sst"), Some(42));
        assert_eq!(seq_of("MANIFEST"), None);
        assert_eq!(seq_of("run-bad.sst"), None);
        assert_eq!(seq_of("run-+000000042.sst"), None);
        assert_eq!(seq_of("run-42.sst"), None);
        assert_eq!(seq_of("run-00000000042.sst"), None);
        assert_eq!(seq_of(&run_filename(u64::MAX)), Some(u64::MAX));
    }

    #[test]
    fn test_names_outside_the_directory_are_corruption() {
        for name in [
            "../run-0000000001.sst",
            "/tmp/run-0000000001.sst",
            "x.sst",
            "sub/run-0000000001.sst",
        ] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(
                dir.path().join(MANIFEST),
                format!(
                    "{HEADER}
next_seq=2
{name}
"
                ),
            )
            .unwrap();
            assert!(
                matches!(Manifest::load(dir.path()), Err(Error::Corruption { .. })),
                "{name}"
            );
        }
    }

    #[test]
    fn test_store_then_load() {
        let dir = tempfile::tempdir().unwrap();
        let runs = vec![run_filename(5), run_filename(3), run_filename(1)];
        Manifest::store(dir.path(), 6, &runs).unwrap();

        let loaded = Manifest::load(dir.path()).unwrap().unwrap();
        assert_eq!(loaded.next_seq, 6);
        assert_eq!(loaded.runs, runs);
    }

    #[test]
    fn test_load_absent_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Manifest::load(dir.path()).unwrap().is_none());
    }

    #[test]
    fn test_load_empty_run_list() {
        let dir = tempfile::tempdir().unwrap();
        Manifest::store(dir.path(), 1, &[]).unwrap();
        let loaded = Manifest::load(dir.path()).unwrap().unwrap();
        assert_eq!(loaded.next_seq, 1);
        assert!(loaded.runs.is_empty());
    }

    #[test]
    fn test_bad_header_is_corruption() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(MANIFEST), "garbage\n").unwrap();
        assert!(Manifest::load(dir.path()).is_err());
    }
}
