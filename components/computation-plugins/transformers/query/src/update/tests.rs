// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_computation_contracts::{ComponentId, OutputFollowLimits};
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
};
use serde_json::json;
use std::time::Duration;

fn request(
    factory: &dyn Factory,
    configuration: serde_json::Value,
) -> anyhow::Result<CreateRequest> {
    let metadata = factory.metadata();
    Ok(CreateRequest {
        id: ComponentId::try_new("b42ba71c-93dc-40ce-bb84-a191fb06bbf3")?,
        implementation: metadata.implementation,
        configuration_version: metadata.configuration_version,
        configuration,
        scope: None,
        inputs: Vec::new(),
        activation_credentials_bound: false,
        update_directive: None,
    })
}

fn change(sequence: u64) -> anyhow::Result<InputEnvelope> {
    let mut properties = ElementPropertyMap::new();
    properties.insert("value", ElementValue::Integer(i64::try_from(sequence)?));
    Ok(InputEnvelope {
        port: PortId::try_new("in")?,
        envelope: GraphChangeCodec::encode_change(
            SourceChange::Insert {
                element: Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new("inventory", &sequence.to_string()),
                        labels: Arc::from([Arc::from("Item")]),
                        effective_from: sequence,
                    },
                    properties,
                },
            },
            StreamId::try_new("inventory/out")?,
            sequence,
            None,
        )?,
    })
}

#[test]
fn unchanged_output_shape_does_not_make_changed_query_semantics_compatible() -> anyhow::Result<()> {
    let factory = IndexedQueryFactory::default();
    let current = request(
        &factory,
        json!({"query": "MATCH (n:Item) RETURN n.value AS value", "bootstrap": true}),
    )?;
    for query in [
        "MATCH (n:Item) WHERE n.value > 10 RETURN n.value AS value",
        "MATCH (n:Other) RETURN n.value AS value",
    ] {
        let mut proposed = current.clone();
        proposed.configuration["query"] = json!(query);
        assert_eq!(
            factory.plan_update(&UpdateRequest {
                current: current.clone(),
                proposed
            })?,
            ComponentUpdatePlan {
                mode: ComponentUpdateMode::Recreate,
                data: ComponentUpdateData::Rebootstrap,
            }
        );
    }
    Ok(())
}

#[test]
fn factory_planning_accepts_sdk_attached_plugin_provenance_without_allowing_a_change(
) -> anyhow::Result<()> {
    let factory = IndexedQueryFactory::default();
    let mut current = request(
        &factory,
        json!({"query": "MATCH (n) RETURN n", "outbox_capacity": 2}),
    )?;
    current.implementation.plugin = Some(drasi_computation_contracts::PluginIdentity {
        id: env!("CARGO_PKG_NAME").into(),
        version: env!("CARGO_PKG_VERSION").into(),
    });
    let mut proposed = current.clone();
    proposed.configuration["outbox_capacity"] = json!(4);
    let mut request = UpdateRequest { current, proposed };
    assert_eq!(
        factory.plan_update(&request)?.data,
        ComponentUpdateData::Preserve
    );
    request.proposed.implementation.plugin = None;
    assert!(factory.plan_update(&request).is_err());
    Ok(())
}

#[test]
fn streaming_query_updates_reset_but_retained_updates_cannot_discard_history() -> anyhow::Result<()>
{
    let factory = IndexedQueryFactory::default();
    let current = request(
        &factory,
        json!({"query": "MATCH (n:Item) RETURN n.value AS value", "outbox_capacity": 4}),
    )?;
    let mut proposed = current.clone();
    proposed.configuration["query"] = json!("MATCH (n:Other) RETURN n.value AS value");
    assert_eq!(
        factory.plan_update(&UpdateRequest {
            current: current.clone(),
            proposed
        })?,
        ComponentUpdatePlan {
            mode: ComponentUpdateMode::Recreate,
            data: ComponentUpdateData::Reset,
        }
    );
    let mut proposed = current.clone();
    proposed.configuration["outbox_capacity"] = json!(1);
    assert!(factory
        .plan_update(&UpdateRequest {
            current: current.clone(),
            proposed
        })
        .is_err());
    let mut proposed = current.clone();
    proposed.configuration["stream"] = json!("different-stream");
    assert!(factory
        .plan_update(&UpdateRequest { current, proposed })
        .is_err());
    Ok(())
}

#[test]
fn reconstruction_directive_cannot_replace_the_bootstrap_requirement() -> anyhow::Result<()> {
    let factory = IndexedQueryFactory::default();
    let mut request = request(&factory, json!({"query": "MATCH (n) RETURN n"}))?;
    request.update_directive = Some(drasi_computation_plugin_sdk::wire::UpdateDirective {
        operation_id: "accepted-query-update".into(),
        plan: ComponentUpdatePlan {
            mode: ComponentUpdateMode::Recreate,
            data: ComponentUpdateData::Rebootstrap,
        },
    });
    assert!(validate_creation(&request).is_err());
    request.configuration["bootstrap"] = json!(true);
    validate_creation(&request)?;
    assert!(request.configuration.get("reset_configuration").is_none());
    Ok(())
}

#[test]
fn persistent_non_atomic_reconstruction_uses_the_reset_protocol() -> anyhow::Result<()> {
    let factory = IndexedQueryFactory::default();
    let current = request(
        &factory,
        json!({
            "query": "MATCH (n:Item) RETURN n.value AS value",
            "bootstrap": true,
            "publication": "non_atomic"
        }),
    )?;
    let mut proposed = current.clone();
    proposed.configuration["query"] = json!("MATCH (n:Other) RETURN n.value AS value");
    assert_eq!(
        factory.plan_update(&UpdateRequest { current, proposed })?,
        ComponentUpdatePlan {
            mode: ComponentUpdateMode::Recreate,
            data: ComponentUpdateData::Rebootstrap,
        }
    );
    Ok(())
}

#[tokio::test]
async fn retention_update_preserves_rows_cursor_and_existing_readers() -> anyhow::Result<()> {
    let factory = QueryFactory::default();
    let current = request(
        &factory,
        json!({"query": "MATCH (n:Item) RETURN n.value AS value", "volatile": true}),
    )?;
    let created = create_query(
        &current,
        factory.metadata(),
        None,
        None,
        default_middleware_registry(),
    )?;
    let Component::Transformer(mut query) = created.component else {
        anyhow::bail!("query factory did not return a transformer")
    };
    query.initialize().await?;
    query.start().await?;
    let first = query.transform(change(1)?).await?;
    assert_eq!(first.len(), 1);
    let readers = query.output_readers();
    let port = readers
        .ports()
        .get(&PortId::try_new("out")?)
        .context("query output missing")?;
    let snapshots = port.snapshot().context("snapshot provider missing")?;
    let views = port.query().context("query view missing")?;
    let before = snapshots.snapshot().await?;
    assert_eq!(before.records().len(), 1);
    let mut proposed = current.clone();
    proposed.configuration["outbox_capacity"] = json!(4);
    let request = UpdateRequest { current, proposed };
    let plan = factory.plan_update(&request)?;
    assert_eq!(plan.mode, ComponentUpdateMode::InPlace);
    assert_eq!(plan.data, ComponentUpdateData::Preserve);
    query.pause_processing().await?;
    query
        .apply_update(&ApplyUpdateRequest {
            operation_id: "increase-query-retention".into(),
            request,
            plan,
        })
        .await?;
    query.resume_processing().await?;
    let after = snapshots.snapshot().await?;
    assert_eq!(before.cursor(), after.cursor());
    assert_eq!(before.records().len(), after.records().len());
    assert_eq!(views.description().outbox_capacity.get(), 4);
    let follow = port.snapshot_follow().context("follow provider missing")?;
    let lease = follow
        .snapshot_and_follow(OutputFollowLimits {
            max_envelopes: NonZeroUsize::new(4).expect("positive"),
            max_bytes: NonZeroUsize::new(1024 * 1024).expect("positive"),
            lifetime: Duration::from_secs(30),
        })
        .await?;
    assert_eq!(lease.snapshot().cursor(), after.cursor());
    drop(lease);
    assert_eq!(query.transform(change(2)?).await?.len(), 1);
    let (cursor, view) = views.query_view().await?;
    assert_eq!(cursor.sequence(), 2);
    assert_eq!(view.snapshot.rows.len(), 2);
    assert_eq!(view.retained.len(), 2);
    query.stop().await?;
    query.dispose().await?;
    Ok(())
}
