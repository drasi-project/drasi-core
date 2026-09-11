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

use drasi_lib::{DrasiLib, DrasiLibBuilder, ExecutionMode};

pub fn builder() -> DrasiLibBuilder {
    let mode = match std::env::var("DRASI_TEST_EXECUTION").as_deref() {
        Err(std::env::VarError::NotPresent) | Ok("component") => ExecutionMode::ComponentGraph,
        #[cfg(feature = "computation")]
        Ok("computation") => ExecutionMode::ComputationGraph,
        other => panic!(
            "unsupported test execution mode (computation requires its Cargo feature): {other:?}"
        ),
    };
    DrasiLib::builder().with_execution_mode(mode)
}

#[allow(dead_code)]
pub async fn wait_for_gap_recovery(
    core: &DrasiLib,
    store: &dyn drasi_lib::StateStoreProvider,
    reaction: &str,
    query: &str,
    produced: u64,
) -> anyhow::Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let output = core.get_query_output_metrics(query).await?;
            let reactions = core.get_reaction_metrics(reaction).await?;
            if output.outbox_latest_seq >= produced
                && reactions
                    .get(query)
                    .is_some_and(|metrics| metrics.gap_detection_count > 0)
            {
                if let Some(bytes) = store.get(reaction, &format!("checkpoint:{query}")).await? {
                    let checkpoint: drasi_lib::reactions::checkpoint::ReactionCheckpoint =
                        bincode::deserialize(&bytes)?;
                    if checkpoint.sequence > 0 {
                        return Ok::<_, anyhow::Error>(());
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await??;
    Ok(())
}
