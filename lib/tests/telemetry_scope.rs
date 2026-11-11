// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

#[allow(dead_code)]
mod computation_support;
use computation_support::{component, name};

use async_trait::async_trait;
use drasi_lib::{
    computation::v1::*,
    observability::{
        ComponentLogRuntime, LogPolicy, MetricRegistrationContext, TelemetryOptions,
        TelemetryProfile, TelemetrySettings,
    },
    DrasiLib,
};
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use tokio::sync::watch;
use tracing_subscriber::prelude::*;

#[derive(Clone, Debug)]
struct Observation {
    instance: String,
    graph: String,
    component: String,
    profile: TelemetryProfile,
    generation: u64,
}

struct Probe(ComponentDescriptor, serde_json::Value);

#[async_trait]
impl ComputationComponent for Probe {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.0
    }
    fn configuration(&self) -> anyhow::Result<serde_json::Value> {
        Ok(self.1.clone())
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl ComputationService for Probe {
    async fn run(&mut self) -> anyhow::Result<()> {
        std::future::pending().await
    }
}

struct ProbeFactory {
    descriptor: FactoryDescriptor,
    observed: watch::Sender<Vec<Observation>>,
}

impl ProbeFactory {
    fn specification(&self, id: &str) -> ComponentSpecification {
        ComponentSpecification {
            descriptor: ComponentDescriptor::try_new(component(id), Vec::new())
                .expect("portless probe descriptor"),
            role: ComponentRole::Service,
            completion: None,
            implementation: self.descriptor.implementation.clone(),
            configuration_version: 1,
            configuration: BTreeMap::new(),
            dependencies: BTreeMap::new(),
        }
    }

    fn batch(self: &Arc<Self>, id: &str) -> ComponentBatch {
        ComputationGraph::builder("__drasi_lib_runtime__")
            .with_telemetry(
                TelemetryOptions::new(
                    TelemetrySettings::new(TelemetryProfile::Debug).with_metrics(false),
                )
                .with_instance_id("temporary-assembly"),
            )
            .component(name(id), self.specification(id), self.clone())
            .build_components()
            .expect("temporary component assembly")
            .auto_start(true)
    }
}

#[async_trait]
impl ComponentFactory for ProbeFactory {
    fn descriptor(&self) -> &FactoryDescriptor {
        &self.descriptor
    }

    fn validate(&self, specification: &ComponentSpecification) -> anyhow::Result<()> {
        anyhow::ensure!(
            specification.role == ComponentRole::Service
                && specification.descriptor.ports().is_empty(),
            "probe requires a portless service"
        );
        Ok(())
    }

    fn plan_update(
        &self,
        current: &ComponentSpecification,
        proposed: &ComponentSpecification,
    ) -> anyhow::Result<ComponentUpdatePlan> {
        self.validate(proposed)?;
        anyhow::ensure!(
            current.descriptor == proposed.descriptor
                && current.dependencies == proposed.dependencies,
            "probe replacement must retain its identity and bindings"
        );
        Ok(ComponentUpdatePlan {
            mode: ComponentUpdateMode::Recreate,
            data: ComponentUpdateData::Preserve,
        })
    }

    async fn create(
        &self,
        context: ConstructionContext,
    ) -> std::result::Result<ConstructedComponent, ComponentCreationError> {
        let telemetry = context.telemetry();
        let scope = telemetry.scope().ok_or_else(|| {
            ComponentCreationError::terminal(anyhow::anyhow!("missing telemetry scope"))
        })?;
        let metric_context = telemetry.metric_context().ok_or_else(|| {
            ComponentCreationError::terminal(anyhow::anyhow!("missing shared registration context"))
        })?;
        metric_context
            .framework(
                telemetry.policy(),
                scope,
                "drasi.component.calls",
                [("operation".into(), "start".into())],
            )
            .map_err(ComponentCreationError::terminal)?;
        let logging = context.component_log_binding().ok_or_else(|| {
            ComponentCreationError::terminal(anyhow::anyhow!("missing log binding"))
        })?;
        assert_eq!(logging.key.instance_id, scope.instance_id());
        assert_eq!(logging.key.component_id, context.component_id.as_str());
        self.observed.send_modify(|observations| {
            observations.push(Observation {
                instance: scope.instance_id().into(),
                graph: scope.graph_id().into(),
                component: scope
                    .component_id()
                    .expect("factory component scope")
                    .to_string(),
                profile: telemetry.policy().profile(),
                generation: context.generation.0,
            })
        });
        Ok(ConstructedComponent::service(Box::new(Probe(
            context.specification.descriptor.clone(),
            serde_json::to_value(context.configuration())
                .map_err(ComponentCreationError::terminal)?,
        ))))
    }
}

#[tokio::test(flavor = "current_thread")]
async fn assembly_additions_and_replacement_use_the_owning_graph_policy() {
    let logging = ComponentLogRuntime::new(LogPolicy::new("off").unwrap()).unwrap();
    tracing_subscriber::registry()
        .with(logging.layer())
        .try_init()
        .unwrap();
    let metrics =
        MetricRegistrationContext::new(Arc::new(metrics::NoopRecorder), Default::default())
            .unwrap();
    let (observed, mut observations) = watch::channel(Vec::new());
    let factory = Arc::new(ProbeFactory {
        descriptor: FactoryDescriptor {
            implementation: ImplementationIdentity::try_new("telemetry-probe", "1.0.0").unwrap(),
            role: ComponentRole::Service,
            configuration_version: 1,
            configuration: ConfigurationSchema {
                fields: BTreeMap::from([(
                    Arc::from("revision"),
                    ConfigurationField {
                        value_type: ConfigurationType::Integer,
                        required: false,
                        secret: false,
                    },
                )]),
                allow_additional: false,
            },
            dependencies: BTreeMap::new(),
        },
        observed,
    });
    let batch = factory.batch("first");
    assert!(
        observations.borrow().is_empty(),
        "assembly must not execute a factory"
    );
    let mut factories = FactoryRegistry::standard();
    factories.register(factory.clone()).unwrap();
    let core = DrasiLib::builder()
        .with_allow_external_component_ids(true)
        .with_component_factories(factories)
        .with_id("actual-instance")
        .with_component_logging(logging.clone())
        .with_telemetry(
            TelemetryOptions::new(TelemetryProfile::Basic).with_metric_context(metrics.clone()),
        )
        .with_components(batch)
        .build()
        .await
        .unwrap();
    core.start().await.unwrap();
    tokio::time::timeout(
        Duration::from_secs(3),
        observations.wait_for(|values| !values.is_empty()),
    )
    .await
    .unwrap()
    .unwrap();
    core.add_components(factory.batch("second")).await.unwrap();
    tokio::time::timeout(
        Duration::from_secs(3),
        observations.wait_for(|values| values.len() >= 2),
    )
    .await
    .unwrap()
    .unwrap();
    let registrations_before_replacement = metrics.registrations().counts().unwrap();
    let saved = core.desired_configuration().unwrap();
    let mut desired = saved.desired;
    let first = desired
        .topology
        .components
        .iter_mut()
        .find(|entry| entry.descriptor.id() == &component("first"))
        .unwrap();
    let ComponentConstruction::Factory(specification) = &mut first.construction else {
        panic!("probe must retain its factory recipe");
    };
    specification
        .configuration
        .insert(Arc::from("revision"), ConfigurationValue::Literal(1.into()));
    core.apply_desired_state(saved.revision, "replace-probe", desired)
        .await
        .unwrap();
    core.reconcile_desired_state().await.unwrap();
    tokio::time::timeout(
        Duration::from_secs(3),
        observations.wait_for(|values| values.len() >= 3),
    )
    .await
    .unwrap()
    .unwrap();
    let values = observations.borrow().clone();
    for value in &values {
        assert_eq!(value.instance, "actual-instance");
        assert_eq!(value.graph, "__drasi_lib_runtime__");
        assert_eq!(value.profile, TelemetryProfile::Basic);
    }
    let first: Vec<_> = values
        .iter()
        .filter(|value| value.component == component("first").to_string())
        .collect();
    assert_eq!(first.len(), 2);
    assert!(first[1].generation > first[0].generation);
    // Framework instruments and the factory probe reuse stable series on replacement.
    assert_eq!(
        metrics.registrations().counts().unwrap(),
        registrations_before_replacement
    );
    core.shutdown().await.unwrap();
    logging.shutdown(Duration::from_secs(2)).await.unwrap();
}
