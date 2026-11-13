// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use bytes::Bytes;
use drasi_core::interface::{
    CheckpointKey, CheckpointStore, IndexError, OutputPartition, SourceCheckpoint,
};
use serde::{Deserialize, Serialize};

pub const PENDING_QUERY_OUTPUT: &str = "\0computation:pending-output:v1";

/// Existing non-atomic publication protocol. The evaluating marker commits
/// before evaluation; a prepared sequence commits with evaluation/input state;
/// completion commits only after output publication succeeds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryPublicationCheckpoint {
    Evaluating,
    Pending(u64),
    Complete,
}

impl QueryPublicationCheckpoint {
    pub fn checkpoint(self) -> SourceCheckpoint {
        match self {
            Self::Evaluating => {
                SourceCheckpoint::new(0, Some(Bytes::from_static(b"evaluating-v1")))
            }
            Self::Pending(sequence) => SourceCheckpoint::new(sequence, None),
            Self::Complete => SourceCheckpoint::new(0, None),
        }
    }

    pub fn is_incomplete(checkpoint: Option<&SourceCheckpoint>) -> bool {
        checkpoint.is_some_and(|marker| marker.sequence > 0 || marker.source_position.is_some())
    }

    pub async fn stage(self, store: &dyn CheckpointStore) -> Result<(), IndexError> {
        let marker = self.checkpoint();
        store
            .stage_checkpoint(
                &CheckpointKey::Metadata(PENDING_QUERY_OUTPUT.to_owned()),
                marker.sequence,
                marker.source_position.as_ref(),
            )
            .await
    }
}

/// The reset marker lives outside the checkpoint namespace being cleared.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueryResetMarker {
    pub sequence: u64,
    pub in_progress: bool,
    pub generation: u64,
}

impl QueryResetMarker {
    pub fn next_generation(
        visible: u64,
        persisted: u64,
        previous: Option<&Self>,
    ) -> anyhow::Result<u64> {
        visible
            .max(persisted)
            .max(previous.map_or(0, |marker| marker.generation))
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("query output generation exhausted"))
    }
}

pub fn query_reset_key(query: &drasi_core::ComponentId) -> OutputPartition {
    OutputPartition::Component(query.clone()).metadata("computation-reset-v1")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publication_checkpoints_preserve_the_existing_protocol() {
        let evaluating = QueryPublicationCheckpoint::Evaluating.checkpoint();
        assert_eq!(evaluating.sequence, 0);
        assert_eq!(
            evaluating.source_position.as_deref(),
            Some(b"evaluating-v1".as_slice())
        );
        for state in [
            QueryPublicationCheckpoint::Evaluating,
            QueryPublicationCheckpoint::Pending(7),
        ] {
            assert!(QueryPublicationCheckpoint::is_incomplete(Some(
                &state.checkpoint()
            )));
        }
        assert!(!QueryPublicationCheckpoint::is_incomplete(None));
        assert!(!QueryPublicationCheckpoint::is_incomplete(Some(
            &QueryPublicationCheckpoint::Complete.checkpoint(),
        )));
        assert!(QueryPublicationCheckpoint::is_incomplete(Some(
            &SourceCheckpoint::new(0, Some(Bytes::from_static(b"unknown interrupted state"))),
        )));
    }

    #[test]
    fn reset_generation_exceeds_every_retained_frontier() {
        let marker = QueryResetMarker {
            sequence: 12,
            in_progress: true,
            generation: 7,
        };
        assert_eq!(
            QueryResetMarker::next_generation(3, 5, Some(&marker)).unwrap(),
            8
        );
        assert_eq!(
            QueryResetMarker::next_generation(9, 5, Some(&marker)).unwrap(),
            10
        );
        assert_eq!(
            QueryResetMarker::next_generation(3, 11, Some(&marker)).unwrap(),
            12
        );
        assert!(QueryResetMarker::next_generation(0, u64::MAX, None).is_err());
        assert_eq!(
            query_reset_key(
                &drasi_computation_contracts::ComponentId::try_new(
                    "00000000-0000-0000-0000-0000000002cd"
                )
                .expect("fixture UUID")
            ),
            drasi_core::interface::OutputPartition::Metadata {
                output: Box::new(drasi_core::interface::OutputPartition::Component(
                    drasi_computation_contracts::ComponentId::try_new(
                        "00000000-0000-0000-0000-0000000002cd"
                    )
                    .expect("fixture UUID")
                )),
                key: "computation-reset-v1".to_owned(),
            }
        );
    }
}
