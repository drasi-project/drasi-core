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

//! Bridge to computation-owned A2 primitives; no legacy pipeline dependency.

use super::{ProcessingContext, StableIdBuilder};

pub(crate) fn new_context<T: Clone>(namespace: &str, identity: &[u8]) -> ProcessingContext<T> {
    let mut hash = StableIdBuilder::new("drasi.computation.context-root/v1");
    hash.string("namespace", namespace);
    hash.bytes("identity", identity);
    ProcessingContext::new(hash.finish())
}

pub(crate) fn append_context<T: Clone>(
    context: &ProcessingContext<T>,
    contribution: T,
) -> Option<ProcessingContext<T>> {
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
    use std::sync::Arc;

    #[test]
    fn computation_reuses_a2_context_allocations() {
        let root = new_context("example", b"event");
        let entry = Arc::new("immutable annotation");
        let first = append_context(&root, entry.clone()).unwrap();
        let left = append_context(&first, entry.clone()).unwrap();
        let right = append_context(&first, entry.clone()).unwrap();
        assert!(Arc::ptr_eq(&root.root, &left.root));
        assert!(Arc::ptr_eq(
            left.head.as_ref().unwrap().parent.as_ref().unwrap(),
            first.head.as_ref().unwrap()
        ));
        assert!(Arc::ptr_eq(
            left.head.as_ref().unwrap().parent.as_ref().unwrap(),
            right.head.as_ref().unwrap().parent.as_ref().unwrap()
        ));
        assert!(!Arc::ptr_eq(
            left.head.as_ref().unwrap(),
            right.head.as_ref().unwrap()
        ));
        assert!(Arc::ptr_eq(&left.entries().next().unwrap(), &entry));
        assert_eq!(root.len(), 0);
        assert_eq!(first.len(), 1);
    }

    #[test]
    fn append_overflow_preserves_existing_context() {
        let mut context = new_context("example", b"event");
        context.len = usize::MAX;
        assert!(append_context(&context, "entry").is_none());
        assert_eq!(context.len(), usize::MAX);
        assert!(context.head.is_none());
    }

    #[test]
    fn context_id_preserves_opaque_identity_and_field_boundaries() {
        let left = new_context::<()>("a", b"bc");
        let right = new_context::<()>("ab", b"c");
        assert_ne!(left.root, right.root);
        assert_eq!(left.root, new_context::<()>("a", b"bc").root);
        assert_ne!(
            new_context::<()>("source", &[0, 0xff]).root,
            new_context::<()>("source", &[0xff, 0]).root
        );
    }
}
