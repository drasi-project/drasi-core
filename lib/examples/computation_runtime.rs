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

//! Native application components in the DrasiLib-owned ComputationGraph.
//! Run: cargo run -p drasi-lib --example computation_runtime

use drasi_core::computation::InMemoryComputationProvider;
use drasi_lib::{computation::v1::*, DrasiLib};
use drasi_reaction_application::NativeApplicationReaction;
use drasi_source_application::{NativeApplicationSource, PropertyMapBuilder};
use std::{num::NonZeroUsize, sync::Arc, time::Duration};

#[tokio::main]
#[allow(clippy::print_stdout)]
async fn main() -> anyhow::Result<()> {
    const INSTANCE: &str = "native-runtime-example";
    let drasi = DrasiLib::builder().with_id(INSTANCE).build().await?;
    let result = async {
        drasi.start().await?;
        let control = drasi.computation_control()?;
        let graph_id = control.desired_snapshot().id.to_string();
        let capacity = NonZeroUsize::new(16).expect("nonzero capacity");
        let source_stream = StreamId::try_new("orders/out")?;
        let query_stream = StreamId::try_new("orders-query/out")?;
        let (source, input) = NativeApplicationSource::channel(
            INSTANCE,
            graph_id.clone(),
            "orders",
            source_stream.clone(),
            capacity,
        )?;
        let source_output = source.output();
        let query_id = ComponentId::try_new("orders-query")?;
        let query = ContinuousQueryTransformer::new(
            ContinuousQueryDefinition {
                graph_id,
                id: query_id.clone(),
                query: "MATCH (o:Order) RETURN o.name AS name".into(),
                language: ComputationQueryLanguage::Cypher,
                output_stream: query_stream.clone(),
                outbox_capacity: capacity,
            },
            Arc::new(InMemoryComputationProvider),
        )
        .await?
        .with_recovery_scope(INSTANCE)?;
        let (reaction, mut receiver) = NativeApplicationReaction::channel(
            "output",
            QueryChangeCodec::schema().descriptor().clone(),
            capacity,
        )?;
        let reaction_input = reaction.input();
        let query_input = Endpoint::new(query_id.clone(), PortId::try_new("in")?);
        let query_output = Endpoint::new(query_id, PortId::try_new("out")?);
        let mut members = Vec::new();
        for addition in [
            ComponentAddition::new(ConstructedComponent::source(Box::new(source)))
                .bind_stream(PortId::try_new("out")?, source_stream),
            ComponentAddition::new(ConstructedComponent::query(Box::new(query)))
                .bind_stream(PortId::try_new("out")?, query_stream),
            ComponentAddition::new(ConstructedComponent::sink(Box::new(reaction))),
        ] {
            members.push(addition.definition.descriptor.id().clone());
            control.add_component(addition.defer_activation()).await?;
        }
        for (from, to) in [(source_output, query_input), (query_output, reaction_input)] {
            let connected = control
                .connect(
                    EdgeDefinition::new(from, to),
                    Box::new(BoundedPipeConfig { capacity: 16 }),
                    RelationshipPolicy::default(),
                )
                .await?;
            anyhow::ensure!(connected.committed, "{connected:?}");
        }
        let started = control
            .start_components(
                control.desired_snapshot().revision,
                GraphSelection::Exact(members),
            )
            .await?;
        anyhow::ensure!(
            started.summary == OperationSummary::Completed,
            "{started:?}"
        );
        input
            .send_node_insert(
                "one",
                vec!["Order"],
                PropertyMapBuilder::new()
                    .with_string("name", "First order")
                    .build(),
            )
            .await?;
        let result = tokio::time::timeout(Duration::from_secs(5), receiver.recv())
            .await?
            .ok_or_else(|| anyhow::anyhow!("output closed"))?;
        println!(
            "ComputationGraph emitted {} result changes",
            result.envelope.changes().operations().len()
        );
        println!(
            "Current rows: {:?}",
            drasi.get_query_results("orders-query").await?
        );
        println!(
            "Query metrics: {:?}",
            drasi.get_query_output_metrics("orders-query").await?
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let shutdown = drasi.shutdown().await;
    result?;
    shutdown?;
    Ok(())
}
