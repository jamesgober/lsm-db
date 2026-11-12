<h1 align="center">
    <img width="99" alt="Rust logo" src="https://raw.githubusercontent.com/jamesgober/rust-collection/72baabd71f00e14aa9184efcb16fa3deddda3a0a/assets/rust-logo.svg">
    <br><b>lsm-db</b><br>
    <sub><sup>SORTED-RUN FORMAT — NORMATIVE SPECIFICATION</sup></sub>
</h1>
<div align="center">
    <sup>
        <a href="../README.md" title="Project Home"><b>HOME</b></a>
        <span>&nbsp;│&nbsp;</span>
        <a href="./API.md" title="API Reference"><b>API</b></a>
        <span>&nbsp;│&nbsp;</span>
        <span>FORMAT</span>
    </sup>
</div>
<br>

> This document specifies the on-disk byte layout of an `lsm-db` **sorted run**
> (SSTable) and the **manifest**, at format version **1**.
>
> **Stability.** Format v1 is **frozen for the 1.x series**: a `1.x` release
> reads any run or manifest written by any other `1.x` release. A format change
> requires a new version tag in the file and is a major-version event. The key
> words MUST, MUST NOT, SHOULD, and MAY are used as in RFC 2119.
>
> **Implementations.** The engine reads and writes runs as files.
> `lsm_db::format::{RunWriter, RunReader}` (since 1.1, available without `std`)
> write and read the same bytes in memory; `RunWriter` output is byte-identical
> to the engine's for the same entries.

## Table of Contents

- [Conventions](#conventions)
- [Sorted run](#sorted-run)
  - [Header](#header)
  - [Data blocks](#data-blocks)
  - [Index block](#index-block)
  - [Footer](#footer)
  - [Footer check](#footer-check)
  - [Reading a run](#reading-a-run)
  - [Integrity](#integrity)
- [Manifest](#manifest)
- [Directory layout & recovery](#directory-layout--recovery)

---

## Conventions

- All multi-byte integers are **little-endian**, unsigned.
- `u8`, `u32`, `u64` denote 1-, 4-, and 8-byte integers.
- Offsets and lengths are byte counts from the start of the file.
- Checksums are **CRC32C** (Castagnoli): polynomial `0x1EDC6F41` (reflected
  `0x82F63B78`), input and output reflected, initial value `0xFFFFFFFF`, final
  XOR `0xFFFFFFFF`. The check value of the ASCII string `123456789` is
  `0xE3069283`. This is the checksum of iSCSI (RFC 3720) and of the `crc32c`
  crate; `lsm_db::format::crc32c` computes it in every build, including
  `no_std`.
- A *key* and a *value* are arbitrary byte strings. Within a run, keys are
  **unique** and appear in **strictly ascending** lexicographic (`memcmp`) order.

---

## Sorted run

A sorted run is one immutable file. Its structure, in file order:

```text
┌────────────────────────────────┐  offset 0
│ Header        (8 bytes)        │
├────────────────────────────────┤  offset 8
│ Data block 0                   │
│ Data block 1                   │
│ …                              │
├────────────────────────────────┤  index_offset − 12
│ Footer check  (12 bytes)       │  written since 1.4; see below
├────────────────────────────────┤  index_offset
│ Index block                    │
├────────────────────────────────┤  index_offset + index_len
│ Footer        (36 bytes)       │
└────────────────────────────────┘  end of file
```

### Header

| Field | Type | Value |
|-------|------|-------|
| `magic` | `[u8; 8]` | ASCII `"LSMTBL01"` (`4C 53 4D 54 42 4C 30 31`) |

The header MUST be exactly these 8 bytes. The same 8 bytes also terminate the
footer, so truncation that removes the footer is detectable.

### Data blocks

The key/value data is partitioned into one or more **data blocks**, laid out
contiguously after the header. A block is a sequence of **entries**; a reader
locates the block that may contain a key from the index, then scans the block.

A block holds entries until adding the next entry would make it exceed the target
block size (the reference writer uses 4096 bytes). A single entry larger than the
target becomes a block of its own; an entry MUST NOT be split across blocks.
Blocks partition the key space: every key in block *i* is less than every key in
block *i+1*.

Each **entry** is:

| Field | Type | Description |
|-------|------|-------------|
| `key_len` | `u32` | Length of `key` in bytes. MUST be ≤ 2³⁰. |
| `key` | `[u8; key_len]` | The key. |
| `tag` | `u8` | `0` = value, `1` = tombstone. Other values are invalid. |
| `value_len` | `u32` | Length of `value`. MUST be ≤ 2³⁰. MUST be `0` when `tag = 1`. |
| `value` | `[u8; value_len]` | The value. Absent (zero-length) for a tombstone. |

A **tombstone** records a deletion. Tombstones MAY appear in runs produced by a
flush. A run produced by a full compaction (every live run merged into one) MUST
NOT contain tombstones — with no older run left to mask, a deletion is resolved
by omitting the key.

### Index block

The index block follows the last data block and has one **index entry** per data
block, in ascending order:

| Field | Type | Description |
|-------|------|-------------|
| `last_key_len` | `u32` | Length of `last_key`. MUST be ≤ 2³⁰. |
| `last_key` | `[u8; last_key_len]` | The **last** (largest) key in the block. |
| `block_offset` | `u64` | Offset of the block from the start of the file. |
| `block_len` | `u32` | Length of the block in bytes. |
| `block_crc` | `u32` | CRC32C of the block's bytes. |

Index entries MUST be ordered so that each `last_key` is strictly greater than
the previous. An empty run (no entries) has an empty index block (`index_len = 0`).

### Footer

The footer is the final **36 bytes** of the file:

| Field | Type | Description |
|-------|------|-------------|
| `entry_count` | `u64` | Total number of entries (values + tombstones). See below. |
| `index_offset` | `u64` | Offset of the index block. |
| `index_len` | `u64` | Length of the index block in bytes. |
| `index_crc` | `u32` | CRC32C of the index block's bytes. |
| `magic` | `[u8; 8]` | ASCII `"LSMTBL01"`, identical to the header. |

`index_offset + index_len` MUST equal `file_len − 36`. `index_offset` MUST be
≥ 8 (past the header).

`entry_count` MUST be at least the number of data blocks (a block holds at
least one entry) and at most the data blocks' total length divided by 9 (the
smallest entry: a key length, a tag, and a value length, with no key or value
bytes). A reader MUST treat any other count as corruption; since 1.4 the
reference reader does. A count is never used to size an allocation before it
is checked.

### Footer check

Since 1.4 the reference writers put a **footer check** in the 12 bytes between
the end of the last data block and the index block:

| Field | Type | Description |
|-------|------|-------------|
| `check_magic` | `[u8; 8]` | ASCII `"LSMFCK01"` (`4C 53 4D 46 43 4B 30 31`). |
| `footer_crc` | `u32` | CRC32C of the 36 footer bytes. |

A run has a footer check when `index_offset` is exactly 12 bytes past the end
of its last data block (offset 8 when there are no blocks) and those 12 bytes
begin with `check_magic`. A reader MUST then verify `footer_crc` and treat a
mismatch as corruption. A run without one (any run written before 1.4) is read
as before; its `entry_count` is still checked as above.

The check is additive: no data block covers those 12 bytes, so a reader that
predates it reads a run that has it, and the format version stays 1. Readers
from 1.0 to 1.3 read runs written by 1.4, and 1.4 reads theirs.

### Reading a run

A reader MUST:

1. Verify the file is at least `8 + 36` bytes and begins with the header magic.
2. Read the 36-byte footer and verify the trailing `magic`.
3. Read the index block at `[index_offset, index_offset + index_len)` and verify
   its CRC32C equals `index_crc`.
4. Parse the index into block handles, rejecting non-increasing `last_key`s and
   any block whose extent `[block_offset, block_offset + block_len)` is not
   inside the data region `[8, index_offset)`.
5. If the run has a footer check, verify it; in every run, check `entry_count`
   against the blocks.

To look up a key, binary-search the index for the first block whose `last_key`
≥ the key, read that block, verify its `block_crc`, and scan it. To iterate,
read blocks in order, checking that each block ends with the `last_key` the
index records for it and that its first key is greater than the previous
block's `last_key`. `entry_count` is not required for reading.

### Integrity

Every data block is covered by a CRC32C in the index, the index by a CRC32C in
the footer, and (since 1.4) the footer by the CRC32C in its footer check. A
reader MUST treat a checksum mismatch, an out-of-range offset or length, an
unknown `tag`, a non-zero `value_len` on a tombstone, non-increasing keys, or an
`entry_count` that does not fit the blocks as **corruption** and MUST NOT return
fabricated or partial data for the affected block.

These checksums detect damage. They are not authentication: anyone who can
write a run can compute them.

---

## Manifest

The manifest names the live runs and the next sequence number. It is a UTF-8,
line-oriented text file named `MANIFEST`:

```text
LSMDB-MANIFEST v1
next_seq=<u64>
<run filename>      # newest live run
<run filename>
…                   # oldest live run
```

- Line 1 MUST be exactly `LSMDB-MANIFEST v1`.
- Line 2 MUST be `next_seq=` followed by a base-10 `u64`: the next run sequence
  number to allocate.
- Each remaining non-empty line is a run filename, listed **newest first**. This
  order is the recency order reads and merges rely on. A name MUST be exactly
  `run-<seq>.sst` as defined below, with no path separator; a reader MUST treat
  any other line as corruption (the reference reader does since 1.4).
- A name MUST NOT appear twice.

Run files are named `run-<seq>.sst`, where `<seq>` is the zero-padded, 10-digit
decimal sequence number (e.g. `run-0000000042.sst`). The manifest is the sole
authority on which runs are live and in what order; a run's filename does not by
itself imply liveness or recency.

The manifest is rewritten on every flush and compaction by writing
`MANIFEST.tmp`, `fsync`ing it, and renaming it over `MANIFEST`. The rename is the
atomic commit point.

---

## Directory layout & recovery

A database is a directory containing:

- `MANIFEST` — the live run list (above).
- `run-<seq>.sst` — sorted runs.
- `run-<seq>.sst.bloom`: a run's bloom filter (`bloom` feature); a hint only.
- `wal.log`: the write-ahead log (`durability` feature).
- `LOCK`: held by the open handle (since 1.4). Its content is the owner's
  process id, for information.
- `MANIFEST.old`: a manifest replaced by a rebuild (since 1.4).
- `*.tmp` — transient files mid-write; never live.

Every store writes its `MANIFEST` when it is created (since 1.3). On open, an
implementation MUST:

1. Take the directory lock, and fail if another handle holds it.
2. Load `MANIFEST`. Only a manifest that does not exist is missing; any other
   failure to read it is an error. When it is missing, the directory is a new,
   empty database only if it holds nothing but `LOCK`, `*.tmp` files, and
   `wal.log` (a store from before 1.3 that never flushed may have only a log);
   run files without a manifest are corruption, and any other content refuses
   the open. In both cases nothing is deleted.
3. Open every run the manifest names, in order. A named run that is missing or
   fails its integrity checks is corruption.
4. Delete every `*.tmp` file and every `run-<seq>.sst` **not** named by the
   manifest — these are orphans from a flush or compaction interrupted before its
   commit. The committed state is exactly what the manifest names. This step
   runs only after steps 2 and 3 succeeded.
5. Set the next sequence number to at least one past the highest `<seq>` seen on
   disk.

A lost or damaged manifest can be written again from the runs present
(`Lsm::rebuild_manifest`): the runs are ordered newest first by `<seq>`, which
is the recency order, because a flush or compaction takes its sequence number
when it starts and a compaction merges every run that exists at that moment.

Because both the run files and the manifest are installed by atomic rename, a
crash at any point leaves either the previous committed state or the next one,
never a torn mixture: a half-written run or manifest is a `*.tmp` file that step
3 reclaims, and a finished-but-uncommitted run is an orphan that step 3 reclaims.

---

<sub>Copyright &copy; 2026 <strong>James Gober</strong>. All rights reserved.</sub>
