// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

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

struct Probe(ComponentDescriptor);

#[async_trait]
impl ComputationComponent for Probe {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.0
    }
    fn configuration(&self) -> anyhow::Result<serde_json::Value> {
        Ok(serde_json::json!({}))
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
            descriptor: ComponentDescriptor::try_new(
                ComponentId::try_new(id).expect("probe component ID"),
                Vec::new(),
            )
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
            .component(self.specification(id), self.clone())
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
            configuration: ConfigurationSchema::default(),
            dependencies: BTreeMap::new(),
        },
        observed,
    });
    let batch = factory.batch("first");
    assert!(
        observations.borrow().is_empty(),
        "assembly must not execute a factory"
    );
    let core = DrasiLib::builder()
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
    let control = core.computation_control().unwrap();
    let desired = control
        .desired_snapshot()
        .select(GraphSelection::Exact(vec![
            ComponentId::try_new("first").unwrap()
        ]))
        .unwrap()
        .components
        .remove(0);
    let preview = control
        .preview(
            control.desired_snapshot().revision,
            vec![DesiredMutation::ReplaceComponent(desired)],
        )
        .await
        .unwrap();
    let report = control
        .reconcile(preview, TopologyBindings::default())
        .await
        .unwrap();
    assert_eq!(report.summary, OperationSummary::Completed);
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
        .filter(|value| value.component == "first")
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
