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

use std::time::Duration;

use drasi_lib::{
    queries::OutboxResponse, reactions::ReactionCheckpoint, state_store::StateStoreProvider,
    DrasiLib, ExecutionMode,
};

pub fn execution_mode() -> ExecutionMode {
    match std::env::var("DRASI_TEST_EXECUTION").as_deref() {
        Err(std::env::VarError::NotPresent) | Ok("component") => ExecutionMode::ComponentGraph,
        #[cfg(feature = "computation-tests")]
        Ok("computation") => ExecutionMode::ComputationGraph,
        #[cfg(not(feature = "computation-tests"))]
        Ok("computation") => {
            panic!("DRASI_TEST_EXECUTION=computation requires --features computation-tests")
        }
        mode => panic!("unsupported DRASI_TEST_EXECUTION: {mode:?}"),
    }
}

pub async fn outbox(core: &DrasiLib, query_id: &str) -> OutboxResponse {
    core.query_manager()
        .get_query_instance(query_id)
        .await
        .expect("query instance")
        .fetch_outbox(0)
        .await
        .expect("retained query outbox")
}

pub async fn wait_for_outbox(core: &DrasiLib, query_id: &str, expected: u64) -> OutboxResponse {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let observed = outbox(core, query_id).await;
        if observed.latest_sequence >= expected {
            assert_eq!(observed.latest_sequence, expected);
            assert_eq!(
                observed
                    .results
                    .iter()
                    .map(|result| result.sequence)
                    .collect::<Vec<_>>(),
                (1..=expected).collect::<Vec<_>>(),
                "outbox must contain the exact ordered result history"
            );
            return observed;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "query {query_id} did not reach {expected}; outbox is {observed:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

pub async fn checkpoint(
    store: &dyn StateStoreProvider,
    reaction_id: &str,
    query_id: &str,
) -> Option<ReactionCheckpoint> {
    store
        .get(reaction_id, &format!("checkpoint:{query_id}"))
        .await
        .expect("read reaction checkpoint")
        .map(|bytes| bincode::deserialize(&bytes).expect("decode reaction checkpoint"))
}

pub async fn wait_for_checkpoint(
    core: &DrasiLib,
    store: &dyn StateStoreProvider,
    reaction_id: &str,
    query_id: &str,
    expected: u64,
) -> ReactionCheckpoint {
    let config_hash = outbox(core, query_id).await.config_hash;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let observed = checkpoint(store, reaction_id, query_id).await;
        if let Some(checkpoint) = &observed {
            assert_eq!(
                checkpoint.config_hash, config_hash,
                "query identity changed"
            );
            assert!(
                checkpoint.sequence <= expected,
                "checkpoint advanced past {expected}: {checkpoint:?}"
            );
            if checkpoint.sequence == expected {
                return checkpoint.clone();
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "reaction {reaction_id} did not checkpoint {expected}: {observed:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
