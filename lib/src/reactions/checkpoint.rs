// Copyright 2025 The Drasi Authors.
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

//! Checkpoint type for durable reactions.
//!
//! A [`ReactionCheckpoint`] records the last successfully processed sequence
//! number and the query configuration hash at that point. Reactions persist
//! checkpoints via the state store so they can resume from where they left off
//! after a restart.

use serde::{Deserialize, Serialize};

use crate::state_store::StateStoreProvider;

/// Persisted progress marker for a reaction's subscription to a single query.
///
/// Stored in the state store under the key `"checkpoint:{query_id}"` using
/// `bincode` serialization for compactness.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReactionCheckpoint {
    /// The last outbox sequence number that was successfully processed.
    pub sequence: u64,
    /// The query config hash at the time this checkpoint was written.
    pub config_hash: u64,
}

// ============================================================================
// Shared checkpoint I/O helpers
// ============================================================================

/// Read a single checkpoint from the state store.
///
/// Returns `Ok(None)` if the key does not exist.
pub(crate) async fn read_checkpoint(
    store: &dyn StateStoreProvider,
    reaction_id: &str,
    query_id: &str,
) -> anyhow::Result<Option<ReactionCheckpoint>> {
    let key = format!("checkpoint:{query_id}");
    match store.get(reaction_id, &key).await? {
        Some(bytes) => {
            let cp: ReactionCheckpoint = bincode::deserialize(&bytes)
                .map_err(|e| anyhow::anyhow!("Failed to deserialize checkpoint: {e}"))?;
            Ok(Some(cp))
        }
        None => Ok(None),
    }
}

/// Read checkpoints for all given query IDs in a single batch.
pub(crate) async fn read_checkpoints_batch(
    store: &dyn StateStoreProvider,
    reaction_id: &str,
    query_ids: &[String],
) -> anyhow::Result<std::collections::HashMap<String, ReactionCheckpoint>> {
    let keys: Vec<String> = query_ids
        .iter()
        .map(|q| format!("checkpoint:{q}"))
        .collect();
    let key_refs: Vec<&str> = keys.iter().map(|k| k.as_str()).collect();

    let raw = store.get_many(reaction_id, &key_refs).await?;

    let mut result = std::collections::HashMap::new();
    for (key, bytes) in raw {
        let qid = key.strip_prefix("checkpoint:").unwrap_or(&key).to_string();
        let cp: ReactionCheckpoint = bincode::deserialize(&bytes).map_err(|e| {
            anyhow::anyhow!("Failed to deserialize checkpoint for query '{qid}': {e}")
        })?;
        result.insert(qid, cp);
    }
    Ok(result)
}

/// Write a single checkpoint to the state store.
pub(crate) async fn write_checkpoint(
    store: &dyn StateStoreProvider,
    reaction_id: &str,
    query_id: &str,
    checkpoint: &ReactionCheckpoint,
) -> anyhow::Result<()> {
    let key = format!("checkpoint:{query_id}");
    let bytes = bincode::serialize(checkpoint)
        .map_err(|e| anyhow::anyhow!("Failed to serialize checkpoint: {e}"))?;
    store.set(reaction_id, &key, bytes).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bincode_round_trip() {
        let checkpoint = ReactionCheckpoint {
            sequence: 42,
            config_hash: 0xDEAD_BEEF,
        };
        let bytes = bincode::serialize(&checkpoint).unwrap();
        let decoded: ReactionCheckpoint = bincode::deserialize(&bytes).unwrap();
        assert_eq!(checkpoint, decoded);
    }

    #[test]
    fn serde_json_round_trip() {
        let checkpoint = ReactionCheckpoint {
            sequence: 100,
            config_hash: 12345,
        };
        let json = serde_json::to_string(&checkpoint).unwrap();
        let decoded: ReactionCheckpoint = serde_json::from_str(&json).unwrap();
        assert_eq!(checkpoint, decoded);
    }
}
