// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use crate::{ComponentId, SourceProgressKey, MAX_SOURCE_POSITION_BYTES};
use std::sync::Arc;

/// Source-defined ordering for an event and a consumer's handled position.
/// This need not be a total order: partitioned sources compare the event's
/// partition, not unrelated offsets. Both arguments use checkpointable source
/// positions. Return false only when the event is covered, and fail for invalid
/// or incomparable positions. Self-comparison must return false; the host uses
/// it to validate a first position before saving that position as a checkpoint.
pub trait InputPositionComparator: Send + Sync + 'static {
    fn is_after(&self, event: &[u8], handled: &[u8]) -> anyhow::Result<bool>;
}

/// Immutable, bounded cursor semantics captured from a producer at construction.
/// The comparator performs no I/O and never reads mutable component state.
#[derive(Clone)]
pub struct InputPositionPolicy {
    key: SourceProgressKey,
    comparator: Arc<dyn InputPositionComparator>,
}

impl std::fmt::Debug for InputPositionPolicy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InputPositionPolicy")
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

impl InputPositionPolicy {
    pub fn new(
        key: SourceProgressKey,
        comparator: Arc<dyn InputPositionComparator>,
    ) -> anyhow::Result<Self> {
        if let SourceProgressKey::Source(source) = &key {
            ComponentId::try_new(source.as_str())?;
        }
        Ok(Self { key, comparator })
    }

    pub fn key(&self) -> &SourceProgressKey {
        &self.key
    }

    pub fn is_after(&self, event: &[u8], handled: &[u8]) -> anyhow::Result<bool> {
        anyhow::ensure!(
            event.len() <= MAX_SOURCE_POSITION_BYTES && handled.len() <= MAX_SOURCE_POSITION_BYTES,
            "input replay position exceeds the source cursor limit"
        );
        self.comparator.is_after(event, handled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Partitioned;

    impl InputPositionComparator for Partitioned {
        fn is_after(&self, event: &[u8], handled: &[u8]) -> anyhow::Result<bool> {
            let [partition, _, _] = event else {
                anyhow::bail!("invalid partitioned event position");
            };
            anyhow::ensure!(
                handled.len() == event.len() && *partition < 2 && handled[0] < 2,
                "invalid partitioned handled position"
            );
            let index = usize::from(*partition) + 1;
            let previous = handled
                .get(index)
                .ok_or_else(|| anyhow::anyhow!("unknown partition in handled position"))?;
            Ok(event[index] > *previous)
        }
    }

    #[test]
    fn replay_ordering_is_source_defined_not_lexicographic_or_a_shared_counter() {
        let policy = InputPositionPolicy::new(
            SourceProgressKey::Source("partitioned-source".into()),
            Arc::new(Partitioned),
        )
        .unwrap();
        assert!(policy.is_after(&[0, 7, 90], &[1, 6, 90]).unwrap());
        assert!(!policy.is_after(&[1, 6, 89], &[0, 6, 90]).unwrap());
        assert!(!policy.is_after(&[1, 6, 90], &[0, 6, 90]).unwrap());
        assert!(!policy.is_after(&[0, 7, 90], &[0, 7, 90]).unwrap());
        assert!(policy.is_after(&[2, 1, 0], &[0, 6, 90]).is_err());
        assert!(policy.is_after(&[0], &[0, 6, 90]).is_err());
        assert!(policy
            .is_after(&vec![0; MAX_SOURCE_POSITION_BYTES + 1], &[0])
            .is_err());
        assert!(InputPositionPolicy::new(
            SourceProgressKey::Source(String::new()),
            Arc::new(Partitioned),
        )
        .is_err());
    }
}
