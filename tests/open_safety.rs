//! What an open may and may not do to a directory.
//!
//! - One handle per directory: a second open, in this process or another,
//!   is refused while the first is open, because two handles hand out the
//!   same run names and overwrite each other's runs.
//! - A directory without a manifest is a new store only when it is empty.
//!   Anything else is refused and left exactly as it was.
//! - A manifest names only run files in the directory.
//! - Without the `durability` feature, a directory whose `wal.log` holds
//!   writes is refused: this build cannot replay them, and a durable build
//!   would later replay them over the newer data.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use lsm_db::{Error, Lsm, LsmConfig};

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn run_files(dir: &Path) -> Vec<PathBuf> {
    let mut runs: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("run-") && n.ends_with(".sst"))
        })
        .collect();
    runs.sort();
    runs
}

fn is_held(result: &lsm_db::Result<Lsm>) -> bool {
    matches!(result, Err(Error::Io { source, .. }) if source.kind() == ErrorKind::WouldBlock)
}

/// Two handles on one directory both took run name 0 for their next flush,
/// and the second rename replaced the first handle's run: a flushed write
/// was gone after a reopen. The second open is refused now, and the
/// directory opens again once the first handle is dropped.
#[test]
fn a_second_handle_on_a_directory_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let config = || LsmConfig::new().compaction_trigger(usize::MAX);
    let a = Lsm::open_with(dir.path(), config()).unwrap();
    let second = Lsm::open_with(dir.path(), config());
    if let Ok(b) = second {
        // What two handles do to each other.
        a.put(b"k2", b"from a").unwrap();
        a.flush().unwrap();
        b.put(b"k3", b"from b").unwrap();
        b.flush().unwrap();
        drop(a);
        drop(b);
        let db = Lsm::open(dir.path()).unwrap();
        let k2 = db.get(b"k2").unwrap();
        panic!("a second handle opened the directory; after both flushed, k2 = {k2:?}");
    }
    assert!(is_held(&second), "{second:?}");
    drop(a);
    let again = Lsm::open(dir.path()).unwrap();
    drop(again);
}

/// The lock holds against another process too.
#[test]
fn a_second_process_cannot_open_the_directory() {
    let dir = tempfile::tempdir().unwrap();
    let db = Lsm::open(dir.path()).unwrap();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "probe_open_from_a_child_process",
            "--exact",
            "--test-threads=1",
        ])
        .env("LSM_DB_PROBE_DIR", dir.path())
        .status()
        .unwrap();
    assert_eq!(
        status.code(),
        Some(PROBE_REFUSED),
        "the child process could open the directory (3) or failed otherwise (4)"
    );
    drop(db);
}

/// The child's exit code when its open is refused as held.
const PROBE_REFUSED: i32 = 42;

/// Run by [`a_second_process_cannot_open_the_directory`] in a child
/// process; does nothing in a normal test run.
#[test]
fn probe_open_from_a_child_process() {
    let Some(dir) = std::env::var_os("LSM_DB_PROBE_DIR") else {
        return;
    };
    let result = Lsm::open(&dir);
    let code = if is_held(&result) {
        PROBE_REFUSED
    } else if result.is_ok() {
        3
    } else {
        4
    };
    std::process::exit(code);
}

/// A directory without a manifest that holds files other than a lock file,
/// temporaries, and a write-ahead log is not a new store. It used to be
/// opened as one, and the open removed the files it took for orphans (here
/// a bloom sidecar). It is refused now and left as it was.
#[test]
fn a_directory_without_a_manifest_and_with_other_files_is_refused_untouched() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("run-0000000003.sst.bloom"),
        b"a run's filter",
    )
    .unwrap();
    fs::write(dir.path().join("notes.txt"), b"not a store").unwrap();
    let before = names(dir.path());
    let result = Lsm::open(dir.path());
    assert!(
        matches!(result, Err(Error::InvalidInput { .. })),
        "{result:?}"
    );
    assert_eq!(names(dir.path()), before, "nothing is created or removed");
}

/// Run files without a manifest are refused and kept, whatever else the
/// directory holds.
#[test]
fn run_files_without_a_manifest_are_refused_and_kept() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Lsm::open(dir.path()).unwrap();
        db.put(b"k", b"v").unwrap();
        db.flush().unwrap();
    }
    fs::remove_file(dir.path().join("MANIFEST")).unwrap();
    let before = names(dir.path());
    assert!(matches!(
        Lsm::open(dir.path()),
        Err(Error::Corruption { .. })
    ));
    assert_eq!(names(dir.path()), before);
}

/// An empty directory, and one holding only a lock file and temporaries,
/// are new stores.
#[test]
fn an_empty_directory_is_a_new_store() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("LOCK"), b"").unwrap();
    fs::write(dir.path().join("run-0000000000.sst.tmp"), b"half a run").unwrap();
    let db = Lsm::open(dir.path()).unwrap();
    db.put(b"k", b"v").unwrap();
    drop(db);
    assert!(!dir.path().join("run-0000000000.sst.tmp").exists());
    let db = Lsm::open(dir.path()).unwrap();
    assert_eq!(db.get(b"k").unwrap(), Some(b"v".to_vec()));
}

/// A manifest line was joined to the directory as it stood, so a manifest
/// naming `../elsewhere/run-….sst` opened a run outside the directory, and
/// the next compaction deleted it. Such a name is refused now and the file
/// outside is left alone.
#[test]
fn a_manifest_naming_a_file_outside_the_directory_is_refused() {
    let root = tempfile::tempdir().unwrap();
    let db_dir = root.path().join("db");
    let elsewhere = root.path().join("elsewhere");
    fs::create_dir(&elsewhere).unwrap();
    {
        let db = Lsm::open(&db_dir).unwrap();
        db.put(b"k", b"v").unwrap();
        db.flush().unwrap();
    }
    let run = run_files(&db_dir).pop().unwrap();
    let outside = elsewhere.join("run-0000000001.sst");
    let _ = fs::copy(&run, &outside).unwrap();
    fs::remove_file(&run).unwrap();
    fs::write(
        db_dir.join("MANIFEST"),
        "LSMDB-MANIFEST v1\nnext_seq=2\n../elsewhere/run-0000000001.sst\n",
    )
    .unwrap();
    let result = Lsm::open(&db_dir);
    assert!(
        matches!(result, Err(Error::Corruption { .. })),
        "{result:?}"
    );
    assert!(outside.exists());
}

/// A run file named with the largest sequence number used to overflow the
/// next sequence number (a panic in a debug build). It is reported as
/// corruption.
#[test]
fn a_run_file_with_the_largest_sequence_number_is_corruption() {
    let dir = tempfile::tempdir().unwrap();
    drop(Lsm::open(dir.path()).unwrap());
    fs::write(dir.path().join(format!("run-{}.sst", u64::MAX)), b"junk").unwrap();
    let result = Lsm::open(dir.path());
    assert!(
        matches!(result, Err(Error::Corruption { .. })),
        "{result:?}"
    );
}

/// A durable build logs writes to `wal.log`; one without `durability`
/// ignored the file, so the writes it held were replayed by the next
/// durable open over everything written in between: deleted keys came
/// back and new values reverted. A build without the feature refuses such
/// a directory now and changes nothing in it.
#[cfg(not(feature = "durability"))]
#[test]
fn a_non_empty_write_ahead_log_is_refused_without_durability() {
    let dir = tempfile::tempdir().unwrap();
    drop(Lsm::open(dir.path()).unwrap());
    // Any bytes: this build cannot read the log, so it cannot tell a log
    // with records from one without.
    fs::write(dir.path().join("wal.log"), b"logged by a durable build").unwrap();
    fs::write(
        dir.path().join("run-0000000007.sst.tmp"),
        b"left by a crash",
    )
    .unwrap();
    let before = names(dir.path());
    let result = Lsm::open(dir.path());
    assert!(
        matches!(result, Err(Error::InvalidInput { .. })),
        "{result:?}"
    );
    assert_eq!(names(dir.path()), before, "nothing is removed");

    // An empty log, which a durable build leaves after every flush, is fine.
    fs::write(dir.path().join("wal.log"), b"").unwrap();
    drop(Lsm::open(dir.path()).unwrap());
}
