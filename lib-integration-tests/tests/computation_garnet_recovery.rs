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

use anyhow::{ensure, Result};
use async_trait::async_trait;
use drasi_core::{
    computation::ComputationIndexProvider,
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
                SourceChange::Update { element }
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
        id: ComponentId::try_new("query").expect("id"),
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
