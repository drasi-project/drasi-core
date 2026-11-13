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

#![cfg(test)]
#![cfg(feature = "garnet-tests")]

use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use drasi_core::{
    computation::ComputationIndexProvider,
    interface::OutputPartition,
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
};
use drasi_index_garnet::{computation::GarnetComputationProvider, GarnetIndexProvider};
use drasi_lib::computation::v1::*;
use std::{num::NonZeroUsize, sync::Arc};

fn input(sequence: u64) -> InputEnvelope {
    let element = Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("source", "one"),
            labels: vec!["Item".into()].into(),
            effective_from: sequence,
        },
        properties: ElementPropertyMap::from(
            serde_json::json!({"name": format!("name-{sequence}"), "active": true}),
        ),
    };
    InputEnvelope {
        port: PortId::try_new("in").expect("port"),
        envelope: GraphChangeCodec::encode_change(
            if sequence == 1 {
                SourceChange::Insert { element }
            } else {
                SourceChange::update(element)
            },
            StreamId::try_new("source/out").expect("stream"),
            sequence,
            None,
        )
        .expect("input"),
    }
}

struct Bootstrap;

#[async_trait]
impl ComputationBootstrapProvider for Bootstrap {
    async fn snapshot(&self) -> Result<ComputationBootstrapSnapshot> {
        Ok(ComputationBootstrapSnapshot {
            changes: Box::pin(futures::stream::iter([Ok(input(1).envelope)])),
            watermarks: vec![BootstrapWatermark {
                producer: None,
                stream: StreamId::try_new("source/out")?,
                source_id: None,
                sequence: 1,
                position: None,
            }],
        })
    }
}

fn definition(graph: &str, changed: bool) -> ContinuousQueryDefinition {
    ContinuousQueryDefinition {
        graph_id: graph.into(),
        id: ComponentId::try_new("70a753ff-f396-5582-91b3-1c2f5948550c").expect("id"),
        query: if changed {
            "MATCH (n:Item) RETURN n.name AS name, n.active AS active"
        } else {
            "MATCH (n:Item) RETURN n.name AS name"
        }
        .into(),
        language: ComputationQueryLanguage::Cypher,
        output_stream: StreamId::try_new("query/out").expect("stream"),
        outbox_capacity: NonZeroUsize::new(2).expect("capacity"),
    }
}

async fn exercise(provider: Arc<dyn ComputationIndexProvider>, graph: &str) -> Result<()> {
    {
        let mut query = ContinuousQueryTransformer::new(definition(graph, false), provider.clone())
            .await?
            .with_bootstrap(Arc::new(Bootstrap));
        query.start().await?;
        ensure!(query.transform(input(2)).await?.len() == 1);
        ensure!(query.transform(input(3)).await?.len() == 1);
        query.stop().await?;
    }
    {
        let mut query = ContinuousQueryTransformer::new(definition(graph, false), provider.clone())
            .await?
            .with_bootstrap(Arc::new(Bootstrap));
        query.start().await?;
        ensure!(query.results().snapshot()?.as_of_sequence == 2);
        ensure!(query.results().replay(0)?.len() == 2);
        ensure!(query.transform(input(3)).await?.is_empty());
        query.stop().await?;
    }
    let generation;
    {
        let mut query = ContinuousQueryTransformer::new_with_options(
            definition(graph, true),
            provider.clone(),
            QueryOptions {
                recovery: QueryRecoveryPolicy::AutoReset,
                publication: QueryPublicationMode::Atomic,
            },
        )
        .await?
        .with_bootstrap(Arc::new(Bootstrap));
        query.start().await?;
        let snapshot = query.results().snapshot()?;
        generation = snapshot.generation;
        ensure!(generation > 0);
        ensure!(snapshot.rows.len() == 1);
        ensure!(snapshot.as_of_sequence == 2);
        query.stop().await?;
    }
    let mut query = ContinuousQueryTransformer::new(definition(graph, true), provider)
        .await?
        .with_bootstrap(Arc::new(Bootstrap));
    query.start().await?;
    let snapshot = query.results().snapshot()?;
    ensure!(snapshot.generation == generation);
    ensure!(snapshot.as_of_sequence == 2);
    ensure!(snapshot.rows.len() == 1);
    ensure!(query.transform(input(1)).await?.is_empty());
    let output = query.transform(input(2)).await?;
    ensure!(output.len() == 1);
    ensure!(output[0].envelope.system().sequence() == 3);
    query.stop().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires a Docker-compatible Redis container runtime"]
async fn garnet_native_and_standard_adapters_recover_primary_output_and_separate_reset_markers(
) -> Result<()> {
    let redis = shared_tests::redis_helpers::setup_redis().await;
    let result = async {
        let native = Arc::new(GarnetComputationProvider::new(redis.url(), false));
        exercise(native, "native-garnet").await?;
        let adapter = LegacyIndexProviderAdapter::scoped(
            Arc::new(GarnetIndexProvider::new(redis.url(), None, false)),
            "instance",
        )?;
        let result = exercise(adapter.clone(), "ordinary-garnet").await;
        adapter.shutdown().await?;
        result
    }
    .await;
    redis.cleanup().await;
    result
}

#[tokio::test]
#[ignore = "requires a Docker-compatible Redis container runtime"]
async fn native_query_rejects_unqualified_garnet_without_touching_storage() -> Result<()> {
    let redis = shared_tests::redis_helpers::setup_redis().await;
    let result = async {
        let provider = Arc::new(GarnetComputationProvider::new(redis.url(), false));
        let plugin = drasi_host_sdk::computation::NativePlugin::from_definition(
            drasi_computation_transformer_query::plugin()?,
        )?;
        let factory = plugin
            .factories()
            .iter()
            .find(|factory| {
                factory.metadata().implementation.name.as_ref()
                    == "drasi.experimental/indexed-query"
            })
            .context("indexed native query")?;
        let query_id = ComponentId::try_new("70a753ff-f396-5582-91b3-1c2f5948550c")?;
        let output = OutputPartition::Component(query_id.clone());
        let mut query = factory.create_component_with_processing(
            query_id.clone(),
            serde_json::json!({"query":"MATCH (n:Item) RETURN n.name AS name"}),
            drasi_computation_plugin_sdk::Scope {
                instance_id: "native-garnet-capabilities".into(),
                graph_id: "native-garnet-capabilities".into(),
                generation: 1,
            },
            provider.clone(),
        )?;
        query.initialize().await?;
        let error = query
            .start()
            .await
            .expect_err("unknown storage durability cannot be upgraded");
        ensure!(format!("{error:#}").contains("not guaranteed"), "{error:#}");
        query.stop().await?;
        drop(query);
        let indexes = provider
            .create_indexes("native-garnet-capabilities", &query_id)
            .await?;
        let checkpoint = indexes.checkpoint_store().context("Garnet checkpoints")?;
        ensure!(checkpoint.read_all_checkpoints().await?.is_empty());
        ensure!(checkpoint.read_output_generation(&output).await?.is_none());
        ensure!(checkpoint.read_result_sequence(&output).await?.is_none());
        ensure!(indexes
            .live_results_writer()
            .context("Garnet live rows")?
            .read_snapshot(&query_id)
            .await?
            .is_empty());
        ensure!(indexes
            .outbox_writer()
            .context("Garnet outbox")?
            .read_from(&output, 0)
            .await?
            .is_empty());
        Ok(())
    }
    .await;
    redis.cleanup().await;
    result
}
