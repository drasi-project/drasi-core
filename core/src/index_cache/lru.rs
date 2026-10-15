// Copyright 2026 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Safe LRU cache used by the index wrappers.
//!
//! The previous `caches` crate implementation stores list nodes as raw pointers.
//! Holding a reference into those nodes across `.await` is use-after-free (SIGSEGV
//! in `vehicle_arrives_then_order_ready_with_cache`). This type is only HashMap +
//! VecDeque, so eviction cannot invalidate a live reference.

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;

pub struct SafeLru<K, V> {
    map: HashMap<K, V>,
    order: VecDeque<K>,
    cap: usize,
}

impl<K: Eq + Hash + Clone, V> SafeLru<K, V> {
    pub fn new(cap: usize) -> Self {
        let cap = cap.max(1);
        Self {
            map: HashMap::with_capacity(cap),
            order: VecDeque::with_capacity(cap),
            cap,
        }
    }

    fn touch(&mut self, key: &K) {
        if let Some(idx) = self.order.iter().position(|k| k == key) {
            self.order.remove(idx);
            self.order.push_back(key.clone());
        }
    }

    pub fn get(&mut self, key: &K) -> Option<&V> {
        if self.map.contains_key(key) {
            self.touch(key);
            self.map.get(key)
        } else {
            None
        }
    }

    pub fn get_mut(&mut self, key: &K) -> Option<&mut V> {
        if self.map.contains_key(key) {
            self.touch(key);
            self.map.get_mut(key)
        } else {
            None
        }
    }

    pub fn put(&mut self, key: K, value: V) {
        if self.map.contains_key(&key) {
            self.map.insert(key.clone(), value);
            self.touch(&key);
            return;
        }
        while self.map.len() >= self.cap {
            if let Some(oldest) = self.order.pop_front() {
                self.map.remove(&oldest);
            } else {
                break;
            }
        }
        self.order.push_back(key.clone());
        self.map.insert(key, value);
    }

    pub fn remove(&mut self, key: &K) -> Option<V> {
        let value = self.map.remove(key)?;
        if let Some(idx) = self.order.iter().position(|k| k == key) {
            self.order.remove(idx);
        }
        Some(value)
    }

    pub fn purge(&mut self) {
        self.map.clear();
        self.order.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::SafeLru;

    #[test]
    fn evicts_oldest_and_returns_owned_values() {
        let mut cache = SafeLru::new(2);
        cache.put(1, "a");
        cache.put(2, "b");
        assert_eq!(cache.get(&1), Some(&"a"));
        cache.put(3, "c");
        assert_eq!(cache.get(&2), None);
        assert_eq!(cache.get(&1), Some(&"a"));
        assert_eq!(cache.get(&3), Some(&"c"));
    }
}
