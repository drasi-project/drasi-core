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

use drasi_lib::{DrasiLib, DrasiLibBuilder};

pub fn builder() -> DrasiLibBuilder {
    DrasiLib::builder().with_query_factory(
        drasi_lib::computation::v1::FactoryRegistry::standard()
            .get(
                &drasi_lib::computation::v1::ImplementationIdentity::try_new(
                    "drasi/continuous-query",
                    "1",
                )
                .expect("reference query identity"),
            )
            .expect("reference query factory")
            .clone(),
    )
}

#[allow(dead_code)]
pub async fn wait_for_gap_recovery(
    core: &DrasiLib,
    store: &dyn drasi_lib::StateStoreProvider,
    reaction: &drasi_core_types::ComponentName,
    query: &drasi_core_types::ComponentName,
    produced: u64,
) -> anyhow::Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let output = core.get_query_output_metrics_by_name(query).await?;
            // Covered broadcast lag can be replayed without recording an outbox gap.
            if output.outbox_latest_seq >= produced {
                let reaction_id = core
                    .component_by_name(reaction)
                    .map_err(|error| anyhow::anyhow!("reaction {reaction} not found: {error}"))?
                    .id()
                    .clone();
                let query_id = core
                    .component_by_name(query)
                    .map_err(|error| anyhow::anyhow!("query {query} not found: {error}"))?
                    .id()
                    .clone();
                let partition = drasi_core_types::StorePartition::component(reaction_id);
                if let Some(bytes) = store
                    .get(&partition, &format!("checkpoint:{query_id}"))
                    .await?
                {
                    let checkpoint: drasi_lib::reactions::checkpoint::ReactionCheckpoint =
                        bincode::deserialize(&bytes)?;
                    if checkpoint.sequence >= produced {
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
