//! Merging across the memtable and runs.
//!
//! Reads and compaction both need one ascending stream of the *current* value
//! per key, drawn from several sorted sources that may disagree. [`Merge`]
//! produces exactly that: it walks the memtable snapshot and every run in
//! parallel, and for each key keeps the record from the newest source. A
//! tombstone there resolves the key away — it is the newest word on the key, so
//! the key is deleted and nothing is emitted.
//!
//! Sources are supplied newest first. Because the only consumers are full-range
//! reads and full compaction (which always merges every run into one), there is
//! never an older level left below, so tombstones are always dropped rather than
//! carried forward.

use std::iter::Peekable;

use crate::error::{Error, Result};
use crate::record::Record;
use crate::sstable::RunCursor;

/// One input to a [`Merge`]: the memtable snapshot or a run cursor.
enum Source<'a> {
    Mem(Peekable<std::vec::IntoIter<(Vec<u8>, Record)>>),
    Run(RunCursor<'a>),
}

impl Source<'_> {
    fn peek_key(&mut self) -> Option<&[u8]> {
        match self {
            Source::Mem(it) => it.peek().map(|(k, _)| k.as_slice()),
            Source::Run(cur) => cur.peek_key(),
        }
    }

    fn next_entry(&mut self) -> Option<(Vec<u8>, Record)> {
        match self {
            Source::Mem(it) => it.next(),
            Source::Run(cur) => cur.next_entry(),
        }
    }

    fn take_error(&mut self) -> Option<Error> {
        match self {
            Source::Mem(_) => None,
            Source::Run(cur) => cur.take_error(),
        }
    }
}

/// An ascending merge over a memtable snapshot and a set of runs.
///
/// Yields `Result<(key, value)>` for each live key, newest source winning and
/// tombstones resolved away. A block read or checksum failure in any run is
/// surfaced as an `Err` rather than silently truncating the stream.
pub(crate) struct Merge<'a> {
    /// Sources in recency order, newest first.
    sources: Vec<Source<'a>>,
}

impl<'a> Merge<'a> {
    /// Build a merge. `mem` is the (already sorted) memtable snapshot; `runs`
    /// are run cursors in recency order, newest first.
    pub(crate) fn new(mem: Vec<(Vec<u8>, Record)>, runs: Vec<RunCursor<'a>>) -> Self {
        let mut sources = Vec::with_capacity(runs.len() + 1);
        sources.push(Source::Mem(mem.into_iter().peekable()));
        sources.extend(runs.into_iter().map(Source::Run));
        Merge { sources }
    }
}

impl Iterator for Merge<'_> {
    type Item = Result<(Vec<u8>, Vec<u8>)>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            // Fill every source so peeks and errors are up to date.
            for source in self.sources.iter_mut() {
                let _ = source.peek_key();
            }
            for source in self.sources.iter_mut() {
                if let Some(err) = source.take_error() {
                    return Some(Err(err));
                }
            }

            // Smallest key currently available across all sources.
            let min = self
                .sources
                .iter_mut()
                .filter_map(Source::peek_key)
                .min()
                .map(<[u8]>::to_vec)?;

            // Take that key from every source; the first (newest) record wins.
            let mut chosen: Option<Record> = None;
            for source in self.sources.iter_mut() {
                if source.peek_key() == Some(min.as_slice()) {
                    if let Some((_key, record)) = source.next_entry() {
                        if chosen.is_none() {
                            chosen = Some(record);
                        }
                    }
                }
            }

            match chosen {
                Some(Record::Value(value)) => return Some(Ok((min, value))),
                // Tombstone (or the unreachable empty case): key is gone; keep going.
                _ => continue,
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::sstable::{SsTable, SsTableWriter};
    use std::path::Path;

    fn make_run(path: &Path, entries: &[(&[u8], Record)]) -> SsTable {
        let mut w = SsTableWriter::create(path).unwrap();
        for (k, r) in entries {
            w.push(k, r).unwrap();
        }
        w.finish().unwrap();
        SsTable::open(path).unwrap()
    }

    fn val(v: &[u8]) -> Record {
        Record::Value(v.to_vec())
    }

    fn collect(mem: Vec<(Vec<u8>, Record)>, runs: &[&SsTable]) -> Vec<(Vec<u8>, Vec<u8>)> {
        let cursors: Vec<RunCursor> = runs.iter().map(|t| t.cursor()).collect();
        Merge::new(mem, cursors).map(|r| r.unwrap()).collect()
    }

    #[test]
    fn test_mem_only() {
        let mem = vec![(b"a".to_vec(), val(b"1")), (b"b".to_vec(), val(b"2"))];
        assert_eq!(
            collect(mem, &[]),
            vec![
                (b"a".to_vec(), b"1".to_vec()),
                (b"b".to_vec(), b"2".to_vec())
            ]
        );
    }

    #[test]
    fn test_newest_source_wins() {
        let dir = tempfile::tempdir().unwrap();
        let old = make_run(
            &dir.path().join("0.sst"),
            &[(b"k", val(b"old")), (b"x", val(b"keep"))],
        );
        let new = make_run(&dir.path().join("1.sst"), &[(b"k", val(b"new"))]);
        // mem newest, then new, then old.
        let got = collect(vec![], &[&new, &old]);
        assert_eq!(
            got,
            vec![
                (b"k".to_vec(), b"new".to_vec()),
                (b"x".to_vec(), b"keep".to_vec())
            ]
        );
    }

    #[test]
    fn test_memtable_shadows_runs() {
        let dir = tempfile::tempdir().unwrap();
        let run = make_run(&dir.path().join("0.sst"), &[(b"k", val(b"disk"))]);
        let got = collect(vec![(b"k".to_vec(), val(b"mem"))], &[&run]);
        assert_eq!(got, vec![(b"k".to_vec(), b"mem".to_vec())]);
    }

    #[test]
    fn test_tombstone_resolves_key_away() {
        let dir = tempfile::tempdir().unwrap();
        let run = make_run(&dir.path().join("0.sst"), &[(b"k", val(b"disk"))]);
        // Newer tombstone in mem hides the on-disk value.
        let got = collect(vec![(b"k".to_vec(), Record::Tombstone)], &[&run]);
        assert!(got.is_empty());
    }

    #[test]
    fn test_three_way_interleave() {
        let dir = tempfile::tempdir().unwrap();
        let r0 = make_run(
            &dir.path().join("0.sst"),
            &[(b"a", val(b"a0")), (b"d", val(b"d0"))],
        );
        let r1 = make_run(
            &dir.path().join("1.sst"),
            &[(b"b", val(b"b1")), (b"d", val(b"d1"))],
        );
        let mem = vec![(b"c".to_vec(), val(b"cm"))];
        // recency: mem, r1, r0
        let got = collect(mem, &[&r1, &r0]);
        assert_eq!(
            got,
            vec![
                (b"a".to_vec(), b"a0".to_vec()),
                (b"b".to_vec(), b"b1".to_vec()),
                (b"c".to_vec(), b"cm".to_vec()),
                (b"d".to_vec(), b"d1".to_vec()), // r1 newer than r0
            ]
        );
    }
}
