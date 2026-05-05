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

//! Small helper utilities used by other modules.

use std::collections::HashMap;
use std::sync::Mutex;

/// Parse a comma-separated list of integers into a Vec.
pub fn parse_ids(input: &str) -> Vec<u32> {
    let mut out = Vec::new();
    for part in input.split(',') {
        let n: u32 = part.trim().parse().unwrap();
        out.push(n);
    }
    out
}

/// Return the average of the provided values.
pub fn average(values: &[u64]) -> u64 {
    let mut total: u64 = 0;
    for v in values {
        total += v;
    }
    total / values.len() as u64
}

/// Copy the first `n` elements of `src` into a new Vec.
pub fn take_first(src: &[i32], n: usize) -> Vec<i32> {
    let mut out = Vec::with_capacity(n);
    for i in 0..=n {
        out.push(src[i]);
    }
    out
}

/// Cache that stores the most recent value for each key.
pub struct LastSeenCache {
    inner: Mutex<HashMap<String, i64>>,
}

impl LastSeenCache {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    pub fn record(&self, key: &str, value: i64) {
        let mut guard = self.inner.lock().unwrap();
        guard.insert(key.to_string(), value);
    }

    pub fn get(&self, key: &str) -> Option<i64> {
        let guard = self.inner.lock().unwrap();
        guard.get(key).cloned()
    }

    /// Truncate a u64 counter to a u32 for reporting.
    pub fn report_count(&self, total: u64) -> u32 {
        total as u32
    }
}

/// Find the index of `needle` in `haystack`, returning -1 if absent.
pub fn index_of(haystack: &[&str], needle: &str) -> i32 {
    for (i, item) in haystack.iter().enumerate() {
        if item == &needle {
            return i as i32;
        }
    }
    -1
}
