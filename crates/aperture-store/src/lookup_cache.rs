//! The **ingest-time lookup cache**: what `(predicate, key)` already names.
//!
//! [`FjallDb::intern`](crate::store::FjallDb::intern) resolves every fact it is
//! handed, and a nested code index hands it the same parents over and over — one
//! `src.File` is nested inside thousands of references, and each one used to be two
//! live LSM point reads. On the 25M-fact `dotnet/runtime` index the server interned
//! 94.9M facts to create 25.0M, so **73.6% of that work was re-reading something
//! already present** (`bench/FINDINGS.md` §12).
//!
//! Glean has the same cache for the same reason, and calls it the same thing
//! (`glean/rts/cache.h`, *"An LRU fact cache for speeding up point lookups (and only
//! those) during writes"*). Ours is allowed to be much simpler, because a database
//! here is **written once and sealed**: an entry can never go stale, so there is no
//! coherence story, no invalidation, and no partial-fact bookkeeping.
//!
//! **Two generations rather than an LRU list.** A linked list per entry is what
//! Glean's own post-mortem warns about — its id-and-key maps *"used quite a bit of
//! memory (almost exactly as much as the facts themselves)"* — so this keeps two
//! hash maps and rotates them: lookups check `young`, then `old` (promoting on the
//! way), and when `young` fills, `old` is dropped and `young` becomes the new `old`.
//! Bursts on the same parents — exactly what a syntax walk emits — stay resident,
//! and the cost is one hash rather than a hash plus list surgery.
//!
//! # What it is built to have, and what comes next
//!
//! **A hit allocates nothing.** The question `intern` asks is not "give me the row"
//! but "is this key present, and does the stored value agree with the one I hold" —
//! so [`LookupCache::lookup`] answers *that*, comparing under the lock and handing
//! back an id. Returning the row instead would clone its value on every hit, which
//! is a heap allocation per interned reference on the one path whose entire purpose
//! is to remove per-reference work. Five of `code_index`'s 27 predicates carry a
//! value side and `src.Decl` — a hot parent — is one of them.
//!
//! **The bound is bytes, not entries.** An entry count does not bound memory: these
//! keys are encoded fact keys, and a corpus of long paths costs several times one of
//! short ones for the same count. The budget is charged per entry as `key + value`
//! plus [`ENTRY_OVERHEAD`], so a stated ceiling is a real one. At most twice the
//! budget is resident, since `old` is a whole generation.
//!
//! **Sharding, when it comes.** [Phase 12d](../../../PLAN.md) stripes interning by
//! `hash(predicate ++ key)` and gives each stripe its own cache. This type is what
//! goes behind one stripe: `young`/`old` are already per-instance, and a stripe's
//! lock is what supplies the `&mut` a promote needs. The budget is then **divided**
//! across stripes, never multiplied by them.

use std::collections::HashMap;

use aperture_schema::id::FactId;

/// Charged per entry on top of its key and value bytes: the map slot (control byte,
/// two boxed-slice headers and a [`FactId`], at hashbrown's load factor) plus the two
/// heap headers behind them. Approximate on purpose — it makes the budget an honest
/// ceiling rather than an exact one, and being wrong by a few bytes per entry costs
/// accuracy in a memory report, not correctness.
const ENTRY_OVERHEAD: usize = 48;

/// What one entry costs against the budget.
pub(crate) fn cost(key_len: usize, value_len: usize) -> usize {
    key_len + value_len + ENTRY_OVERHEAD
}

/// What a `keys` index row names: the fact's id, and the value stored beside it.
struct Cached {
    id: FactId,
    /// Empty for a key-only predicate, which is also the only value it can have.
    value: Box<[u8]>,
}

/// What the cache knows about a key it holds.
///
/// The two arms are `intern`'s two outcomes, decided here so the value never leaves
/// the cache: `ops-I5`'s silent dedup, and its same-key-different-value reject.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Hit {
    /// Present, and the stored value is the one the caller holds.
    Agrees(FactId),
    /// Present under this key with a *different* value — a conflict.
    Conflicts(FactId),
}

/// A byte-bounded, two-generation map from an index row to the fact it names.
pub(crate) struct LookupCache {
    young: HashMap<Box<[u8]>, Cached>,
    old: HashMap<Box<[u8]>, Cached>,
    /// Bytes charged to `young`; it rotates once this reaches `budget`.
    young_bytes: usize,
    /// The ceiling on `young`. At most `2 * budget` is resident.
    budget: usize,
    hits: u64,
    misses: u64,
}

impl LookupCache {
    /// A cache holding at most `budget` bytes per generation.
    ///
    /// **Reserves nothing.** A server opens every database under its store root and
    /// most of them are sealed, so a budget-sized reservation here would be megabytes
    /// of hash table per database that can never be written to — reads do not consult
    /// this. The map grows on demand instead, and a rotation sizes the new generation
    /// from the one it replaces, which is a better estimate than any constant.
    pub(crate) fn new(budget: usize) -> Self {
        Self {
            young: HashMap::new(),
            old: HashMap::new(),
            young_bytes: 0,
            budget: budget.max(cost(0, 0)),
            hits: 0,
            misses: 0,
        }
    }

    /// Whether `key` is held, and whether what is held agrees with `value`.
    ///
    /// Promotes a hit found in `old`, which is the one thing a generational cache
    /// gets wrong if it only ever reads: without it a live entry is dropped at the
    /// next rotation. The promotion **moves** the entry rather than copying it, so a
    /// hit costs one key allocation for the new generation's map and nothing else.
    pub(crate) fn lookup(&mut self, key: &[u8], value: &[u8]) -> Option<Hit> {
        if let Some(found) = self.young.get(key) {
            self.hits += 1;
            return Some(found.hit(value));
        }

        if let Some(found) = self.old.remove(key) {
            self.hits += 1;
            let hit = found.hit(value);
            self.insert_young(key.to_vec().into_boxed_slice(), found);
            return Some(hit);
        }

        self.misses += 1;
        None
    }

    pub(crate) fn insert(&mut self, key: &[u8], id: FactId, value: &[u8]) {
        self.insert_young(
            key.to_vec().into_boxed_slice(),
            Cached {
                id,
                value: value.to_vec().into_boxed_slice(),
            },
        );
    }

    fn insert_young(&mut self, key: Box<[u8]>, entry: Cached) {
        if self.young_bytes >= self.budget {
            // The generation being retired is the best available estimate of how many
            // entries the next one will hold.
            let previously = self.young.len();
            self.old = std::mem::take(&mut self.young);
            self.young = HashMap::with_capacity(previously);
            self.young_bytes = 0;
        }

        let (key_len, added) = (key.len(), cost(key.len(), entry.value.len()));
        // A key already held is one row, not two, so charge for one. A fact never
        // changes, so this can only be the same entry arriving twice — the
        // arithmetic is kept right anyway, because a wrong byte count is how a
        // budget stops being a bound.
        if let Some(previous) = self.young.insert(key, entry) {
            self.young_bytes -= cost(key_len, previous.value.len());
        }
        self.young_bytes += added;
    }

    /// Hits and misses since open — the number that says whether this is working.
    pub(crate) fn counters(&self) -> (u64, u64) {
        (self.hits, self.misses)
    }
}

impl Cached {
    fn hit(&self, value: &[u8]) -> Hit {
        if &*self.value == value {
            Hit::Agrees(self.id)
        } else {
            Hit::Conflicts(self.id)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u64) -> FactId {
        FactId::from_raw(n)
    }

    /// A budget holding exactly `entries` single-byte keys with empty values.
    fn budget_for(entries: usize) -> usize {
        entries * cost(1, 0)
    }

    #[test]
    fn returns_what_was_inserted() {
        let mut cache = LookupCache::new(budget_for(8));
        cache.insert(b"a", id(1), b"v");

        assert_eq!(cache.lookup(b"a", b"v"), Some(Hit::Agrees(id(1))));
        assert_eq!(cache.lookup(b"b", b""), None);
    }

    /// The same key with a different value is the conflict `ops-I5` rejects, and the
    /// cache is what decides it — so the value never has to leave the cache.
    #[test]
    fn a_different_value_under_a_held_key_is_a_conflict() {
        let mut cache = LookupCache::new(budget_for(8));
        cache.insert(b"a", id(1), b"v");

        assert_eq!(cache.lookup(b"a", b"other"), Some(Hit::Conflicts(id(1))));
    }

    #[test]
    fn rotation_keeps_the_previous_generation_readable() {
        let mut cache = LookupCache::new(budget_for(2));
        cache.insert(b"a", id(1), b"");
        cache.insert(b"b", id(2), b"");
        // Rotates: a and b move to `old`, which is still consulted.
        cache.insert(b"c", id(3), b"");

        assert_eq!(cache.lookup(b"a", b""), Some(Hit::Agrees(id(1))));
        assert_eq!(cache.lookup(b"c", b""), Some(Hit::Agrees(id(3))));
    }

    #[test]
    fn a_hit_in_the_old_generation_is_promoted() {
        let mut cache = LookupCache::new(budget_for(2));
        cache.insert(b"a", id(1), b"");
        cache.insert(b"b", id(2), b"");
        cache.insert(b"c", id(3), b""); // a, b -> old
        assert!(cache.lookup(b"a", b"").is_some()); // promotes a into young
        cache.insert(b"d", id(4), b""); // rotates again; a survives in old
        cache.insert(b"e", id(5), b"");

        assert_eq!(cache.lookup(b"a", b""), Some(Hit::Agrees(id(1))));
    }

    /// **The budget is a bound, and it is a bound in bytes.** The reason this is a
    /// test and not a comment: the earlier form counted *entries*, which bounds
    /// nothing when the keys are encoded fact keys of whatever length the corpus has.
    #[test]
    fn the_budget_bounds_what_is_resident() {
        let budget = budget_for(4);
        let mut cache = LookupCache::new(budget);

        // Keys far longer than the one the budget was quoted in, so a count-based
        // bound would let this run away.
        for n in 0..200u8 {
            let key = [n; 64];
            cache.insert(&key, id(u64::from(n) + 1), b"value");
        }

        assert!(
            cache.young_bytes <= budget + cost(64, 5),
            "young holds {} bytes against a {budget}-byte budget",
            cache.young_bytes
        );
        // `old` is a whole generation, so the resident claim is two budgets, and the
        // rotation has to have actually happened for that to be the operative bound.
        assert!(!cache.old.is_empty(), "nothing ever rotated");
    }

    /// A key inserted twice is charged once — the arithmetic a bound depends on.
    #[test]
    fn the_same_key_twice_is_charged_once() {
        let mut cache = LookupCache::new(budget_for(64));
        cache.insert(b"a", id(1), b"v");
        let after_first = cache.young_bytes;
        cache.insert(b"a", id(1), b"v");

        assert_eq!(cache.young_bytes, after_first);
        assert_eq!(cache.young.len(), 1);
    }

    #[test]
    fn counts_hits_and_misses() {
        let mut cache = LookupCache::new(budget_for(8));
        cache.insert(b"a", id(1), b"");
        cache.lookup(b"a", b"");
        cache.lookup(b"z", b"");

        assert_eq!(cache.counters(), (1, 1));
    }
}
