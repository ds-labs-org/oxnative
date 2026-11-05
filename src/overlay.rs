//! Pending writes of a transaction and the cursor that merges them with a
//! snapshot.

use crate::error::{KvError, from_fjall_ref};
use fjall::Slice;
use std::collections::btree_map;
use std::collections::{BTreeMap, BTreeSet};
use std::iter::Peekable;
use std::ops::Bound;

/// Pending writes of one column family: `Some(value)` for an insert, `None` for
/// a removal. The latest write to a key wins.
pub type Writes = BTreeMap<Vec<u8>, Option<Vec<u8>>>;

/// Pending writes of all column families, by column family index.
#[derive(Default)]
pub struct Overlay {
    pub writes: BTreeMap<usize, Writes>,
    /// Ranges cleared by `remove_range`, applied to the committed state before
    /// `writes` at commit time. Keys written in `writes` after a clear are kept.
    pub cleared: BTreeMap<usize, Vec<(Vec<u8>, Vec<u8>)>>,
}

impl Overlay {
    pub fn insert(&mut self, column_family: usize, key: &[u8], value: &[u8]) {
        self.writes
            .entry(column_family)
            .or_default()
            .insert(key.to_vec(), Some(value.to_vec()));
    }

    pub fn remove(&mut self, column_family: usize, key: &[u8]) {
        self.writes
            .entry(column_family)
            .or_default()
            .insert(key.to_vec(), None);
    }

    pub fn remove_range(&mut self, column_family: usize, start_key: &[u8], end_key: &[u8]) {
        if start_key >= end_key {
            return;
        }
        let writes = self.writes.entry(column_family).or_default();
        // Earlier pending writes in the range become removals: the key may also
        // exist in the committed state.
        let keys = writes
            .range::<[u8], _>((Bound::Included(start_key), Bound::Excluded(end_key)))
            .map(|(k, _)| k.clone())
            .collect::<Vec<_>>();
        for key in keys {
            writes.insert(key, None);
        }
        self.cleared
            .entry(column_family)
            .or_default()
            .push((start_key.to_vec(), end_key.to_vec()));
    }

    pub fn get(&self, column_family: usize, key: &[u8]) -> Option<&Option<Vec<u8>>> {
        self.writes.get(&column_family)?.get(key)
    }

    /// Pending writes of a column family whose key starts with `prefix`.
    pub fn scan_prefix(
        &self,
        column_family: usize,
        prefix: &[u8],
    ) -> Option<btree_map::Range<'_, Vec<u8>, Option<Vec<u8>>>> {
        let writes = self.writes.get(&column_family)?;
        Some(match prefix_upper_bound(prefix) {
            Some(upper) => writes
                .range::<[u8], _>((Bound::Included(prefix), Bound::Excluded(upper.as_slice()))),
            None => writes.range::<[u8], _>((Bound::Included(prefix), Bound::Unbounded)),
        })
    }

    /// Keys of the committed state removed by the cleared ranges and not
    /// overwritten by a later pending write.
    pub fn keys_to_clear(
        &self,
        column_family: usize,
        committed: &fjall::Keyspace,
    ) -> Result<BTreeSet<Slice>, KvError> {
        let mut keys = BTreeSet::new();
        let Some(ranges) = self.cleared.get(&column_family) else {
            return Ok(keys);
        };
        let writes = self.writes.get(&column_family);
        for (start_key, end_key) in ranges {
            for guard in committed.range(start_key.as_slice()..end_key.as_slice()) {
                let key = guard.key()?;
                if writes.is_none_or(|w| !w.contains_key(&*key)) {
                    keys.insert(key);
                }
            }
        }
        Ok(keys)
    }
}

/// The smallest key greater than every key starting with `prefix`, or `None`
/// if there is none (empty prefix or only `0xFF` bytes).
pub fn prefix_upper_bound(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut bound = prefix.to_vec();
    while let Some(last) = bound.pop() {
        if let Some(next) = last.checked_add(1) {
            bound.push(next);
            return Some(bound);
        }
    }
    None
}

enum CurrentKey<'a> {
    Base(Slice),
    Pending(&'a [u8]),
}

/// Cursor over the keys of a snapshot, possibly merged with pending writes.
pub struct Cursor<'a> {
    base: fjall::Iter,
    base_next: Option<Slice>,
    pending: Option<Peekable<btree_map::Range<'a, Vec<u8>, Option<Vec<u8>>>>>,
    current: Option<CurrentKey<'a>>,
    error: Option<fjall::Error>,
}

impl<'a> Cursor<'a> {
    pub fn new(
        base: fjall::Iter,
        pending: Option<btree_map::Range<'a, Vec<u8>, Option<Vec<u8>>>>,
    ) -> Self {
        let mut cursor = Self {
            base,
            base_next: None,
            pending: pending.map(Iterator::peekable),
            current: None,
            error: None,
        };
        cursor.pull_base();
        cursor.advance();
        cursor
    }

    fn pull_base(&mut self) {
        self.base_next = match self.base.next().map(fjall::Guard::key) {
            Some(Ok(key)) => Some(key),
            Some(Err(e)) => {
                self.error = Some(e);
                None
            }
            None => None,
        };
    }

    pub fn advance(&mut self) {
        loop {
            if self.error.is_some() {
                self.current = None;
                return;
            }
            let pending_next = self.pending.as_mut().and_then(Peekable::peek).copied();
            match (self.base_next.take(), pending_next) {
                (None, None) => {
                    self.current = None;
                    return;
                }
                (Some(base), None) => {
                    self.pull_base();
                    self.current = Some(CurrentKey::Base(base));
                    return;
                }
                (base, Some((pending_key, pending_value))) => {
                    if let Some(base) = base {
                        match (*base).cmp(pending_key.as_slice()) {
                            std::cmp::Ordering::Less => {
                                self.pull_base();
                                self.current = Some(CurrentKey::Base(base));
                                return;
                            }
                            // The pending write replaces the committed key
                            std::cmp::Ordering::Equal => self.pull_base(),
                            std::cmp::Ordering::Greater => self.base_next = Some(base),
                        }
                    }
                    if let Some(pending) = &mut self.pending {
                        pending.next();
                    }
                    if pending_value.is_some() {
                        self.current = Some(CurrentKey::Pending(pending_key));
                        return;
                    }
                    // A pending removal: skip the key
                }
            }
        }
    }

    pub fn key(&self) -> Option<&[u8]> {
        match self.current.as_ref()? {
            CurrentKey::Base(key) => Some(key),
            CurrentKey::Pending(key) => Some(key),
        }
    }

    pub fn status(&self) -> Result<(), KvError> {
        match &self.error {
            Some(e) => Err(from_fjall_ref(e)),
            None => Ok(()),
        }
    }
}
