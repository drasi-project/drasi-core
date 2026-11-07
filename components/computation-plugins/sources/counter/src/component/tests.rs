// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use crate::CounterFactory;
use drasi_computation_contracts::ComponentId;
use drasi_computation_plugin_sdk::{CreateRequest, Factory};
use serde_json::json;

fn request(configuration: serde_json::Value) -> anyhow::Result<CreateRequest> {
    let metadata = CounterFactory.metadata();
    Ok(CreateRequest {
        id: ComponentId::try_new("08f93596-5c0d-4c5d-890a-cf015bb3d809")?,
        implementation: metadata.implementation,
        configuration_version: metadata.configuration_version,
        configuration,
        inputs: Vec::new(),
        scope: None,
        activation_credentials_bound: false,
        update_directive: None,
    })
}

fn value(output: &OutputEnvelope) -> anyhow::Result<i64> {
    let changes = GraphChangeCodec::decode_changes(&output.envelope)?;
    let [SourceChange::Insert {
        element: Element::Node { properties, .. },
    }] = changes.as_slice()
    else {
        anyhow::bail!("counter did not emit one inserted node")
    };
    let Some(ElementValue::Integer(value)) = properties.get("value") else {
        anyhow::bail!("counter did not emit an integer")
    };
    Ok(*value)
}

#[tokio::test]
async fn interval_update_retains_identity_sequence_and_emitted_values() -> anyhow::Result<()> {
    let current = request(json!({"stream": "counter-stream", "count": 3, "start": 10}))?;
    let mut proposed = current.clone();
    proposed.configuration["interval_ms"] = json!(1);
    let update = UpdateRequest {
        current: current.clone(),
        proposed,
    };
    let plan = CounterFactory.plan_update(&update)?;
    assert_eq!(plan.mode, ComponentUpdateMode::InPlace);
    assert_eq!(plan.data, ComponentUpdateData::Preserve);
    let (mut counter, control) = Counter::new(
        CounterFactory.metadata().descriptor(current.id.clone())?,
        current.configuration.clone(),
    )?;
    counter.initialize().await?;
    counter.start().await?;
    let first = counter
        .next()
        .await?
        .ok_or_else(|| anyhow::anyhow!("first value missing"))?;
    assert_eq!(value(&first)?, 10);
    assert_eq!(first.envelope.system().sequence(), 1);
    assert_eq!(counter.emitted, 1);
    counter.pause_processing().await?;
    control.0.send_replace(true);
    counter
        .apply_update(&ApplyUpdateRequest {
            operation_id: "change-interval".into(),
            request: update,
            plan,
        })
        .await?;
    counter.resume_processing().await?;
    assert!(*counter.paused.borrow());
    control.0.send_replace(false);
    assert_eq!(counter.descriptor.id(), &current.id);
    assert_eq!(counter.emitted, 1);
    let second = counter
        .next()
        .await?
        .ok_or_else(|| anyhow::anyhow!("second value missing"))?;
    assert_eq!(value(&second)?, 11);
    assert_eq!(second.envelope.system().sequence(), 2);
    assert_eq!(counter.emitted, 2);
    counter.stop().await?;
    counter.start().await?;
    let third = counter
        .next()
        .await?
        .ok_or_else(|| anyhow::anyhow!("third value missing"))?;
    assert_eq!(value(&third)?, 12);
    assert_eq!(third.envelope.system().sequence(), 3);
    assert!(counter.next().await?.is_none());
    counter.stop().await?;
    counter.dispose().await?;
    Ok(())
}

#[test]
fn factory_rejects_changes_outside_its_supported_update_contract() -> anyhow::Result<()> {
    let current = request(json!({"stream": "counter-stream"}))?;
    for (field, replacement) in [
        ("stream", json!("different-stream")),
        ("count", json!(20)),
        ("start", json!(50)),
        ("step", json!(2)),
        ("paused", json!(true)),
    ] {
        let mut proposed = current.clone();
        proposed.configuration[field] = replacement;
        assert!(
            CounterFactory
                .plan_update(&UpdateRequest {
                    current: current.clone(),
                    proposed,
                })
                .is_err(),
            "{field} was accepted as an interval-only update"
        );
    }
    let mut proposed = current.clone();
    proposed.id = ComponentId::generate();
    assert!(CounterFactory
        .plan_update(&UpdateRequest { current, proposed })
        .is_err());
    Ok(())
}

#[tokio::test]
async fn stale_or_forged_update_does_not_change_the_counter() -> anyhow::Result<()> {
    let current = request(json!({"stream": "counter-stream"}))?;
    let mut proposed = current.clone();
    proposed.configuration["interval_ms"] = json!(1);
    let (mut counter, _control) = Counter::new(
        CounterFactory.metadata().descriptor(current.id.clone())?,
        current.configuration.clone(),
    )?;
    counter.initialize().await?;
    counter.start().await?;
    let mut update = ApplyUpdateRequest {
        operation_id: "unsupported-reset".into(),
        request: UpdateRequest { current, proposed },
        plan: ComponentUpdatePlan {
            mode: ComponentUpdateMode::InPlace,
            data: ComponentUpdateData::Rebootstrap,
        },
    };
    assert!(counter.apply_update(&update).await.is_err());
    assert_eq!(counter.configuration.interval_ms, 0);
    update.plan.data = ComponentUpdateData::Preserve;
    update.request.current.configuration["count"] = json!(50);
    update.request.proposed.configuration["count"] = json!(50);
    assert!(counter.apply_update(&update).await.is_err());
    assert_eq!(counter.configuration.count, 10);
    counter.stop().await?;
    counter.dispose().await?;
    Ok(())
}
