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

use crate::{
    evaluation::temporal::{
        codec::TemporalCodecError, EpochState, QueryEpoch, TemporalBatch, TemporalKey,
        TemporalNamespace, TemporalRecord, TemporalStateError,
    },
    interface::TemporalIndex,
};

use super::TemporalRuntimeError;

#[derive(Debug, Clone, Copy)]
pub enum EpochRequest {
    Disabled,
    /// The enclosing query descriptor must establish that this is a new query, not a restart.
    /// Allocate a new namespace; do not infer freshness from an absent manifest.
    Fresh(TemporalNamespace),
    /// The epoch comes from persisted query metadata; it must not be regenerated on startup.
    Reopen(TemporalNamespace),
}

#[derive(Debug)]
#[must_use = "stage initialization with the query descriptor in the same root"]
pub enum PreparedEpoch {
    Disabled,
    Reopened(EpochState),
    Initialize {
        state: EpochState,
        batch: TemporalBatch,
    },
}

/// Missing state on reopen is a migration error. It is never interpreted as a fresh query.
/// This helper deliberately does not choose an epoch, write metadata, or clear other indexes.
pub async fn prepare_epoch(
    index: &dyn TemporalIndex,
    request: EpochRequest,
) -> Result<PreparedEpoch, TemporalRuntimeError> {
    let namespace = match request {
        EpochRequest::Disabled => return Ok(PreparedEpoch::Disabled),
        EpochRequest::Fresh(namespace) | EpochRequest::Reopen(namespace) => namespace,
    };
    let record = index.get(&TemporalKey::Epoch(namespace)).await?;
    let existing = match record {
        Some(TemporalRecord::Epoch(state)) => {
            if state.namespace != namespace {
                return Err(TemporalStateError::NamespaceMismatch.into());
            }
            Some(state)
        }
        Some(_) => return Err(TemporalRuntimeError::UnexpectedRecord("epoch manifest")),
        None => None,
    };
    match (request, existing) {
        (EpochRequest::Fresh(_), Some(_)) => {
            Err(TemporalRuntimeError::EpochAlreadyExists(namespace))
        }
        (EpochRequest::Fresh(_), None) => {
            let state = EpochState::new(namespace);
            let mut batch = TemporalBatch::new(namespace);
            batch.put(TemporalRecord::Epoch(state.clone()))?;
            Ok(PreparedEpoch::Initialize { state, batch })
        }
        (EpochRequest::Reopen(_), Some(state)) => Ok(PreparedEpoch::Reopened(state)),
        (EpochRequest::Reopen(_), None) => Err(TemporalCodecError::MigrationRequired.into()),
        (EpochRequest::Disabled, _) => Ok(PreparedEpoch::Disabled),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviousEpoch {
    Known(QueryEpoch),
    Legacy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetHistory {
    HistoricalReplay,
    SnapshotReset,
}

#[derive(Debug)]
#[must_use = "coordinate temporal, queue, graph, checkpoint, and output state in one reset"]
pub struct PreparedReset {
    pub previous: PreviousEpoch,
    pub history: ResetHistory,
    pub replacement: EpochState,
    pub initialize: TemporalBatch,
}

/// Explicit reset preparation can replace unreadable legacy history, but never in the old
/// epoch. The caller coordinates the old-epoch/queue removal and persisted query descriptor
/// with lib's checkpoint and output-watermark policy; no counters are reset here.
pub async fn prepare_reset(
    index: &dyn TemporalIndex,
    previous: PreviousEpoch,
    replacement: TemporalNamespace,
    history: ResetHistory,
) -> Result<PreparedReset, TemporalRuntimeError> {
    if previous == PreviousEpoch::Known(replacement.epoch) {
        return Err(TemporalRuntimeError::EpochReuse);
    }
    if index.get(&TemporalKey::Epoch(replacement)).await?.is_some() {
        return Err(TemporalRuntimeError::EpochAlreadyExists(replacement));
    }
    let state = EpochState::new(replacement);
    let mut initialize = TemporalBatch::new(replacement);
    initialize.put(TemporalRecord::Epoch(state.clone()))?;
    Ok(PreparedReset {
        previous,
        history,
        replacement: state,
        initialize,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;

    use super::*;
    use crate::{
        evaluation::temporal::{codec, fixtures, Incarnation, PlanVersion},
        interface::IndexError,
    };

    struct ManifestIndex {
        manifest: Option<Vec<u8>>,
        reads: AtomicUsize,
    }

    impl ManifestIndex {
        fn new(state: Option<EpochState>) -> Self {
            Self {
                manifest: state.map(|s| codec::encode_record(&TemporalRecord::Epoch(s)).unwrap()),
                reads: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl TemporalIndex for ManifestIndex {
        async fn load_catalog(
            &self,
        ) -> Result<Option<crate::evaluation::temporal::TemporalCatalog>, IndexError> {
            panic!("manifest preparation must not rediscover the query catalog");
        }

        async fn store_catalog(
            &self,
            _: crate::evaluation::temporal::TemporalCatalog,
        ) -> Result<(), IndexError> {
            panic!("epoch preparation must not update the query catalog");
        }

        async fn clear(&self) -> Result<(), IndexError> {
            panic!("epoch preparation must not clear query history");
        }

        async fn get(&self, key: &TemporalKey) -> Result<Option<TemporalRecord>, IndexError> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            self.manifest
                .as_ref()
                .map(|bytes| codec::decode_record(key, bytes).map_err(IndexError::other))
                .transpose()
        }

        async fn apply(&self, _: TemporalBatch) -> Result<(), IndexError> {
            panic!("epoch preparation must not stage a partial reset");
        }

        async fn clear_epoch(&self, _: QueryEpoch) -> Result<(), IndexError> {
            panic!("epoch preparation must not destroy old history");
        }
    }

    #[tokio::test]
    async fn disabled_parts_do_not_read_or_allocate_temporal_state() {
        let index = ManifestIndex::new(None);
        assert!(matches!(
            prepare_epoch(&index, EpochRequest::Disabled).await.unwrap(),
            PreparedEpoch::Disabled
        ));
        assert_eq!(index.reads.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn reopen_retains_the_incarnation_allocator() {
        let namespace = fixtures::namespace();
        let index = ManifestIndex::new(Some(EpochState {
            namespace,
            next_incarnation: Incarnation(41),
        }));
        let PreparedEpoch::Reopened(mut state) =
            prepare_epoch(&index, EpochRequest::Reopen(namespace))
                .await
                .unwrap()
        else {
            panic!("reopen initialized a new lifetime");
        };
        assert_eq!(state.allocate().unwrap(), Incarnation(41));
        assert!(matches!(
            prepare_epoch(&index, EpochRequest::Fresh(namespace)).await,
            Err(TemporalRuntimeError::EpochAlreadyExists(_))
        ));
    }

    #[tokio::test]
    async fn missing_manifest_is_legacy_on_reopen_but_explicitly_fresh_on_creation() {
        let namespace = fixtures::namespace();
        let index = ManifestIndex::new(None);
        assert!(matches!(
            prepare_epoch(&index, EpochRequest::Reopen(namespace)).await,
            Err(TemporalRuntimeError::Codec(
                TemporalCodecError::MigrationRequired
            ))
        ));
        let PreparedEpoch::Initialize { state, batch } =
            prepare_epoch(&index, EpochRequest::Fresh(namespace))
                .await
                .unwrap()
        else {
            panic!("fresh query was not initialized");
        };
        assert_eq!(state.next_incarnation, Incarnation(0));
        assert_eq!(batch.mutations().len(), 1);
    }

    #[tokio::test]
    async fn old_plan_or_corrupt_manifest_cannot_silently_restart_timers() {
        let namespace = fixtures::namespace();
        let index = ManifestIndex::new(Some(EpochState::new(namespace)));
        let newer_plan = TemporalNamespace {
            plan: PlanVersion(namespace.plan.0 + 1),
            ..namespace
        };
        assert!(prepare_epoch(&index, EpochRequest::Reopen(newer_plan))
            .await
            .is_err());
        let corrupt = ManifestIndex {
            manifest: Some(b"old timers".to_vec()),
            reads: AtomicUsize::new(0),
        };
        assert!(prepare_epoch(&corrupt, EpochRequest::Reopen(namespace))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn explicit_reset_requires_a_new_unoccupied_epoch_and_preserves_its_policy() {
        let namespace = fixtures::namespace();
        let index = ManifestIndex::new(None);
        assert!(matches!(
            prepare_reset(
                &index,
                PreviousEpoch::Known(namespace.epoch),
                namespace,
                ResetHistory::SnapshotReset
            )
            .await,
            Err(TemporalRuntimeError::EpochReuse)
        ));
        assert_eq!(index.reads.load(Ordering::Relaxed), 0);
        for history in [ResetHistory::HistoricalReplay, ResetHistory::SnapshotReset] {
            let reset = prepare_reset(&index, PreviousEpoch::Legacy, namespace, history)
                .await
                .unwrap();
            assert_eq!(reset.history, history);
            assert_eq!(reset.previous, PreviousEpoch::Legacy);
            assert_eq!(reset.replacement.namespace, namespace);
            assert_eq!(reset.initialize.mutations().len(), 1);
        }
        let occupied = ManifestIndex::new(Some(EpochState::new(namespace)));
        assert!(matches!(
            prepare_reset(
                &occupied,
                PreviousEpoch::Legacy,
                namespace,
                ResetHistory::SnapshotReset
            )
            .await,
            Err(TemporalRuntimeError::EpochAlreadyExists(_))
        ));
    }
}
