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

//! Compiled as a private child of `change`, so v1 can reuse context storage
//! without widening the visibility of any existing A2 constructor or type.

use super::{ContextContribution, ContextRootId, ProcessingContext, StableIdBuilder};

pub(crate) fn new_context(namespace: &str, identity: &[u8]) -> ProcessingContext {
    let mut hash = StableIdBuilder::new("drasi.computation.context-root/v1");
    hash.string("namespace", namespace);
    hash.bytes("identity", identity);
    ProcessingContext::new(ContextRootId::new(hash.finish()))
}

// The v1 caller checks length overflow before using A2's append primitive.
pub(crate) fn append_context(
    context: &ProcessingContext,
    contribution: ContextContribution,
) -> ProcessingContext {
    context.append(contribution)
}

pub(crate) fn schema_fingerprint(id: &str, version: u32, encoding: &str, definition: &[u8]) -> u64 {
    let mut hash = StableIdBuilder::new("drasi.computation.schema/v1");
    hash.string("schema-id", id);
    hash.u64("schema-version", u64::from(version));
    hash.string("encoding", encoding);
    hash.bytes("definition", definition);
    hash.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change::{ContextContributor, ContextContributorKind, ContextValue};
    use std::sync::Arc;

    #[test]
    fn computation_reuses_a2_context_allocations() {
        let root = new_context("example", b"event");
        let entry = ContextContribution::new(
            ContextContributor::new(ContextContributorKind::Runtime, "transform"),
            "count",
            ContextValue::Unsigned(1),
        );
        let first = append_context(&root, entry.clone());
        let left = append_context(&first, entry.clone());
        let right = append_context(&first, entry);
        assert!(Arc::ptr_eq(root.root(), left.root()));
        assert!(Arc::ptr_eq(
            left.head().unwrap().parent().unwrap(),
            first.head().unwrap()
        ));
        assert!(Arc::ptr_eq(
            left.head().unwrap().parent().unwrap(),
            right.head().unwrap().parent().unwrap()
        ));
        assert!(!Arc::ptr_eq(left.head().unwrap(), right.head().unwrap()));
        assert_eq!(root.len(), 0);
        assert_eq!(first.len(), 1);
    }
}
