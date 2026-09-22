// Copyright 2026 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#![cfg(feature = "computation-middleware-tests")]

use anyhow::Result;
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
    SourceMiddlewareConfig,
};
use drasi_core::{evaluation::variable_value::VariableValue, middleware::MiddlewareTypeRegistry};
use drasi_index_rocksdb::RocksDbIndexProvider;
use drasi_lib::{
    channels::{SourceEvent, SourceEventWrapper},
    computation::v1::*,
    DrasiLib,
};
use std::{num::NonZeroUsize, sync::Arc};

fn definition() -> Result<MiddlewareTransformerDefinition> {
    Ok(MiddlewareTransformerDefinition {
        id: ComponentId::try_new("middleware")?,
        output_stream: StreamId::try_new("middleware/out")?,
        middleware: vec![SourceMiddlewareConfig::new(
            "unwind",
            "children",
            serde_json::json!({"Parent": [{
                "selector":"$.items[*]", "label":"Child", "key":"$.id"
            }]})
            .as_object()
            .expect("config")
            .clone(),
        )],
        pipeline: vec!["children".into()],
    })
}

fn input(sequence: u64, update: bool) -> Result<InputEnvelope> {
    source_input(sequence, update, true)
}

fn source_input(sequence: u64, update: bool, raw_metadata: bool) -> Result<InputEnvelope> {
    let element = Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("source", "parent"),
            labels: Arc::from([Arc::from("Parent")]),
            effective_from: sequence,
        },
        properties: ElementPropertyMap::from(if update {
            serde_json::json!({"items":[{"id":"a"}]})
        } else {
            serde_json::json!({"items":[{"id":"a"},{"id":"b"}]})
        }),
    };
    let change = if update {
        SourceChange::Update { element }
    } else {
        SourceChange::Insert { element }
    };
    let timestamp = chrono::DateTime::from_timestamp_millis(sequence as i64).expect("time");
    if !raw_metadata {
        return Ok(InputEnvelope {
            port: PortId::try_new("in")?,
            envelope: GraphChangeCodec::encode_change(
                change,
                StreamId::try_new("source/out")?,
                sequence,
                Some(timestamp),
            )?,
        });
    }
    let source = SourceEventWrapper::with_sequence(
        "source".into(),
        SourceEvent::Change(change),
        timestamp,
        sequence,
        None,
    );
    Ok(InputEnvelope {
        port: PortId::try_new("in")?,
        envelope: GraphChangeCodec::encode_source_event(
            Arc::new(source),
            &ComponentId::try_new("source")?,
            StreamId::try_new("source/out")?,
            sequence,
            None,
        )?,
    })
}

async fn persistent_query(path: &std::path::Path) -> Result<ContinuousQueryTransformer> {
    let mut query = ContinuousQueryTransformer::new(
        ContinuousQueryDefinition {
            graph_id: "middleware-durability".into(),
            id: ComponentId::try_new("query")?,
            query: "MATCH (n:Child) RETURN n.id AS id".into(),
            language: ComputationQueryLanguage::Cypher,
            output_stream: StreamId::try_new("query/out")?,
            outbox_capacity: NonZeroUsize::new(8).expect("capacity"),
        },
        LegacyIndexProviderAdapter::new(Arc::new(RocksDbIndexProvider::new(path, false, false))),
    )
    .await?;
    query.start().await?;
    Ok(query)
}

async fn durable_middleware(
    path: &std::path::Path,
    registry: Arc<MiddlewareTypeRegistry>,
    capacity: usize,
) -> Result<MiddlewareTransformer> {
    let mut middleware = MiddlewareTransformer::new_durable(
        definition()?,
        registry,
        LegacyIndexProviderAdapter::new(Arc::new(RocksDbIndexProvider::new(path, false, false))),
        DurableMiddlewareOptions {
            graph_id: "middleware-durability".into(),
            outbox_capacity: NonZeroUsize::new(capacity).expect("capacity"),
        },
    )
    .await?;
    middleware.start().await?;
    Ok(middleware)
}

fn child_ids(query: &ContinuousQueryTransformer) -> Result<Vec<String>> {
    let mut values = Vec::new();
    for row in query.results().snapshot()?.rows.values() {
        let row = QueryChangeCodec::decode_row(row)?;
        let Some(VariableValue::String(value)) = row.values.get("id") else {
            anyhow::bail!("query row omitted its child ID");
        };
        values.push(value.to_string());
    }
    values.sort();
    Ok(values)
}

async fn deliver(
    query: &mut ContinuousQueryTransformer,
    outputs: &[OutputEnvelope],
) -> Result<usize> {
    let mut emitted = 0;
    for output in outputs {
        emitted += query
            .transform(InputEnvelope {
                port: PortId::try_new("in")?,
                envelope: output.envelope.clone(),
            })
            .await?
            .len();
    }
    Ok(emitted)
}

#[tokio::test]
async fn persistent_query_must_not_silently_accept_volatile_stateful_middleware() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let registry_owner = DrasiLib::builder().build().await?;
    let mut middleware =
        MiddlewareTransformer::new(definition()?, registry_owner.middleware_registry())?;
    middleware.start().await?;
    let mut query = persistent_query(directory.path()).await?;
    let mut output = middleware.transform(input(1, false)?).await?;
    assert_eq!(output.len(), 1);
    let accepted = query
        .transform(InputEnvelope {
            port: PortId::try_new("in")?,
            envelope: output.remove(0).envelope,
        })
        .await;
    assert!(accepted.is_err(), "persistent query must reject a known volatile middleware boundary rather than imply restart safety");
    assert!(query.results().snapshot()?.rows.is_empty());
    middleware.stop().await?;
    query.stop().await?;
    registry_owner.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn durable_unwind_reconstruction_recovers_state_and_unconfirmed_delivery() -> Result<()> {
    let registry_owner = DrasiLib::builder().build().await?;
    let registry = registry_owner.middleware_registry();
    for raw_metadata in [false, true] {
        for delivered_before_restart in [false, true] {
            let directory = tempfile::tempdir()?;
            let middleware_path = directory.path().join("middleware");
            let query_path = directory.path().join("query");
            let first_output = {
                let mut middleware =
                    durable_middleware(&middleware_path, registry.clone(), 8).await?;
                let mut query = persistent_query(&query_path).await?;
                let output = middleware
                    .transform(source_input(1, false, raw_metadata)?)
                    .await?;
                assert_eq!(output.len(), 1);
                if delivered_before_restart {
                    assert_eq!(deliver(&mut query, &output).await?, 1);
                    assert_eq!(child_ids(&query)?, ["a", "b"]);
                }
                // No completion callback: publication or its acknowledgement was lost.
                middleware.stop().await?;
                query.stop().await?;
                output[0].envelope.clone()
            };

            let mut middleware = durable_middleware(&middleware_path, registry.clone(), 8).await?;
            let mut query = persistent_query(&query_path).await?;
            assert!(middleware.has_pending_emissions());
            let replay = middleware.on_wakeup().await?;
            assert_eq!(replay.len(), 1);
            assert_eq!(
                replay[0].envelope.changes().id(),
                first_output.changes().id()
            );
            assert!(replay[0].envelope.system().sequence() > first_output.system().sequence());
            assert_eq!(
                GraphProducerProgress::from_envelope(&replay[0].envelope)?,
                GraphProducerProgress::from_envelope(&first_output)?,
            );
            assert_eq!(
                GraphChangeCodec::source_metadata(&replay[0].envelope)?,
                GraphChangeCodec::source_metadata(&first_output)?,
            );
            assert_eq!(
                deliver(&mut query, &replay).await?,
                usize::from(!delivered_before_restart),
                "logical progress, not fresh transport sequence, controls duplicate handling"
            );
            assert_eq!(child_ids(&query)?, ["a", "b"]);
            middleware.delivery_completed(&replay).await?;
            assert!(!middleware.has_pending_emissions());

            let update = middleware
                .transform(source_input(2, true, raw_metadata)?)
                .await?;
            assert_eq!(update.len(), 1);
            deliver(&mut query, &update).await?;
            assert_eq!(
                child_ids(&query)?,
                ["a"],
                "removed children must not survive reconstruction"
            );
            middleware.delivery_completed(&update).await?;
            middleware.stop().await?;
            query.stop().await?;
        }
    }
    registry_owner.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn full_middleware_retention_preserves_input_for_retry_after_durable_confirmation(
) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let registry_owner = DrasiLib::builder().build().await?;
    let mut middleware = durable_middleware(
        &directory.path().join("middleware"),
        registry_owner.middleware_registry(),
        1,
    )
    .await?;
    let mut query = persistent_query(&directory.path().join("query")).await?;
    let first = middleware.transform(input(1, false)?).await?;
    let error = middleware
        .transform(input(2, true)?)
        .await
        .expect_err("unconfirmed output is retained");
    assert!(matches!(
        error.downcast_ref::<MiddlewareRecoveryError>(),
        Some(MiddlewareRecoveryError::RetentionExhausted)
    ));
    assert!(child_ids(&query)?.is_empty());
    deliver(&mut query, &first).await?;
    middleware.delivery_completed(&first).await?;
    let second = middleware.transform(input(2, true)?).await?;
    deliver(&mut query, &second).await?;
    assert_eq!(child_ids(&query)?, ["a"]);
    middleware.delivery_completed(&second).await?;
    middleware.stop().await?;
    query.stop().await?;
    registry_owner.shutdown().await?;
    Ok(())
}
