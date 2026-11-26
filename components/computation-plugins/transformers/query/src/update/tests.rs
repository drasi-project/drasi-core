// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_computation_contracts::{
    ComponentId, ComponentUpdateData, InputEnvelope, OutputFollowLimits,
};
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
};
use serde_json::json;
use std::time::Duration;

/// The v3 planner view of the factory assessment: unsupported is an error.
trait PlanUpdate {
    fn plan_update(&self, request: &UpdateRequest) -> anyhow::Result<ComponentUpdatePlan>;
}

impl<F: Factory> PlanUpdate for F {
    fn plan_update(&self, request: &UpdateRequest) -> anyhow::Result<ComponentUpdatePlan> {
        self.update_planner()
            .ok_or_else(|| anyhow::anyhow!("factory does not plan updates"))?
            .assess(request)?
            .plan()
            .ok_or_else(|| anyhow::anyhow!("update unsupported"))
    }
}

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
        execution_context: Default::default(),
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
        json!({"query": "MATCH (n) RETURN n", "outboxCapacity": 2}),
    )?;
    current.implementation.plugin = Some(drasi_computation_contracts::PluginIdentity {
        id: env!("CARGO_PKG_NAME").into(),
        version: env!("CARGO_PKG_VERSION").into(),
    });
    let mut proposed = current.clone();
    proposed.configuration["outboxCapacity"] = json!(4);
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
        json!({"query": "MATCH (n:Item) RETURN n.value AS value", "outboxCapacity": 4}),
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
    proposed.configuration["outboxCapacity"] = json!(1);
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
    assert!(transformer::validate_reconstruction(&request).is_err());
    request.configuration["bootstrap"] = json!(true);
    transformer::validate_reconstruction(&request)?;
    assert!(request.configuration.get("resetConfiguration").is_none());
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

#[allow(dead_code)]
pub(crate) mod lifecycle_ctx {
    use drasi_computation_plugin_sdk::lifecycle::{
        ApplyContext, CancellationToken, DisposeContext, DisposeReason, InitializeContext,
        OperationScope, PauseContext, ResumeContext, StartContext, StateIntent, StopContext,
        StopMode, UpdateBoundary,
    };

    pub(crate) fn scope() -> OperationScope {
        OperationScope::new("test", "component", 1, None, CancellationToken::new())
    }
    pub(crate) fn initialize() -> InitializeContext {
        InitializeContext::new(scope())
    }
    pub(crate) fn start() -> StartContext {
        StartContext::new(scope(), None)
    }
    pub(crate) fn pause() -> PauseContext {
        PauseContext::new(scope())
    }
    pub(crate) fn resume() -> ResumeContext {
        ResumeContext::new(scope())
    }
    pub(crate) fn stop() -> StopContext {
        StopContext::new(scope(), StopMode::Drain)
    }
    pub(crate) fn dispose() -> DisposeContext {
        DisposeContext::new(scope(), DisposeReason::Shutdown, StateIntent::Preserve, 1)
            .expect("valid dispose context")
    }
    pub(crate) fn erase() -> DisposeContext {
        DisposeContext::new(scope(), DisposeReason::Removal, StateIntent::Erase, 1)
            .expect("valid dispose context")
    }
    pub(crate) fn apply(boundary: UpdateBoundary) -> ApplyContext {
        ApplyContext::new(scope(), boundary)
    }
}

#[test]
fn configuration_keys_are_camel_case_and_nested_unknown_keys_are_refused() -> anyhow::Result<()> {
    let factory = IndexedQueryFactory::default();
    let middleware = default_middleware_registry();
    let metadata = factory.metadata();
    drasi_computation_plugin_sdk::testing::ensure_camel_case_configuration(
        &metadata.configuration,
    )?;
    let accepted = request(
        &factory,
        json!({
            "query": "MATCH (n) RETURN n",
            "outboxCapacity": 3,
            "resetConfiguration": true,
            "sourceTransactions": {"maxChanges": 8, "maxBytes": 4096, "maxDurationMs": 1000}
        }),
    )?;
    metadata.configuration.validate(&accepted.configuration)?;
    let (definition, execution) = query_configuration(&accepted, &middleware)?;
    assert_eq!(definition.outbox_capacity.get(), 3);
    let limits = execution.source_transactions.expect("limits");
    assert_eq!(
        (
            limits.max_changes.get(),
            limits.max_bytes.get(),
            limits.max_duration_ms.get()
        ),
        (8, 4096, 1000)
    );
    for snake in [
        "outbox_capacity",
        "reset_configuration",
        "source_transactions",
    ] {
        let mut configuration = json!({"query": "MATCH (n) RETURN n"});
        configuration[snake] = json!(1);
        assert!(
            metadata.configuration.validate(&configuration).is_err(),
            "{snake} is not a published key"
        );
    }
    let nested = request(
        &factory,
        json!({
            "query": "MATCH (n) RETURN n",
            "sourceTransactions": {"maxChanges": 8, "maxBytes": 4096, "max_duration_ms": 1000}
        }),
    )?;
    assert!(query_configuration(&nested, &middleware).is_err());
    Ok(())
}
