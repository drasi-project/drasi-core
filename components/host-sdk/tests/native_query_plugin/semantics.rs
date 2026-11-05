// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_computation_plugin_sdk::Scope;

#[path = "../../../../lib/tests/fixtures/query_semantics.rs"]
mod cases;

async fn semantics() -> Result<()> {
    for loaded in [false, true] {
        let plugin = if loaded {
            computation::load(library())?
        } else {
            computation::NativePlugin::from_definition(
                drasi_computation_transformer_query::plugin()?,
            )?
        };
        let factory = plugin
            .factories()
            .iter()
            .find(|factory| {
                factory.metadata().implementation.name.as_ref()
                    == "drasi.experimental/indexed-query"
            })
            .context("native indexed factory")?;
        for persistent in [false, true] {
            for case in [
                cases::disconnected(ComputationQueryLanguage::Cypher)?,
                cases::disconnected(ComputationQueryLanguage::Gql)?,
                cases::optional_list()?,
                cases::assignment_maps()?,
                cases::numeric_count()?,
                cases::optional_join()?,
                cases::canonical_replacement()?,
                cases::ordered_batch()?,
            ] {
                let directory = tempfile::tempdir()?;
                let definition = case.definition()?;
                let mut config = serde_json::to_value(case.execution())?;
                config["query"] = json!(definition.query);
                config["language"] = json!(match definition.language {
                    ComputationQueryLanguage::Cypher => "cypher",
                    ComputationQueryLanguage::Gql => "gql",
                });
                config["stream"] = json!(definition.output_stream);
                config["outbox_capacity"] = json!(8);
                config["volatile"] = json!(!persistent);
                let provider: Arc<dyn drasi_core::computation::ComputationIndexProvider> =
                    if persistent {
                        Arc::new(RocksDbComputationProvider::new(
                            directory.path(),
                            RocksIndexOptions::new(
                                false,
                                false,
                                RocksDbMemoryBudget::from_total_budget_bytes(16 << 20)?,
                            ),
                        ))
                    } else {
                        Arc::new(drasi_core::computation::InMemoryComputationProvider)
                    };
                let mut query = factory.create_component_with_processing(
                    definition.id,
                    config,
                    Scope {
                        instance_id: "configured".into(),
                        graph_id: definition.graph_id,
                        generation: 1,
                    },
                    provider,
                )?;
                let reader = query
                    .output_readers()
                    .ports()
                    .get(&PortId::try_new("out")?)
                    .and_then(|output| output.snapshot())
                    .cloned()
                    .context("canonical snapshot provider")?;
                case.exercise(&mut query, reader).await.with_context(|| {
                    format!(
                        "{} (loaded={loaded}, persistent={persistent})",
                        case.config.id
                    )
                })?;
            }
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_query_semantics_match_shared_reference_cases_current_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(60), semantics()).await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_query_semantics_match_shared_reference_cases_multi_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(60), semantics()).await?
}
