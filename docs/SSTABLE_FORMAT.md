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

## Table of Contents

- [Conventions](#conventions)
- [Sorted run](#sorted-run)
  - [Header](#header)
  - [Data blocks](#data-blocks)
  - [Index block](#index-block)
  - [Footer](#footer)
  - [Reading a run](#reading-a-run)
  - [Integrity](#integrity)
- [Manifest](#manifest)
- [Directory layout & recovery](#directory-layout--recovery)

---

## Conventions

- All multi-byte integers are **little-endian**, unsigned.
- `u8`, `u32`, `u64` denote 1-, 4-, and 8-byte integers.
- Offsets and lengths are byte counts from the start of the file.
- Checksums are **CRC32C** (Castagnoli polynomial, `0x1EDC6F41`), as produced by
  the `crc32c` crate.
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
| `entry_count` | `u64` | Total number of entries (values + tombstones). Informational. |
| `index_offset` | `u64` | Offset of the index block. |
| `index_len` | `u64` | Length of the index block in bytes. |
| `index_crc` | `u32` | CRC32C of the index block's bytes. |
| `magic` | `[u8; 8]` | ASCII `"LSMTBL01"`, identical to the header. |

`index_offset + index_len` MUST equal `file_len − 36`. `index_offset` MUST be
≥ 8 (past the header).

### Reading a run

A reader MUST:

1. Verify the file is at least `8 + 36` bytes.
2. Read the 36-byte footer and verify the trailing `magic`.
3. Read the index block at `[index_offset, index_offset + index_len)` and verify
   its CRC32C equals `index_crc`.
4. Parse the index into block handles, rejecting non-increasing `last_key`s.

To look up a key, binary-search the index for the first block whose `last_key`
≥ the key, read that block, verify its `block_crc`, and scan it. To iterate,
read blocks in order. `entry_count` is not required for reading and a reader MAY
ignore it.

### Integrity

Every data block is covered by a CRC32C in the index, and the index by a CRC32C
in the footer. A reader MUST treat a checksum mismatch, an out-of-range offset or
length, an unknown `tag`, a non-zero `value_len` on a tombstone, or
non-increasing keys as **corruption** and MUST NOT return fabricated or partial
data for the affected block.

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
  order is the recency order reads and merges rely on.

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
- `*.tmp` — transient files mid-write; never live.

On open, an implementation MUST:

1. Load `MANIFEST` (a missing manifest means an empty database).
2. Open every run it names, in order. A named run that is missing or fails its
   integrity checks is corruption.
3. Delete every `*.tmp` file and every `run-<seq>.sst` **not** named by the
   manifest — these are orphans from a flush or compaction interrupted before its
   commit. The committed state is exactly what the manifest names.
4. Set the next sequence number to at least one past the highest `<seq>` seen on
   disk.

Because both the run files and the manifest are installed by atomic rename, a
crash at any point leaves either the previous committed state or the next one,
never a torn mixture: a half-written run or manifest is a `*.tmp` file that step
3 reclaims, and a finished-but-uncommitted run is an orphan that step 3 reclaims.

---

<sub>Copyright &copy; 2026 <strong>James Gober</strong>. All rights reserved.</sub>
