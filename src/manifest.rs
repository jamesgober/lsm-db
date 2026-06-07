//! The on-disk manifest: which runs are live, and in what order.
//!
//! A leveled store cannot recover its state from the run files alone — a run's
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
    /// Load the manifest from `dir`, or `None` if there is none (a fresh store).
    pub(crate) fn load(dir: &Path) -> Result<Option<Manifest>> {
        let path = dir.join(MANIFEST);
        if !path.exists() {
            return Ok(None);
        }
        let text = fs::read_to_string(&path).map_err(|e| Error::io("read manifest", e))?;
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
        let runs = lines.filter(|l| !l.is_empty()).map(str::to_owned).collect();
        Ok(Some(Manifest { next_seq, runs }))
    }

    /// Atomically write the manifest to `dir`.
    pub(crate) fn store(dir: &Path, next_seq: u64, runs: &[String]) -> Result<()> {
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
        fs::rename(&tmp, &final_path).map_err(|e| Error::io("install manifest", e))?;
        Ok(())
    }
}

/// The filename for the run with sequence number `seq`.
pub(crate) fn run_filename(seq: u64) -> String {
    format!("run-{seq:010}.sst")
}

/// Extract the sequence number from a run filename, if it is one.
pub(crate) fn seq_of(name: &str) -> Option<u64> {
    name.strip_prefix("run-")
        .and_then(|rest| rest.strip_suffix(".sst"))
        .and_then(|digits| digits.parse::<u64>().ok())
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
