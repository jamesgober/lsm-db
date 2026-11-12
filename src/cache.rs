//! A shared cache of decoded data blocks.
//!
//! A point lookup that reaches a run decodes one data block: a positioned read,
//! a CRC32C check, and a parse. When the same blocks are read repeatedly — a hot
//! working set over a stable run — that work is pure waste. The [`BlockCache`]
//! keeps recently-read decoded blocks so a repeat lookup returns an
//! [`Arc`]-shared block with no I/O, no checksum, and no parse.
//!
//! The cache is sharded to cut lock contention between reader threads, and each
//! shard uses **CLOCK** (second-chance) eviction (the classic O(1) buffer-pool
//! policy) to approximate LRU without a per-access linked-list write.
//!
//! Capacity is counted in bytes: each block is charged the memory its decoded
//! form holds (its keys and values plus a fixed cost per entry), and a shard
//! evicts until its blocks fit its share of the capacity. A block larger than
//! a quarter of a shard is not cached at all, so one large value cannot push
//! out a shard's whole working set. A capacity of zero disables the cache, and
//! every lookup decodes directly.
//!
//! Only point lookups populate the cache. Sequential scans and compaction read
//! each block once and would only pollute it, so they bypass it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::record::Record;

/// A decoded data block: its entries in ascending key order.
pub(crate) type DecodedBlock = Vec<(Vec<u8>, Record)>;

/// Most shards. A small power of two keeps the shard-select mask cheap while
/// spreading contention across reader threads.
const MAX_SHARDS: usize = 16;

/// Smallest share of the capacity a shard gets: a small cache has fewer
/// shards rather than shards too small to hold a block.
const MIN_SHARD_BYTES: usize = 256 * 1024;

/// The memory a decoded entry holds besides its key and value bytes: the
/// entry itself in the block's vector and the two heap allocations, rounded
/// up for the allocator's own overhead.
const ENTRY_OVERHEAD: usize = std::mem::size_of::<(Vec<u8>, Record)>() + 32;

/// The memory a decoded block holds, as the cache charges it.
pub(crate) fn charge_of(block: &DecodedBlock) -> usize {
    block
        .iter()
        .fold(std::mem::size_of::<DecodedBlock>(), |sum, (key, record)| {
            sum + ENTRY_OVERHEAD + key.len() + record.value_len()
        })
}

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
    /// What the block is charged against the shard's budget.
    charge: usize,
    /// CLOCK reference bit: set on access, cleared by the eviction hand.
    referenced: bool,
}

/// One shard: the resident blocks, a CLOCK eviction hand over them, and a
/// byte budget.
#[derive(Debug)]
struct Shard {
    slots: Vec<Slot>,
    index: HashMap<BlockKey, usize>,
    hand: usize,
    /// Sum of the resident blocks' charges; never above `budget`.
    bytes: usize,
    budget: usize,
}

impl Shard {
    fn with_budget(budget: usize) -> Self {
        Shard {
            slots: Vec::new(),
            index: HashMap::new(),
            hand: 0,
            bytes: 0,
            budget,
        }
    }

    /// The largest block this shard caches: a quarter of its budget, or a
    /// whole small budget up to 16 KiB, so even a tiny cache holds a block.
    fn max_charge(&self) -> usize {
        (self.budget / 4).max(self.budget.min(16 * 1024))
    }

    fn get(&mut self, key: BlockKey) -> Option<Arc<DecodedBlock>> {
        let i = *self.index.get(&key)?;
        let slot = self.slots.get_mut(i)?;
        slot.referenced = true;
        Some(Arc::clone(&slot.block))
    }

    fn insert(&mut self, key: BlockKey, block: Arc<DecodedBlock>, charge: usize) {
        if charge > self.max_charge() {
            return; // too large to cache, or the shard is disabled
        }
        if let Some(i) = self.index.get(&key).copied() {
            self.remove(i);
        }
        while self.bytes + charge > self.budget && !self.slots.is_empty() {
            self.evict_one();
        }
        self.bytes += charge;
        let _ = self.index.insert(key, self.slots.len());
        self.slots.push(Slot {
            key,
            block,
            charge,
            referenced: true,
        });
    }

    /// Evict the block the CLOCK hand reaches first with its reference bit
    /// clear, clearing the bits it passes. Ends within two sweeps.
    fn evict_one(&mut self) {
        for _ in 0..2 * self.slots.len() {
            if self.hand >= self.slots.len() {
                self.hand = 0;
            }
            let slot = &mut self.slots[self.hand];
            if slot.referenced {
                slot.referenced = false;
                self.hand += 1;
            } else {
                self.remove(self.hand);
                return;
            }
        }
    }

    /// Remove the block in slot `i`. The last slot moves into its place.
    fn remove(&mut self, i: usize) {
        let old = self.slots.swap_remove(i);
        let _ = self.index.remove(&old.key);
        self.bytes -= old.charge;
        if let Some(moved) = self.slots.get(i) {
            let _ = self.index.insert(moved.key, i);
        }
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
    /// Build a cache holding at most `capacity_bytes` of decoded blocks, as
    /// [`charge_of`] counts them. A capacity of zero returns a disabled cache
    /// that never stores anything.
    pub(crate) fn new(capacity_bytes: usize) -> Arc<Self> {
        let enabled = capacity_bytes > 0;
        let mut shards = MAX_SHARDS;
        while shards > 1 && capacity_bytes / shards < MIN_SHARD_BYTES {
            shards /= 2;
        }
        let budget = capacity_bytes / shards;
        let shards = (0..shards)
            .map(|_| Mutex::new(Shard::with_budget(budget)))
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

    /// Insert a freshly-decoded block, charged `charge` bytes (see
    /// [`charge_of`]).
    pub(crate) fn insert(&self, key: BlockKey, block: Arc<DecodedBlock>, charge: usize) {
        if !self.enabled {
            return;
        }
        self.shard(key)
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(key, block, charge);
    }

    /// Total bytes charged by the resident blocks. Test-only.
    #[cfg(test)]
    pub(crate) fn resident_bytes(&self) -> usize {
        self.shards
            .iter()
            .map(|s| s.lock().unwrap_or_else(|p| p.into_inner()).bytes)
            .sum()
    }

    fn shard(&self, key: BlockKey) -> &Mutex<Shard> {
        // Mix the key to a shard index. The shard count is a power of two, so
        // a mask selects the shard without a division.
        let mut h = key.run_id.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        h ^= u64::from(key.block_idx).wrapping_mul(0xC2B2_AE3D_27D4_EB4F);
        &self.shards[(h as usize) & (self.shards.len() - 1)]
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn block(n: u8) -> Arc<DecodedBlock> {
        Arc::new(vec![(vec![n], Record::Value(vec![n]))])
    }

    fn charge(b: &Arc<DecodedBlock>) -> usize {
        charge_of(b)
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
        let b = block(7);
        c.insert(key(1, 0), Arc::clone(&b), charge(&b));
        let got = c.get(key(1, 0)).expect("hit");
        assert_eq!(got[0].0, vec![7]);
    }

    #[test]
    fn test_disabled_never_stores() {
        let c = BlockCache::new(0);
        let b = block(1);
        c.insert(key(1, 0), Arc::clone(&b), charge(&b));
        assert!(c.get(key(1, 0)).is_none());
    }

    #[test]
    fn test_distinct_keys_coexist() {
        let c = BlockCache::new(1 << 20);
        for (k, n) in [(key(1, 0), 1), (key(1, 1), 2), (key(2, 0), 3)] {
            let b = block(n);
            c.insert(k, Arc::clone(&b), charge(&b));
        }
        assert_eq!(c.get(key(1, 0)).unwrap()[0].0, vec![1]);
        assert_eq!(c.get(key(1, 1)).unwrap()[0].0, vec![2]);
        assert_eq!(c.get(key(2, 0)).unwrap()[0].0, vec![3]);
    }

    #[test]
    fn test_eviction_bounds_a_shard() {
        // A budget for one block. Further keys force evictions; the shard
        // never holds more than its budget.
        let one = charge(&block(1));
        let mut shard = Shard::with_budget(one);
        shard.insert(key(1, 0), block(1), one);
        assert!(shard.get(key(1, 0)).is_some());
        // Inserting a second key (ref bit of the first is set, so one sweep
        // clears it, then it is evicted).
        shard.insert(key(1, 1), block(2), one);
        shard.insert(key(1, 2), block(3), one);
        // At most one resident; the most recently inserted is present.
        assert!(shard.get(key(1, 2)).is_some());
        let resident = [key(1, 0), key(1, 1), key(1, 2)]
            .into_iter()
            .filter(|k| shard.get(*k).is_some())
            .count();
        assert_eq!(resident, 1, "a one-block budget holds exactly one block");
        assert_eq!(shard.bytes, one);
    }

    #[test]
    fn test_reinsert_same_key_updates() {
        let c = BlockCache::new(1 << 20);
        let (b1, b9) = (block(1), block(9));
        c.insert(key(5, 2), Arc::clone(&b1), charge(&b1));
        c.insert(key(5, 2), Arc::clone(&b9), charge(&b9));
        assert_eq!(c.get(key(5, 2)).unwrap()[0].0, vec![9]);
        assert_eq!(c.resident_bytes(), charge(&b9));
    }

    /// Large blocks are charged their bytes. With 8 MiB configured, blocks
    /// of one 256 KiB value each used to fill 2,048 slots (512 MiB). Now
    /// the resident bytes stay within the capacity, and a block larger
    /// than a quarter of a shard (128 KiB of an 8 MiB cache) is not kept.
    #[test]
    fn test_capacity_bounds_bytes_not_blocks() {
        let value_block = |i: u32| -> Arc<DecodedBlock> {
            Arc::new(vec![(
                i.to_be_bytes().to_vec(),
                Record::Value(vec![0; 256 << 10]),
            )])
        };
        let small = BlockCache::new(8 << 20);
        for i in 0..64u32 {
            let b = value_block(i);
            small.insert(key(1, i), Arc::clone(&b), charge(&b));
        }
        assert_eq!(
            small.resident_bytes(),
            0,
            "256 KiB blocks pass an 8 MiB cache by"
        );

        let capacity = 64 << 20;
        let c = BlockCache::new(capacity);
        for i in 0..512u32 {
            let b = value_block(i);
            c.insert(key(1, i), Arc::clone(&b), charge(&b));
        }
        assert!(c.resident_bytes() <= capacity, "{}", c.resident_bytes());
        assert!(c.resident_bytes() > capacity / 2, "{}", c.resident_bytes());
    }

    /// A small cache has fewer, larger shards, so a 4 KiB block still fits.
    #[test]
    fn test_small_cache_still_holds_a_block() {
        let c = BlockCache::new(16 * 1024);
        assert_eq!(c.shards.len(), 1);
        let b: Arc<DecodedBlock> = Arc::new(
            (0..40u32)
                .map(|i| (i.to_be_bytes().to_vec(), Record::Value(vec![1; 90])))
                .collect(),
        );
        c.insert(key(1, 0), Arc::clone(&b), charge(&b));
        assert!(c.get(key(1, 0)).is_some());
    }
}
