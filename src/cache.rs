//! A shared cache of decoded data blocks.
//!
//! A point lookup that reaches a run decodes one data block: a positioned read,
//! a CRC32C check, and a parse. When the same blocks are read repeatedly — a hot
//! working set over a stable run — that work is pure waste. The [`BlockCache`]
//! keeps recently-read decoded blocks so a repeat lookup returns an
//! [`Arc`]-shared block with no I/O, no checksum, and no parse.
//!
//! The cache is sharded to cut lock contention between reader threads, and each
//! shard uses **CLOCK** (second-chance) eviction — the classic O(1) buffer-pool
//! policy — to approximate LRU without a per-access linked-list write. Capacity
//! is counted in block-sized units (`capacity_bytes / 4 KiB` blocks); a capacity
//! of zero disables the cache, and every lookup decodes directly.
//!
//! Only point lookups populate the cache. Sequential scans and compaction read
//! each block once and would only pollute it, so they bypass it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::record::Record;

/// A decoded data block: its entries in ascending key order.
pub(crate) type DecodedBlock = Vec<(Vec<u8>, Record)>;

/// Approximate size, in bytes, that one cached block occupies — used only to
/// translate a byte capacity into a block-slot count.
const ASSUMED_BLOCK_BYTES: usize = 4 * 1024;

/// Number of shards. A small power of two keeps the shard-select mask cheap
/// while spreading contention across reader threads.
const SHARDS: usize = 16;

/// Identifies a block: which run, and which block within it. The run id is
/// unique per [`SsTable`](crate::sstable::SsTable) instance for the lifetime of
/// an engine, so keys never collide across runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct BlockKey {
    pub(crate) run_id: u64,
    pub(crate) block_idx: u32,
}

/// One resident block in a shard.
#[derive(Debug)]
struct Slot {
    key: BlockKey,
    block: Arc<DecodedBlock>,
    /// CLOCK reference bit: set on access, cleared by the eviction hand.
    referenced: bool,
}

/// One shard: a fixed ring of slots with a CLOCK eviction hand.
#[derive(Debug)]
struct Shard {
    slots: Vec<Option<Slot>>,
    index: HashMap<BlockKey, usize>,
    hand: usize,
}

impl Shard {
    fn with_slots(n: usize) -> Self {
        Shard {
            slots: (0..n).map(|_| None).collect(),
            index: HashMap::with_capacity(n),
            hand: 0,
        }
    }

    fn get(&mut self, key: BlockKey) -> Option<Arc<DecodedBlock>> {
        let i = *self.index.get(&key)?;
        if let Some(slot) = self.slots[i].as_mut() {
            slot.referenced = true;
            return Some(Arc::clone(&slot.block));
        }
        None
    }

    fn insert(&mut self, key: BlockKey, block: Arc<DecodedBlock>) {
        if self.slots.is_empty() {
            return; // disabled
        }
        if let Some(&i) = self.index.get(&key) {
            if let Some(slot) = self.slots[i].as_mut() {
                slot.block = block;
                slot.referenced = true;
            }
            return;
        }
        let i = self.free_or_evict();
        if let Some(old) = self.slots[i].take() {
            let _ = self.index.remove(&old.key);
        }
        self.slots[i] = Some(Slot {
            key,
            block,
            referenced: true,
        });
        let _ = self.index.insert(key, i);
    }

    /// Return a slot index to use: a free slot if one exists, else the slot the
    /// CLOCK hand evicts (the first one whose reference bit is clear).
    fn free_or_evict(&mut self) -> usize {
        let len = self.slots.len();
        // Up to one full sweep clearing reference bits, then a guaranteed evict
        // on the next pass — terminates in at most 2·len steps.
        for _ in 0..(2 * len) {
            let i = self.hand;
            self.hand = (self.hand + 1) % len;
            match self.slots[i].as_mut() {
                None => return i,
                Some(slot) if !slot.referenced => return i,
                Some(slot) => slot.referenced = false,
            }
        }
        // Unreachable in practice; fall back to the hand position.
        self.hand
    }
}

/// A sharded, CLOCK-evicting cache of decoded data blocks, shared across all
/// runs of one engine behind an [`Arc`].
#[derive(Debug)]
pub(crate) struct BlockCache {
    shards: Vec<Mutex<Shard>>,
    enabled: bool,
}

impl BlockCache {
    /// Build a cache holding roughly `capacity_bytes` of decoded blocks. A
    /// capacity of zero returns a disabled cache that never stores anything.
    pub(crate) fn new(capacity_bytes: usize) -> Arc<Self> {
        let total_slots = capacity_bytes / ASSUMED_BLOCK_BYTES;
        let enabled = total_slots > 0;
        // At least one slot per shard when enabled, so a hot block is cacheable
        // even with a tiny configured capacity.
        let per_shard = if enabled {
            total_slots.div_ceil(SHARDS).max(1)
        } else {
            0
        };
        let shards = (0..SHARDS)
            .map(|_| Mutex::new(Shard::with_slots(per_shard)))
            .collect();
        Arc::new(BlockCache { shards, enabled })
    }

    /// Fetch a cached block, marking it recently used.
    pub(crate) fn get(&self, key: BlockKey) -> Option<Arc<DecodedBlock>> {
        if !self.enabled {
            return None;
        }
        self.shard(key)
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(key)
    }

    /// Insert a freshly-decoded block.
    pub(crate) fn insert(&self, key: BlockKey, block: Arc<DecodedBlock>) {
        if !self.enabled {
            return;
        }
        self.shard(key)
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(key, block);
    }

    fn shard(&self, key: BlockKey) -> &Mutex<Shard> {
        // Mix the key to a shard index. SHARDS is a power of two, so a mask
        // selects the shard without a division.
        let mut h = key.run_id.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        h ^= u64::from(key.block_idx).wrapping_mul(0xC2B2_AE3D_27D4_EB4F);
        &self.shards[(h as usize) & (SHARDS - 1)]
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn block(n: u8) -> Arc<DecodedBlock> {
        Arc::new(vec![(vec![n], Record::Value(vec![n]))])
    }

    fn key(run: u64, blk: u32) -> BlockKey {
        BlockKey {
            run_id: run,
            block_idx: blk,
        }
    }

    #[test]
    fn test_hit_after_insert() {
        let c = BlockCache::new(1 << 20);
        assert!(c.get(key(1, 0)).is_none());
        c.insert(key(1, 0), block(7));
        let got = c.get(key(1, 0)).expect("hit");
        assert_eq!(got[0].0, vec![7]);
    }

    #[test]
    fn test_disabled_never_stores() {
        let c = BlockCache::new(0);
        c.insert(key(1, 0), block(1));
        assert!(c.get(key(1, 0)).is_none());
    }

    #[test]
    fn test_distinct_keys_coexist() {
        let c = BlockCache::new(1 << 20);
        c.insert(key(1, 0), block(1));
        c.insert(key(1, 1), block(2));
        c.insert(key(2, 0), block(3));
        assert_eq!(c.get(key(1, 0)).unwrap()[0].0, vec![1]);
        assert_eq!(c.get(key(1, 1)).unwrap()[0].0, vec![2]);
        assert_eq!(c.get(key(2, 0)).unwrap()[0].0, vec![3]);
    }

    #[test]
    fn test_eviction_bounds_a_shard() {
        // One slot per shard. Two keys that land in the same shard force an
        // eviction; the cache must never exceed its slots.
        let mut shard = Shard::with_slots(1);
        shard.insert(key(1, 0), block(1));
        assert!(shard.get(key(1, 0)).is_some());
        // Inserting a second key (ref bit of the first is set, so one sweep
        // clears it, then it is evicted).
        shard.insert(key(1, 1), block(2));
        shard.insert(key(1, 2), block(3));
        // At most one resident; the most recently inserted is present.
        assert!(shard.get(key(1, 2)).is_some());
        let resident = [key(1, 0), key(1, 1), key(1, 2)]
            .into_iter()
            .filter(|k| shard.get(*k).is_some())
            .count();
        assert_eq!(resident, 1, "a one-slot shard holds exactly one block");
    }

    #[test]
    fn test_reinsert_same_key_updates() {
        let c = BlockCache::new(1 << 20);
        c.insert(key(5, 2), block(1));
        c.insert(key(5, 2), block(9));
        assert_eq!(c.get(key(5, 2)).unwrap()[0].0, vec![9]);
    }
}
