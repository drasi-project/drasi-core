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
    DrasiLib::builder()
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
            // Covered broadcast lag can be replayed without recording an outbox gap.
            if output.outbox_latest_seq >= produced {
                if let Some(bytes) = store.get(reaction, &format!("checkpoint:{query}")).await? {
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
