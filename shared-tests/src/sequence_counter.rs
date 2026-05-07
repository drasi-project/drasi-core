// Copyright 2024 The Drasi Authors.
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

use std::sync::Arc;

use bytes::Bytes;
use drasi_core::interface::{
    CheckpointStore, ResultSequence, ResultSequenceCounter, SourceCheckpoint,
};

/// Basic result sequence counter test for ResultSequenceCounter implementations.
///
/// Validates apply_sequence / get_sequence round-trip.
pub async fn result_sequence_counter(subject: &dyn ResultSequenceCounter) {
    let result = subject.get_sequence().await.expect("get_sequence failed");
    assert_eq!(result, ResultSequence::default());

    subject
        .apply_sequence(2, "foo")
        .await
        .expect("apply_sequence failed");

    let result = subject.get_sequence().await.expect("get_sequence failed");
    assert_eq!(
        result,
        ResultSequence {
            sequence: 2,
            source_change_id: Arc::from("foo"),
        }
    );
}

/// Basic sequence counter test for CheckpointStore implementations.
///
/// Validates stage_checkpoint / read_checkpoint round-trip.
pub async fn sequence_counter(subject: &dyn CheckpointStore) {
    // Initially no checkpoint for "foo"
    let result = subject
        .read_checkpoint("foo")
        .await
        .expect("read_checkpoint failed");
    assert_eq!(result, None);

    subject
        .stage_checkpoint("foo", 2, None)
        .await
        .expect("stage_checkpoint failed");

    let result = subject
        .read_checkpoint("foo")
        .await
        .expect("read_checkpoint failed");
    assert_eq!(
        result,
        Some(SourceCheckpoint {
            sequence: 2,
            source_position: None,
        })
    );
}

/// Test checkpoint round-trip with per-source position storage.
///
/// Validates that `stage_checkpoint` / `read_checkpoint` correctly persist
/// and retrieve opaque source position bytes of different sizes, stored per-source:
/// - 8 bytes (Postgres WAL LSN)
/// - 20 bytes (MSSQL change tracking version)
/// - 80+ bytes (opaque tokens like MongoDB/Cosmos DB resume tokens)
pub async fn checkpoint_round_trip(subject: &dyn CheckpointStore) {
    // Initially returns no checkpoints
    let all = subject
        .read_all_checkpoints()
        .await
        .expect("read_all_checkpoints failed");
    assert!(all.is_empty());

    // Stage checkpoint with 8-byte position (Postgres-like)
    let pos_8 = Bytes::from_static(&[0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08]);
    subject
        .stage_checkpoint("source-pg", 10, Some(&pos_8))
        .await
        .expect("stage_checkpoint failed");

    let cp = subject
        .read_checkpoint("source-pg")
        .await
        .expect("read_checkpoint failed")
        .expect("expected checkpoint for source-pg");
    assert_eq!(cp.sequence, 10);
    assert_eq!(cp.source_position.as_ref(), Some(&pos_8));

    // Stage checkpoint with 20-byte position (MSSQL-like)
    let pos_20 = Bytes::from(vec![0xAA; 20]);
    subject
        .stage_checkpoint("source-mssql", 20, Some(&pos_20))
        .await
        .expect("stage_checkpoint failed");

    let cp = subject
        .read_checkpoint("source-mssql")
        .await
        .expect("read_checkpoint failed")
        .expect("expected checkpoint for source-mssql");
    assert_eq!(cp.sequence, 20);
    assert_eq!(cp.source_position.as_ref(), Some(&pos_20));

    // Previous source's checkpoint should still be there
    let all = subject
        .read_all_checkpoints()
        .await
        .expect("read_all_checkpoints failed");
    assert_eq!(all.len(), 2);
    assert_eq!(all["source-pg"].source_position.as_ref(), Some(&pos_8));
    assert_eq!(all["source-mssql"].source_position.as_ref(), Some(&pos_20));

    // Stage checkpoint with 80-byte position (Cosmos DB-like resume token)
    let pos_80 = Bytes::from(vec![0xBB; 80]);
    subject
        .stage_checkpoint("source-cosmos", 30, Some(&pos_80))
        .await
        .expect("stage_checkpoint failed");

    let all = subject
        .read_all_checkpoints()
        .await
        .expect("read_all_checkpoints failed");
    assert_eq!(all.len(), 3);
    assert_eq!(all["source-cosmos"].source_position.as_ref(), Some(&pos_80));
    assert_eq!(all["source-pg"].source_position.as_ref(), Some(&pos_8));
    assert_eq!(all["source-mssql"].source_position.as_ref(), Some(&pos_20));

    // Stage checkpoint with None position
    subject
        .stage_checkpoint("source-volatile", 40, None)
        .await
        .expect("stage_checkpoint failed");

    let cp = subject
        .read_checkpoint("source-volatile")
        .await
        .expect("read_checkpoint failed")
        .expect("expected checkpoint for source-volatile");
    assert_eq!(cp.sequence, 40);
    assert_eq!(cp.source_position, None);

    // All checkpoints remain
    let all = subject
        .read_all_checkpoints()
        .await
        .expect("read_all_checkpoints failed");
    assert_eq!(all.len(), 4);
    assert_eq!(all["source-cosmos"].source_position.as_ref(), Some(&pos_80));

    // Overwrite an existing source with new sequence and position
    let pos_updated = Bytes::from_static(&[0xFF; 8]);
    subject
        .stage_checkpoint("source-pg", 50, Some(&pos_updated))
        .await
        .expect("stage_checkpoint failed");

    let cp = subject
        .read_checkpoint("source-pg")
        .await
        .expect("read_checkpoint failed")
        .expect("expected checkpoint for source-pg");
    assert_eq!(cp.sequence, 50);
    assert_eq!(cp.source_position.as_ref(), Some(&pos_updated));
}
