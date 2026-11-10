//! The sorted-run format as an in-memory codec.
//!
//! This module reads and writes the `LSMTBL01` sorted-run format specified in
//! `docs/SSTABLE_FORMAT.md` (frozen for 1.x) over byte buffers instead of files.
//! It is the part of `lsm-db` that works without the standard library: with
//! the `alloc` feature it builds on `no_std` targets, so a run produced by the
//! engine on a host can be read on a device, and a run built on a device can be
//! shipped to a host and installed there.
//!
//! - [`RunWriter`] encodes entries, given in strictly ascending key order, into
//!   a complete run.
//! - [`RunReader`] validates a run held in memory and serves point lookups and
//!   in-order iteration. It borrows from the buffer and copies no keys or
//!   values.
//! - [`crc32c`](fn@crc32c) is the checksum the format uses.
//!
//! The output of [`RunWriter`] is byte-for-byte what the engine writes when it
//! flushes the same entries, and [`RunReader`] applies the same integrity
//! checks the engine applies when it opens a run.
//!
//! ```
//! # #[cfg(feature = "alloc")]
//! # fn main() -> lsm_db::Result<()> {
//! use lsm_db::format::{Entry, RunReader, RunWriter};
//!
//! let mut writer = RunWriter::new();
//! writer.put(b"apple", b"red")?;
//! writer.delete(b"banana")?;
//! writer.put(b"cherry", b"dark red")?;
//! let bytes = writer.finish();
//!
//! let run = RunReader::new(&bytes)?;
//! assert_eq!(run.get(b"apple")?, Some(Entry::Value(b"red")));
//! assert_eq!(run.get(b"banana")?, Some(Entry::Tombstone));
//! assert_eq!(run.get(b"durian")?, None);
//! assert_eq!(run.iter().count(), 3);
//! # Ok(())
//! # }
//! # #[cfg(not(feature = "alloc"))]
//! # fn main() {}
//! ```

#[cfg(feature = "alloc")]
use alloc::vec::Vec;

#[cfg(feature = "alloc")]
use crate::error::{Error, Result};

/// File magic identifying an `lsm-db` v1 sorted run. It opens and closes every
/// run.
#[cfg(feature = "alloc")]
pub(crate) const MAGIC: &[u8; 8] = b"LSMTBL01";

/// Fixed footer size: `entry_count` + `index_offset` + `index_len` +
/// `index_crc` + `magic`.
#[cfg(feature = "alloc")]
pub(crate) const FOOTER_LEN: usize = 8 + 8 + 8 + 4 + 8;

/// Target size of a data block before it is closed. A single entry larger than
/// this becomes a block of its own; entries are never split across blocks.
#[cfg(feature = "alloc")]
pub(crate) const TARGET_BLOCK_SIZE: usize = 4 * 1024;

/// Tag byte for a live value.
#[cfg(feature = "alloc")]
const TAG_VALUE: u8 = 0;
/// Tag byte for a tombstone.
#[cfg(feature = "alloc")]
const TAG_TOMBSTONE: u8 = 1;

/// The longest key a run may hold, in bytes (2³⁰, 1 GiB).
///
/// The format caps every length prefix at this value so a corrupt or hostile
/// prefix cannot drive an unbounded allocation. [`RunWriter`] and the engine's
/// write methods reject longer keys with an
/// [`InvalidInput`](crate::Error::InvalidInput) error.
pub const MAX_KEY_LEN: usize = 1 << 30;

/// The longest value a run may hold, in bytes (2³⁰, 1 GiB).
///
/// [`RunWriter`] and the engine's write methods reject longer values with an
/// [`InvalidInput`](crate::Error::InvalidInput) error.
pub const MAX_VALUE_LEN: usize = 1 << 30;

/// Computes the CRC32C (Castagnoli) checksum the format uses for data blocks,
/// the index block, and bloom sidecars.
///
/// With `std` this uses the SSE4.2 or ARMv8 CRC instructions when the CPU has
/// them (asked at run time) and tables otherwise. Without `std` it uses those instructions
/// when the target enables them at compile time (`-C target-feature=+sse4.2` on
/// x86_64, `+crc` on aarch64) and a table-driven implementation otherwise. All
/// paths produce identical values.
///
/// # Examples
///
/// ```
/// // The standard CRC32C check value.
/// assert_eq!(lsm_db::format::crc32c(b"123456789"), 0xE306_9283);
/// ```
#[inline]
#[must_use]
pub fn crc32c(data: &[u8]) -> u32 {
    crate::crc::crc32c(data)
}

/// What a key maps to in a run, borrowed from the run's bytes.
///
/// A `Tombstone` records that the key was deleted. It must shadow any value for
/// the same key in an older run, so a reader stacking several runs stops at the
/// first entry it finds, tombstone or not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Entry<'a> {
    /// The key maps to this value.
    Value(&'a [u8]),
    /// The key has been deleted.
    Tombstone,
}

impl<'a> Entry<'a> {
    /// The value, or `None` for a tombstone.
    ///
    /// # Examples
    ///
    /// ```
    /// use lsm_db::format::Entry;
    /// assert_eq!(Entry::Value(b"v").value(), Some(&b"v"[..]));
    /// assert_eq!(Entry::Tombstone.value(), None);
    /// ```
    #[inline]
    #[must_use]
    pub fn value(self) -> Option<&'a [u8]> {
        match self {
            Entry::Value(v) => Some(v),
            Entry::Tombstone => None,
        }
    }

    /// Whether this entry is a tombstone.
    #[inline]
    #[must_use]
    pub fn is_tombstone(self) -> bool {
        matches!(self, Entry::Tombstone)
    }
}

/// Reject a key or value longer than the format allows.
#[cfg(feature = "alloc")]
pub(crate) fn check_lengths(key: &[u8], entry: Entry<'_>) -> Result<()> {
    if key.len() > MAX_KEY_LEN {
        return Err(Error::invalid_input("key longer than MAX_KEY_LEN (1 GiB)"));
    }
    if let Entry::Value(value) = entry {
        if value.len() > MAX_VALUE_LEN {
            return Err(Error::invalid_input(
                "value longer than MAX_VALUE_LEN (1 GiB)",
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Encoding, shared by `RunWriter` and the engine's file writer.
// ---------------------------------------------------------------------------

/// Append one data-block entry. The caller has checked the lengths with
/// [`check_lengths`], so both fit the `u32` prefixes.
#[cfg(feature = "alloc")]
pub(crate) fn encode_entry(buf: &mut Vec<u8>, key: &[u8], entry: Entry<'_>) {
    buf.extend_from_slice(&(key.len() as u32).to_le_bytes());
    buf.extend_from_slice(key);
    match entry {
        Entry::Value(value) => {
            buf.push(TAG_VALUE);
            buf.extend_from_slice(&(value.len() as u32).to_le_bytes());
            buf.extend_from_slice(value);
        }
        Entry::Tombstone => {
            buf.push(TAG_TOMBSTONE);
            buf.extend_from_slice(&0u32.to_le_bytes());
        }
    }
}

/// Append one index-block entry.
#[cfg(feature = "alloc")]
pub(crate) fn encode_index_entry(
    buf: &mut Vec<u8>,
    last_key: &[u8],
    offset: u64,
    len: u32,
    crc: u32,
) {
    buf.extend_from_slice(&(last_key.len() as u32).to_le_bytes());
    buf.extend_from_slice(last_key);
    buf.extend_from_slice(&offset.to_le_bytes());
    buf.extend_from_slice(&len.to_le_bytes());
    buf.extend_from_slice(&crc.to_le_bytes());
}

/// The 36-byte footer.
#[cfg(feature = "alloc")]
pub(crate) fn encode_footer(
    entry_count: u64,
    index_offset: u64,
    index_len: u64,
    index_crc: u32,
) -> [u8; FOOTER_LEN] {
    let mut footer = [0u8; FOOTER_LEN];
    footer[0..8].copy_from_slice(&entry_count.to_le_bytes());
    footer[8..16].copy_from_slice(&index_offset.to_le_bytes());
    footer[16..24].copy_from_slice(&index_len.to_le_bytes());
    footer[24..28].copy_from_slice(&index_crc.to_le_bytes());
    footer[28..36].copy_from_slice(MAGIC);
    footer
}

// ---------------------------------------------------------------------------
// Decoding, shared by `RunReader` and the engine's file reader.
// ---------------------------------------------------------------------------

/// A parsed footer.
#[cfg(feature = "alloc")]
#[derive(Debug, Clone, Copy)]
pub(crate) struct Footer {
    pub(crate) entry_count: u64,
    pub(crate) index_offset: u64,
    pub(crate) index_len: u64,
    pub(crate) index_crc: u32,
}

#[cfg(feature = "alloc")]
impl Footer {
    /// Parse the footer of a run that is `file_len` bytes long, checking the
    /// trailing magic and that the index block ends exactly where the footer
    /// begins.
    pub(crate) fn parse(footer: &[u8; FOOTER_LEN], file_len: u64) -> Result<Footer> {
        if &footer[28..36] != MAGIC {
            return Err(Error::corruption("bad magic; not an lsm-db sorted run"));
        }
        let parsed = Footer {
            entry_count: u64::from_le_bytes(arr(&footer[0..8])),
            index_offset: u64::from_le_bytes(arr(&footer[8..16])),
            index_len: u64::from_le_bytes(arr(&footer[16..24])),
            index_crc: u32::from_le_bytes(arr(&footer[24..28])),
        };
        if parsed.index_offset < MAGIC.len() as u64
            || parsed
                .index_offset
                .checked_add(parsed.index_len)
                .is_none_or(|end| end != file_len - FOOTER_LEN as u64)
        {
            return Err(Error::corruption(
                "index extent inconsistent with file size",
            ));
        }
        Ok(parsed)
    }
}

/// One index-block entry, borrowing its key from the index bytes.
#[cfg(feature = "alloc")]
#[derive(Debug, Clone, Copy)]
pub(crate) struct IndexEntry<'a> {
    pub(crate) last_key: &'a [u8],
    pub(crate) offset: u64,
    pub(crate) len: u32,
    pub(crate) crc: u32,
}

/// Validating iterator over an index block.
///
/// Rejects truncated entries, over-long keys, keys that are not strictly
/// increasing, and blocks that fall outside the data region
/// `[8, data_end)`. Stops after the first error.
#[cfg(feature = "alloc")]
#[derive(Debug)]
pub(crate) struct IndexEntries<'a> {
    bytes: &'a [u8],
    pos: usize,
    data_end: u64,
    prev: Option<&'a [u8]>,
    failed: bool,
}

#[cfg(feature = "alloc")]
impl<'a> IndexEntries<'a> {
    /// Iterate the index block `bytes` of a run whose data blocks end at
    /// `data_end` (the index offset).
    pub(crate) fn new(bytes: &'a [u8], data_end: u64) -> Self {
        IndexEntries {
            bytes,
            pos: 0,
            data_end,
            prev: None,
            failed: false,
        }
    }

    fn parse_next(&mut self) -> Result<IndexEntry<'a>> {
        let key_len = read_u32(self.bytes, &mut self.pos)?;
        if key_len as usize > MAX_KEY_LEN {
            return Err(Error::corruption("index key length exceeds maximum"));
        }
        let last_key = read_slice(self.bytes, &mut self.pos, key_len as usize)?;
        let offset = u64::from_le_bytes(read_array::<8>(self.bytes, &mut self.pos)?);
        let len = read_u32(self.bytes, &mut self.pos)?;
        let crc = read_u32(self.bytes, &mut self.pos)?;
        if let Some(prev) = self.prev {
            if last_key <= prev {
                return Err(Error::corruption(
                    "index block keys not strictly increasing",
                ));
            }
        }
        if offset < MAGIC.len() as u64
            || offset
                .checked_add(u64::from(len))
                .is_none_or(|end| end > self.data_end)
        {
            return Err(Error::corruption(
                "data block extent outside the data region",
            ));
        }
        self.prev = Some(last_key);
        Ok(IndexEntry {
            last_key,
            offset,
            len,
            crc,
        })
    }
}

#[cfg(feature = "alloc")]
impl<'a> Iterator for IndexEntries<'a> {
    type Item = Result<IndexEntry<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed || self.pos >= self.bytes.len() {
            return None;
        }
        let item = self.parse_next();
        self.failed = item.is_err();
        Some(item)
    }
}

/// Validating iterator over the entries of one data block, borrowing keys and
/// values from the block bytes.
///
/// Rejects truncated entries, over-long lengths, unknown tags, tombstones with
/// a value, and keys that are not strictly increasing. Stops after the first
/// error.
#[cfg(feature = "alloc")]
#[derive(Debug, Clone)]
pub(crate) struct BlockEntries<'a> {
    bytes: &'a [u8],
    pos: usize,
    prev: Option<&'a [u8]>,
    failed: bool,
}

#[cfg(feature = "alloc")]
impl<'a> BlockEntries<'a> {
    /// Iterate the data block `bytes`. The caller has verified its checksum.
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        BlockEntries {
            bytes,
            pos: 0,
            prev: None,
            failed: false,
        }
    }

    fn parse_next(&mut self) -> Result<(&'a [u8], Entry<'a>)> {
        let key_len = read_u32(self.bytes, &mut self.pos)?;
        if key_len as usize > MAX_KEY_LEN {
            return Err(Error::corruption("key length exceeds maximum"));
        }
        let key = read_slice(self.bytes, &mut self.pos, key_len as usize)?;
        let tag = read_array::<1>(self.bytes, &mut self.pos)?[0];
        let val_len = read_u32(self.bytes, &mut self.pos)?;
        if val_len as usize > MAX_VALUE_LEN {
            return Err(Error::corruption("value length exceeds maximum"));
        }
        let entry = match tag {
            TAG_VALUE => Entry::Value(read_slice(self.bytes, &mut self.pos, val_len as usize)?),
            TAG_TOMBSTONE => {
                if val_len != 0 {
                    return Err(Error::corruption("tombstone with non-zero value length"));
                }
                Entry::Tombstone
            }
            _ => return Err(Error::corruption("unknown record tag")),
        };
        if let Some(prev) = self.prev {
            if key <= prev {
                return Err(Error::corruption("data block keys not strictly increasing"));
            }
        }
        self.prev = Some(key);
        Ok((key, entry))
    }
}

#[cfg(feature = "alloc")]
impl<'a> Iterator for BlockEntries<'a> {
    type Item = Result<(&'a [u8], Entry<'a>)>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.failed || self.pos >= self.bytes.len() {
            return None;
        }
        let item = self.parse_next();
        self.failed = item.is_err();
        Some(item)
    }
}

#[cfg(feature = "alloc")]
#[inline]
fn read_u32(bytes: &[u8], pos: &mut usize) -> Result<u32> {
    Ok(u32::from_le_bytes(read_array::<4>(bytes, pos)?))
}

#[cfg(feature = "alloc")]
#[inline]
fn read_array<const N: usize>(bytes: &[u8], pos: &mut usize) -> Result<[u8; N]> {
    let slice = read_slice(bytes, pos, N)?;
    let mut out = [0u8; N];
    out.copy_from_slice(slice);
    Ok(out)
}

#[cfg(feature = "alloc")]
#[inline]
fn read_slice<'a>(bytes: &'a [u8], pos: &mut usize, len: usize) -> Result<&'a [u8]> {
    let end = pos
        .checked_add(len)
        .ok_or_else(|| Error::corruption("record extent overflows"))?;
    let slice = bytes
        .get(*pos..end)
        .ok_or_else(|| Error::corruption("record truncated"))?;
    *pos = end;
    Ok(slice)
}

#[cfg(feature = "alloc")]
#[inline]
fn arr<const N: usize>(s: &[u8]) -> [u8; N] {
    let mut a = [0u8; N];
    a.copy_from_slice(s);
    a
}

// ---------------------------------------------------------------------------
// Public codec.
// ---------------------------------------------------------------------------

/// Builds a sorted run in memory.
///
/// Add entries with [`put`](RunWriter::put) and
/// [`delete`](RunWriter::delete) in strictly ascending key order, then call
/// [`finish`](RunWriter::finish) for the encoded run. The bytes are exactly
/// what the engine writes for the same entries, so a finished run can be saved
/// as a file and read by any 1.x release.
///
/// # Examples
///
/// ```
/// use lsm_db::format::{RunReader, RunWriter};
///
/// let mut writer = RunWriter::new();
/// for i in 0..100u32 {
///     writer.put(format!("key{i:03}"), i.to_le_bytes())?;
/// }
/// let bytes = writer.finish();
/// assert_eq!(RunReader::new(&bytes)?.entry_count(), 100);
/// # Ok::<(), lsm_db::Error>(())
/// ```
#[cfg(feature = "alloc")]
#[derive(Debug, Clone)]
pub struct RunWriter {
    out: Vec<u8>,
    block_start: usize,
    index: Vec<u8>,
    last_key: Vec<u8>,
    entry_count: u64,
}

#[cfg(feature = "alloc")]
impl Default for RunWriter {
    fn default() -> Self {
        RunWriter::new()
    }
}

#[cfg(feature = "alloc")]
impl RunWriter {
    /// Start an empty run.
    #[must_use]
    pub fn new() -> Self {
        let mut out = Vec::with_capacity(TARGET_BLOCK_SIZE + 256);
        out.extend_from_slice(MAGIC);
        RunWriter {
            out,
            block_start: MAGIC.len(),
            index: Vec::new(),
            last_key: Vec::new(),
            entry_count: 0,
        }
    }

    /// Add `key` with `value`.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidInput`] if `key` is not strictly greater than the
    /// previous key, or if `key` or `value` exceeds [`MAX_KEY_LEN`] /
    /// [`MAX_VALUE_LEN`]. The writer is unchanged when an error is returned.
    pub fn put(&mut self, key: impl AsRef<[u8]>, value: impl AsRef<[u8]>) -> Result<()> {
        self.push(key.as_ref(), Entry::Value(value.as_ref()))
    }

    /// Add a tombstone for `key`, recording that it was deleted.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidInput`] if `key` is not strictly greater than the
    /// previous key or exceeds [`MAX_KEY_LEN`]. The writer is unchanged when an
    /// error is returned.
    pub fn delete(&mut self, key: impl AsRef<[u8]>) -> Result<()> {
        self.push(key.as_ref(), Entry::Tombstone)
    }

    /// The number of entries (values and tombstones) added so far.
    #[inline]
    #[must_use]
    pub fn entry_count(&self) -> u64 {
        self.entry_count
    }

    fn push(&mut self, key: &[u8], entry: Entry<'_>) -> Result<()> {
        check_lengths(key, entry)?;
        if self.entry_count > 0 && key <= self.last_key.as_slice() {
            return Err(Error::invalid_input(
                "keys must be added in strictly ascending order",
            ));
        }
        encode_entry(&mut self.out, key, entry);
        self.last_key.clear();
        self.last_key.extend_from_slice(key);
        self.entry_count += 1;
        if self.out.len() - self.block_start >= TARGET_BLOCK_SIZE {
            self.close_block();
        }
        Ok(())
    }

    /// Record the open block in the index.
    fn close_block(&mut self) {
        let block = &self.out[self.block_start..];
        if block.is_empty() {
            return;
        }
        // A block is under 4 KiB plus one entry of at most 2 GiB + 9 bytes, so
        // its length fits a `u32`.
        let len = block.len() as u32;
        let crc = crc32c(block);
        encode_index_entry(
            &mut self.index,
            &self.last_key,
            self.block_start as u64,
            len,
            crc,
        );
        self.block_start = self.out.len();
    }

    /// Close the last block, append the index and footer, and return the run.
    #[must_use]
    pub fn finish(mut self) -> Vec<u8> {
        self.close_block();
        let index_offset = self.out.len() as u64;
        let index_crc = crc32c(&self.index);
        self.out.reserve(self.index.len() + FOOTER_LEN);
        self.out.extend_from_slice(&self.index);
        self.out.extend_from_slice(&encode_footer(
            self.entry_count,
            index_offset,
            self.index.len() as u64,
            index_crc,
        ));
        self.out
    }
}

/// A validated sorted run held in memory.
///
/// [`new`](RunReader::new) checks the header and footer magic, the index
/// extent, the index checksum, and every index entry. Each data block's
/// checksum is verified when the block is read, by [`get`](RunReader::get) or
/// [`iter`](RunReader::iter). Keys and values are returned as slices of the
/// original buffer; nothing is copied.
///
/// # Examples
///
/// ```
/// use lsm_db::format::{Entry, RunReader, RunWriter};
///
/// let mut writer = RunWriter::new();
/// writer.put(b"a", b"1")?;
/// writer.put(b"b", b"2")?;
/// let bytes = writer.finish();
///
/// let run = RunReader::new(&bytes)?;
/// for item in run.iter() {
///     let (key, entry) = item?;
///     assert!(matches!(entry, Entry::Value(_)), "{key:?}");
/// }
/// # Ok::<(), lsm_db::Error>(())
/// ```
#[cfg(feature = "alloc")]
#[derive(Debug, Clone)]
pub struct RunReader<'a> {
    bytes: &'a [u8],
    index: Vec<IndexEntry<'a>>,
    entry_count: u64,
}

#[cfg(feature = "alloc")]
impl<'a> RunReader<'a> {
    /// Validate `bytes` as a sorted run and parse its index.
    ///
    /// # Errors
    ///
    /// [`Error::Corruption`] if the buffer is too short, either magic is wrong,
    /// the index extent does not match the buffer, the index checksum does not
    /// match, or an index entry is malformed, out of order, or points outside
    /// the data region.
    pub fn new(bytes: &'a [u8]) -> Result<Self> {
        if bytes.len() < MAGIC.len() + FOOTER_LEN {
            return Err(Error::corruption("file shorter than header and footer"));
        }
        if &bytes[..MAGIC.len()] != MAGIC {
            return Err(Error::corruption("bad magic; not an lsm-db sorted run"));
        }
        let file_len = bytes.len() as u64;
        let footer = Footer::parse(&arr(&bytes[bytes.len() - FOOTER_LEN..]), file_len)?;
        // `Footer::parse` checked that the index lies inside the buffer, so
        // both bounds fit `usize`.
        let start = footer.index_offset as usize;
        let index_bytes = &bytes[start..bytes.len() - FOOTER_LEN];
        if crc32c(index_bytes) != footer.index_crc {
            return Err(Error::corruption("index checksum mismatch"));
        }
        let index =
            IndexEntries::new(index_bytes, footer.index_offset).collect::<Result<Vec<_>>>()?;
        Ok(RunReader {
            bytes,
            index,
            entry_count: footer.entry_count,
        })
    }

    /// The entry count recorded in the footer (values and tombstones).
    ///
    /// The format treats this as informational; [`iter`](RunReader::iter) is
    /// the authority on what the run holds.
    #[inline]
    #[must_use]
    pub fn entry_count(&self) -> u64 {
        self.entry_count
    }

    /// Look up `key`, reading and verifying the one block that could hold it.
    ///
    /// Returns `None` if the run has no entry for `key`. A returned
    /// [`Entry::Tombstone`] means the key was deleted in this run.
    ///
    /// # Errors
    ///
    /// [`Error::Corruption`] if the candidate block fails its checksum or is
    /// malformed.
    pub fn get(&self, key: &[u8]) -> Result<Option<Entry<'a>>> {
        let i = self.index.partition_point(|h| h.last_key < key);
        let Some(handle) = self.index.get(i) else {
            return Ok(None);
        };
        for item in BlockEntries::new(self.block(handle)?) {
            let (k, entry) = item?;
            match k.cmp(key) {
                core::cmp::Ordering::Less => {}
                core::cmp::Ordering::Equal => return Ok(Some(entry)),
                core::cmp::Ordering::Greater => break,
            }
        }
        Ok(None)
    }

    /// Iterate every entry in ascending key order.
    ///
    /// Each block's checksum is verified as the iterator reaches it. On the
    /// first error the iterator yields it and then ends.
    #[must_use]
    pub fn iter(&self) -> RunIter<'a, '_> {
        RunIter {
            run: self,
            next_block: 0,
            block: None,
            prev: None,
            done: false,
        }
    }

    /// The bytes of the block `handle` describes, checksum verified.
    fn block(&self, handle: &IndexEntry<'a>) -> Result<&'a [u8]> {
        // `IndexEntries` checked the extent lies inside the data region.
        let start = handle.offset as usize;
        let block = &self.bytes[start..start + handle.len as usize];
        if crc32c(block) != handle.crc {
            return Err(Error::corruption("data block checksum mismatch"));
        }
        Ok(block)
    }
}

#[cfg(feature = "alloc")]
impl<'a, 'r> IntoIterator for &'r RunReader<'a> {
    type Item = Result<(&'a [u8], Entry<'a>)>;
    type IntoIter = RunIter<'a, 'r>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// In-order iterator over a [`RunReader`], from [`RunReader::iter`].
///
/// Yields `(key, entry)` pairs borrowed from the run's buffer. Besides each
/// block's own checks, it verifies that keys keep increasing across blocks and
/// that every block ends with the key the index records for it.
#[cfg(feature = "alloc")]
#[derive(Debug, Clone)]
pub struct RunIter<'a, 'r> {
    run: &'r RunReader<'a>,
    next_block: usize,
    block: Option<BlockEntries<'a>>,
    prev: Option<&'a [u8]>,
    done: bool,
}

#[cfg(feature = "alloc")]
impl<'a> RunIter<'a, '_> {
    fn step(&mut self) -> Option<Result<(&'a [u8], Entry<'a>)>> {
        loop {
            if let Some(block) = &mut self.block {
                match block.next() {
                    Some(Ok((key, entry))) => {
                        if self.prev.is_some_and(|p| key <= p) {
                            return Some(Err(Error::corruption(
                                "keys not strictly increasing across blocks",
                            )));
                        }
                        self.prev = Some(key);
                        return Some(Ok((key, entry)));
                    }
                    Some(Err(e)) => return Some(Err(e)),
                    None => {
                        let expected = self.run.index[self.next_block - 1].last_key;
                        if self.prev != Some(expected) {
                            return Some(Err(Error::corruption(
                                "block's last key does not match the index",
                            )));
                        }
                        self.block = None;
                    }
                }
            }
            let handle = self.run.index.get(self.next_block)?;
            match self.run.block(handle) {
                Ok(bytes) => {
                    self.block = Some(BlockEntries::new(bytes));
                    self.next_block += 1;
                }
                Err(e) => return Some(Err(e)),
            }
        }
    }
}

#[cfg(feature = "alloc")]
impl<'a> Iterator for RunIter<'a, '_> {
    type Item = Result<(&'a [u8], Entry<'a>)>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let item = self.step();
        self.done = !matches!(item, Some(Ok(_)));
        item
    }
}

#[cfg(feature = "alloc")]
impl core::iter::FusedIterator for RunIter<'_, '_> {}

#[cfg(all(test, feature = "alloc"))]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use alloc::format;
    use alloc::vec;

    fn build(entries: &[(&[u8], Option<&[u8]>)]) -> Vec<u8> {
        let mut w = RunWriter::new();
        for (k, v) in entries {
            match v {
                Some(v) => w.put(k, v).unwrap(),
                None => w.delete(k).unwrap(),
            }
        }
        w.finish()
    }

    fn many(n: u32, value_len: usize) -> Vec<u8> {
        let mut w = RunWriter::new();
        for i in 0..n {
            w.put(format!("key{i:06}"), vec![b'x'; value_len]).unwrap();
        }
        w.finish()
    }

    #[test]
    fn test_empty_run_roundtrip() {
        let bytes = RunWriter::new().finish();
        assert_eq!(bytes.len(), MAGIC.len() + FOOTER_LEN);
        let run = RunReader::new(&bytes).unwrap();
        assert_eq!(run.entry_count(), 0);
        assert_eq!(run.get(b"anything").unwrap(), None);
        assert_eq!(run.iter().count(), 0);
    }

    #[test]
    fn test_values_and_tombstones_roundtrip() {
        let bytes = build(&[
            (b"", Some(b"empty key")),
            (b"a", Some(b"1")),
            (b"b", None),
            (b"c", Some(b"")),
        ]);
        let run = RunReader::new(&bytes).unwrap();
        assert_eq!(run.entry_count(), 4);
        assert_eq!(run.get(b"").unwrap(), Some(Entry::Value(b"empty key")));
        assert_eq!(run.get(b"a").unwrap(), Some(Entry::Value(b"1")));
        assert_eq!(run.get(b"b").unwrap(), Some(Entry::Tombstone));
        assert_eq!(run.get(b"c").unwrap(), Some(Entry::Value(b"")));
        assert_eq!(run.get(b"bb").unwrap(), None);
        assert_eq!(run.get(b"d").unwrap(), None);
        let all: Vec<_> = run.iter().map(Result::unwrap).collect();
        assert_eq!(
            all,
            vec![
                (&b""[..], Entry::Value(b"empty key")),
                (&b"a"[..], Entry::Value(b"1")),
                (&b"b"[..], Entry::Tombstone),
                (&b"c"[..], Entry::Value(b"")),
            ]
        );
    }

    #[test]
    fn test_multi_block_roundtrip() {
        let bytes = many(5_000, 200);
        let run = RunReader::new(&bytes).unwrap();
        assert!(run.index.len() > 1, "expected several blocks");
        assert_eq!(run.iter().count(), 5_000);
        assert!(run.iter().all(|r| r.is_ok()));
        for i in [0u32, 1, 2_500, 4_999] {
            let key = format!("key{i:06}");
            assert_eq!(
                run.get(key.as_bytes()).unwrap(),
                Some(Entry::Value(&[b'x'; 200]))
            );
        }
        assert_eq!(run.get(b"key005000").unwrap(), None);
        assert_eq!(run.get(b"a").unwrap(), None);
        // `&RunReader` iterates too.
        let mut n = 0;
        for item in &run {
            let _ = item.unwrap();
            n += 1;
        }
        assert_eq!(n, 5_000);
    }

    #[test]
    fn test_large_value_gets_its_own_block() {
        let big = vec![0xABu8; 100_000];
        let mut w = RunWriter::new();
        w.put(b"a", b"small").unwrap();
        w.put(b"b", &big).unwrap();
        w.put(b"c", b"small").unwrap();
        let bytes = w.finish();
        let run = RunReader::new(&bytes).unwrap();
        assert_eq!(run.get(b"b").unwrap(), Some(Entry::Value(&big)));
        assert_eq!(run.iter().count(), 3);
    }

    #[test]
    fn test_writer_rejects_out_of_order_and_duplicate_keys() {
        let mut w = RunWriter::new();
        w.put(b"b", b"1").unwrap();
        assert!(matches!(w.put(b"b", b"2"), Err(Error::InvalidInput { .. })));
        assert!(matches!(w.delete(b"a"), Err(Error::InvalidInput { .. })));
        // A rejected call leaves the writer usable and unchanged.
        assert_eq!(w.entry_count(), 1);
        w.put(b"c", b"3").unwrap();
        let bytes = w.finish();
        assert_eq!(RunReader::new(&bytes).unwrap().iter().count(), 2);
    }

    #[test]
    fn test_check_lengths_enforces_the_format_limits() {
        // Exercised without allocating 1 GiB: the slice lengths are what matter.
        let key = vec![0u8; 16];
        assert!(check_lengths(&key, Entry::Tombstone).is_ok());
        assert!(check_lengths(&key, Entry::Value(&[])).is_ok());
        let long = vec![0u8; MAX_KEY_LEN + 1];
        assert!(matches!(
            check_lengths(&long, Entry::Tombstone),
            Err(Error::InvalidInput { .. })
        ));
        assert!(matches!(
            check_lengths(b"k", Entry::Value(&long)),
            Err(Error::InvalidInput { .. })
        ));
        assert!(check_lengths(&long[..MAX_KEY_LEN], Entry::Value(&long[..MAX_VALUE_LEN])).is_ok());
    }

    #[test]
    fn test_short_buffer_is_corruption() {
        for len in 0..MAGIC.len() + FOOTER_LEN {
            let bytes = vec![0u8; len];
            assert!(matches!(
                RunReader::new(&bytes),
                Err(Error::Corruption { .. })
            ));
        }
    }

    #[test]
    fn test_bad_header_magic_is_corruption() {
        let mut bytes = many(10, 8);
        bytes[0] ^= 0xFF;
        assert!(matches!(
            RunReader::new(&bytes),
            Err(Error::Corruption { .. })
        ));
    }

    #[test]
    fn test_bad_footer_magic_is_corruption() {
        let mut bytes = many(10, 8);
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        assert!(matches!(
            RunReader::new(&bytes),
            Err(Error::Corruption { .. })
        ));
    }

    #[test]
    fn test_truncated_or_extended_buffer_is_corruption() {
        let bytes = many(100, 16);
        assert!(RunReader::new(&bytes[..bytes.len() - 1]).is_err());
        let mut longer = bytes.clone();
        longer.push(0);
        assert!(RunReader::new(&longer).is_err());
    }

    #[test]
    fn test_index_bit_flip_is_corruption() {
        let bytes = many(1_000, 32);
        let run = RunReader::new(&bytes).unwrap();
        let index_start = run.index[0].offset as usize; // first block, not the index
        assert_eq!(index_start, MAGIC.len());
        let footer =
            Footer::parse(&arr(&bytes[bytes.len() - FOOTER_LEN..]), bytes.len() as u64).unwrap();
        let mut bad = bytes.clone();
        bad[footer.index_offset as usize + 2] ^= 0x01;
        assert!(matches!(
            RunReader::new(&bad),
            Err(Error::Corruption { .. })
        ));
    }

    #[test]
    fn test_data_bit_flip_is_caught_by_get_and_iter() {
        let mut bytes = many(1_000, 32);
        bytes[MAGIC.len() + 3] ^= 0x10; // inside the first data block
        let run = RunReader::new(&bytes).unwrap(); // index is intact
        assert!(matches!(
            run.get(b"key000000"),
            Err(Error::Corruption { .. })
        ));
        let mut it = run.iter();
        assert!(matches!(it.next(), Some(Err(Error::Corruption { .. }))));
        assert!(it.next().is_none(), "iterator ends after an error");
        // Blocks other than the damaged one still read.
        assert!(run.get(b"key000999").unwrap().is_some());
    }

    #[test]
    fn test_every_single_byte_corruption_is_detected_or_harmless() {
        let bytes = many(300, 24);
        let clean: Vec<(Vec<u8>, Option<Vec<u8>>)> = RunReader::new(&bytes)
            .unwrap()
            .iter()
            .map(|r| {
                let (k, e) = r.unwrap();
                (k.to_vec(), e.value().map(<[u8]>::to_vec))
            })
            .collect();
        for i in 0..bytes.len() {
            let mut bad = bytes.clone();
            bad[i] ^= 0x5A;
            let Ok(run) = RunReader::new(&bad) else {
                continue;
            };
            let read: core::result::Result<Vec<_>, _> = run
                .iter()
                .map(|r| r.map(|(k, e)| (k.to_vec(), e.value().map(<[u8]>::to_vec))))
                .collect();
            if let Ok(read) = read {
                // Only the informational entry count may change undetected.
                assert_eq!(read, clean, "byte {i} changed the data without an error");
            }
        }
    }

    #[test]
    fn test_out_of_range_block_extent_is_corruption() {
        // A hand-built run whose single index entry points past the data
        // region, with every checksum valid.
        let mut out = MAGIC.to_vec();
        let mut block = Vec::new();
        encode_entry(&mut block, b"k", Entry::Value(b"v"));
        out.extend_from_slice(&block);
        let index_offset = out.len() as u64;
        let mut index = Vec::new();
        encode_index_entry(&mut index, b"k", 8, block.len() as u32 + 1, crc32c(&block));
        out.extend_from_slice(&index);
        out.extend_from_slice(&encode_footer(
            1,
            index_offset,
            index.len() as u64,
            crc32c(&index),
        ));
        assert!(matches!(
            RunReader::new(&out),
            Err(Error::Corruption { .. })
        ));
    }

    #[test]
    fn test_index_last_key_mismatch_is_caught_by_iter() {
        // The block holds "a" but the index claims its last key is "b".
        let mut out = MAGIC.to_vec();
        let mut block = Vec::new();
        encode_entry(&mut block, b"a", Entry::Value(b"v"));
        out.extend_from_slice(&block);
        let index_offset = out.len() as u64;
        let mut index = Vec::new();
        encode_index_entry(&mut index, b"b", 8, block.len() as u32, crc32c(&block));
        out.extend_from_slice(&index);
        out.extend_from_slice(&encode_footer(
            1,
            index_offset,
            index.len() as u64,
            crc32c(&index),
        ));
        let run = RunReader::new(&out).unwrap();
        let items: Vec<_> = run.iter().collect();
        assert!(matches!(
            items.as_slice(),
            [Ok(_), Err(Error::Corruption { .. })]
        ));
    }

    #[test]
    fn test_entry_helpers() {
        assert_eq!(Entry::Value(b"x").value(), Some(&b"x"[..]));
        assert!(!Entry::Value(b"x").is_tombstone());
        assert_eq!(Entry::Tombstone.value(), None);
        assert!(Entry::Tombstone.is_tombstone());
    }
}
