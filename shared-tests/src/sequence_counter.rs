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
use drasi_core::interface::{ResultCheckpoint, ResultSequence, CheckpointStore};

pub async fn sequence_counter(subject: &dyn CheckpointStore) {
    let result = subject.get_sequence().await.expect("get_sequence failed");
    assert_eq!(result, ResultSequence::default());

    subject
        .apply_checkpoint(2, "foo", None)
        .await
        .expect("apply_checkpoint failed");

    let result = subject.get_sequence().await.expect("get_sequence failed");
    assert_eq!(
        result,
        ResultSequence {
            sequence: 2,
            source_id: Arc::from("foo"),
        }
    );
}

/// Test checkpoint round-trip with per-source position storage.
///
/// Validates that `apply_checkpoint` / `get_checkpoint` correctly persist
/// and retrieve opaque source position bytes of different sizes, stored per-source:
/// - 8 bytes (Postgres WAL LSN)
/// - 20 bytes (MSSQL change tracking version)
/// - 80+ bytes (opaque tokens like MongoDB/Cosmos DB resume tokens)
pub async fn checkpoint_round_trip(subject: &dyn CheckpointStore) {
    // Initially returns default checkpoint (no positions)
    let checkpoint = subject
        .get_checkpoint()
        .await
        .expect("get_checkpoint failed");
    assert_eq!(checkpoint, ResultCheckpoint::default());
    assert!(checkpoint.source_positions.is_empty());

    // Apply checkpoint with 8-byte position (Postgres-like)
    let pos_8 = Bytes::from_static(&[0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08]);
    subject
        .apply_checkpoint(10, "source-pg", Some(&pos_8))
        .await
        .expect("apply_checkpoint failed");

    let checkpoint = subject
        .get_checkpoint()
        .await
        .expect("get_checkpoint failed");
    assert_eq!(checkpoint.sequence, 10);
    assert_eq!(checkpoint.source_id.as_ref(), "source-pg");
    assert_eq!(checkpoint.get_source_position("source-pg"), Some(&pos_8));

    // Also verify via the trait's get_source_position method
    let pos = subject
        .get_source_position("source-pg")
        .await
        .expect("get_source_position failed");
    assert_eq!(pos.as_ref(), Some(&pos_8));

    // Apply checkpoint with 20-byte position (MSSQL-like)
    let pos_20 = Bytes::from(vec![0xAA; 20]);
    subject
        .apply_checkpoint(20, "source-mssql", Some(&pos_20))
        .await
        .expect("apply_checkpoint failed");

    let checkpoint = subject
        .get_checkpoint()
        .await
        .expect("get_checkpoint failed");
    assert_eq!(checkpoint.sequence, 20);
    assert_eq!(checkpoint.source_id.as_ref(), "source-mssql");
    assert_eq!(
        checkpoint.get_source_position("source-mssql"),
        Some(&pos_20)
    );
    // Previous source position should still be there
    assert_eq!(checkpoint.get_source_position("source-pg"), Some(&pos_8));

    // Apply checkpoint with 80-byte position (Cosmos DB-like resume token)
    let pos_80 = Bytes::from(vec![0xBB; 80]);
    subject
        .apply_checkpoint(30, "source-cosmos", Some(&pos_80))
        .await
        .expect("apply_checkpoint failed");

    let checkpoint = subject
        .get_checkpoint()
        .await
        .expect("get_checkpoint failed");
    assert_eq!(checkpoint.sequence, 30);
    assert_eq!(checkpoint.source_id.as_ref(), "source-cosmos");
    assert_eq!(
        checkpoint.get_source_position("source-cosmos"),
        Some(&pos_80)
    );
    // All previous positions remain
    assert_eq!(checkpoint.get_source_position("source-pg"), Some(&pos_8));
    assert_eq!(
        checkpoint.get_source_position("source-mssql"),
        Some(&pos_20)
    );

    // Apply checkpoint with None position — removes that source's position
    subject
        .apply_checkpoint(40, "source-volatile", None)
        .await
        .expect("apply_checkpoint failed");

    let checkpoint = subject
        .get_checkpoint()
        .await
        .expect("get_checkpoint failed");
    assert_eq!(checkpoint.sequence, 40);
    assert_eq!(checkpoint.source_id.as_ref(), "source-volatile");
    assert_eq!(checkpoint.get_source_position("source-volatile"), None);
    // Other sources' positions remain
    assert_eq!(
        checkpoint.get_source_position("source-cosmos"),
        Some(&pos_80)
    );

    // Verify that apply_checkpoint with None position is consistent with get_sequence
    subject
        .apply_checkpoint(50, "source-legacy", None)
        .await
        .expect("apply_checkpoint failed");

    let checkpoint = subject
        .get_checkpoint()
        .await
        .expect("get_checkpoint failed");
    assert_eq!(checkpoint.sequence, 50);
    assert_eq!(checkpoint.source_id.as_ref(), "source-legacy");
    let seq = subject.get_sequence().await.expect("get_sequence failed");
    assert_eq!(seq.sequence, 50);
    assert_eq!(seq.source_id.as_ref(), "source-legacy");
}
