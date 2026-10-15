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

//! Computation-owned A2 context sharing and canonical identity primitives.

use std::sync::Arc;

#[path = "../../a2_bridge.rs"]
pub(crate) mod computation_bridge;

#[derive(Debug)]
struct ProcessingContextNode<T> {
    parent: Option<Arc<ProcessingContextNode<T>>>,
    contribution: T,
}

/// A2's persistent chain, with contribution data owned by the versioned contract.
#[derive(Debug, Clone)]
pub(crate) struct ProcessingContext<T> {
    root: Arc<u64>,
    head: Option<Arc<ProcessingContextNode<T>>>,
    len: usize,
}

impl<T: Clone> ProcessingContext<T> {
    fn new(root_id: u64) -> Self {
        Self {
            root: Arc::new(root_id),
            head: None,
            len: 0,
        }
    }

    fn append(&self, contribution: T) -> Option<Self> {
        let len = self.len.checked_add(1)?;
        Some(Self {
            root: self.root.clone(),
            head: Some(Arc::new(ProcessingContextNode {
                parent: self.head.clone(),
                contribution,
            })),
            len,
        })
    }

    pub(crate) const fn len(&self) -> usize {
        self.len
    }

    pub(crate) const fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub(crate) fn entries(&self) -> impl Iterator<Item = T> {
        let mut head = self.head.clone();
        std::iter::from_fn(move || {
            let node = head.take()?;
            head = node.parent.clone();
            Some(node.contribution.clone())
        })
    }
}

/// The existing length-delimited A2 identity hash, not a reversible codec.
struct StableIdBuilder {
    state: u64,
}

impl StableIdBuilder {
    fn new(domain: &str) -> Self {
        let mut builder = Self {
            state: 0xcbf2_9ce4_8422_2325,
        };
        builder.bytes("domain", domain.as_bytes());
        builder
    }

    fn update(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.state ^= u64::from(*byte);
            self.state = self.state.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    fn bytes(&mut self, label: &str, value: &[u8]) {
        self.update(&(label.len() as u64).to_le_bytes());
        self.update(label.as_bytes());
        self.update(&(value.len() as u64).to_le_bytes());
        self.update(value);
    }

    fn string(&mut self, label: &str, value: &str) {
        self.bytes(label, value.as_bytes());
    }

    fn u64(&mut self, label: &str, value: u64) {
        self.bytes(label, &value.to_le_bytes());
    }

    fn finish(self) -> u64 {
        self.state
    }
}
