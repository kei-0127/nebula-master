//! # Key-Sorted Cache
//! 
//! Cache that maintains items sorted by key order.
//! Automatically evicts smallest keys when full.

use std::collections::BTreeMap;

/// Fixed-capacity, key-sorted map. Keeps the largest `limit` keys and
/// evicts the smallest keys automatically when over capacity.
pub struct KeySortedCache<K, V> {
    limit: usize,                    // Maximum number of items
    value_by_key: BTreeMap<K, V>,   // Sorted key-value storage
}

impl<K: Ord + Clone, V> KeySortedCache<K, V> {
    /// Create a key-sorted cache that holds at most `limit` entries.
    pub fn new(limit: usize) -> Self {
        Self {
            limit,
            value_by_key: BTreeMap::new(),
        }
    }

    /// Insert or replace a value; if capacity exceeded, evict the smallest key.
    pub fn insert(&mut self, key: K, value: V) {
        self.value_by_key.insert(key, value);
        // Remove smallest key if cache is full
        if self.value_by_key.len() > self.limit {
            let smallest_key = self.value_by_key.keys().next().unwrap().clone();
            self.value_by_key.remove(&smallest_key);
        }
    }

    /// Iterate over entries in ascending key order.
    pub fn iter(&self) -> impl Iterator<Item = (&K, &V)> + '_ {
        self.value_by_key.iter()
    }

    /// Mutably iterate over entries in ascending key order.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (&K, &mut V)> + '_ {
        self.value_by_key.iter_mut()
    }

    /// Remove an entry by key; no-op if missing.
    pub fn remove(&mut self, key: &K) {
        self.value_by_key.remove(key);
    }

    /// True if the cache currently holds no entries.
    pub fn is_empty(&self) -> bool {
        self.value_by_key.is_empty()
    }

    /// Keep only entries for which `f(key, value)` returns true.
    pub fn retain(&mut self, f: impl FnMut(&K, &mut V) -> bool) {
        self.value_by_key.retain(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_buffer() {
        let mut buffer = KeySortedCache::new(2);
        assert!(buffer.is_empty());
        buffer.insert(1, "A");
        assert!(!buffer.is_empty());
        buffer.insert(2, "B");
        assert!(!buffer.is_empty());

        assert_eq!(
            vec![(&1, &"A"), (&2, &"B")],
            buffer.iter().collect::<Vec<_>>()
        );
    }

    #[test]
    fn overfill_buffer() {
        let mut buffer = KeySortedCache::new(2);
        buffer.insert(1, "A");
        buffer.insert(2, "B");
        buffer.insert(3, "C");

        assert_eq!(
            vec![(&2, &"B"), (&3, &"C")],
            buffer.iter().collect::<Vec<_>>()
        );
    }

    #[test]
    fn overfill_buffer_with_lower_key() {
        let mut buffer = KeySortedCache::new(2);
        buffer.insert(2, "B");
        buffer.insert(3, "C");
        buffer.insert(1, "A");

        assert_eq!(
            vec![(&2, &"B"), (&3, &"C")],
            buffer.iter().collect::<Vec<_>>()
        );
    }

    #[test]
    fn overfill_buffer_with_middle_key() {
        let mut buffer = KeySortedCache::new(2);
        buffer.insert(1, "A");
        buffer.insert(3, "C");
        buffer.insert(2, "B");

        assert_eq!(
            vec![(&2, &"B"), (&3, &"C")],
            buffer.iter().collect::<Vec<_>>()
        );
    }

    #[test]
    fn replace_key() {
        let mut buffer = KeySortedCache::new(3);
        buffer.insert(1, "A");
        buffer.insert(2, "B");
        buffer.insert(1, "C");

        assert_eq!(
            vec![(&1, &"C"), (&2, &"B")],
            buffer.iter().collect::<Vec<_>>()
        );
    }

    #[test]
    fn replace_key_once_full() {
        let mut buffer = KeySortedCache::new(2);
        buffer.insert(1, "A");
        buffer.insert(2, "B");
        buffer.insert(1, "C");

        assert_eq!(
            vec![(&1, &"C"), (&2, &"B")],
            buffer.iter().collect::<Vec<_>>()
        );
    }

    #[test]
    fn remove_key() {
        let mut buffer = KeySortedCache::new(3);
        buffer.insert(1, "A");
        buffer.insert(2, "B");
        buffer.remove(&1);

        assert_eq!(vec![(&2, &"B")], buffer.iter().collect::<Vec<_>>());
    }

    #[test]
    fn iter_mut_update_value() {
        let mut buffer = KeySortedCache::new(3);
        buffer.insert(1, "A".to_string());
        buffer.insert(2, "B".to_string());
        buffer.insert(3, "C".to_string());

        for (_k, v) in buffer.iter_mut() {
            *v = format!("{}x{}", v, v);
        }

        assert_eq!(
            vec![
                (&1, &"AxA".to_string()),
                (&2, &"BxB".to_string()),
                (&3, &"CxC".to_string())
            ],
            buffer.iter().collect::<Vec<_>>()
        );
    }

    #[test]
    fn retain() {
        let mut buffer = KeySortedCache::new(4);
        buffer.insert(1, "A");
        buffer.insert(2, "B");
        buffer.insert(3, "C");
        buffer.insert(4, "D");
        buffer.retain(|key, _value| *key > 2);

        assert_eq!(
            vec![(&3, &"C"), (&4, &"D")],
            buffer.iter().collect::<Vec<_>>()
        );
    }
}
