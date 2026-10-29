// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Authored real-server gates. Compilation is not qualification.

use super::*;
use drasi_core::computation::ComputationIndexProvider;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::sync::Semaphore;

#[path = "recovery.rs"]
mod recovery;
#[path = "sink_effects.rs"]
mod sink_effects;
#[path = "snapshots.rs"]
mod snapshots;

pub(super) struct Gate {
    blocked: AtomicBool,
    entered: AtomicUsize,
    permits: Semaphore,
}
impl Gate {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            blocked: AtomicBool::new(false),
            entered: AtomicUsize::new(0),
            permits: Semaphore::new(0),
        })
    }
    fn release(&self) {
        self.blocked.store(false, Ordering::Release);
        self.permits.add_permits(64);
    }
}
struct Drain {
    descriptor: ComponentDescriptor,
    gate: Option<Arc<Gate>>,
}
#[async_trait]
impl ComputationComponent for Drain {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
        Ok(())
    }
}
#[async_trait]
impl EnvelopeSink for Drain {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, _: InputEnvelope) -> Result<()> {
        if let Some(gate) = &self.gate {
            if gate.blocked.load(Ordering::Acquire) {
                gate.entered.fetch_add(1, Ordering::AcqRel);
                gate.permits.acquire().await?.forget();
            }
        }
        Ok(())
    }
}

struct Harness {
    core: DrasiLib,
    results: Vec<QueryResults>,
    owners: Vec<Arc<QuerySourceProgress>>,
    gate: Arc<Gate>,
}
impl Harness {
    async fn new(
        dynamic: bool,
        settings: Value,
        indexes: Arc<dyn ComputationIndexProvider>,
        companion: bool,
    ) -> Result<Self> {
        let native = plugins(dynamic)?;
        let owners = ["a", "b"]
            .into_iter()
            .map(|name| {
                Ok(Arc::new(QuerySourceProgress::new(
                    ComponentBatch::GRAPH_ID,
                    id(name),
                )?))
            })
            .collect::<Result<Vec<_>>>()?;
        let bootstrap = native[1]
            .bootstrap_factories()
            .iter()
            .find(|factory| {
                factory.metadata().implementation.name.as_ref() == "drasi.mssql/bootstrap"
            })
            .context("coordinated SQL Server bootstrap factory")?;
        let gate = Gate::new();
        let mut results = Vec::new();
        let mut builder = ComponentBatch::builder();
        for (index, name) in ["a", "b"].into_iter().enumerate() {
            let provider = bootstrap.create_provider(
                id(name),
                json!({"source":"items","stream":stream("items"),"settings":settings}),
                Some(drasi_computation_plugin_sdk::Scope {
                    instance_id: "mssql-coverage".into(),
                    graph_id: ComponentBatch::GRAPH_ID.into(),
                    generation: 1,
                }),
                Some(owners[index].clone()),
            )?;
            let label = if companion && name == "b" {
                "Companion"
            } else {
                "Items"
            };
            let query = ContinuousQueryTransformer::new_configured(
                ContinuousQueryDefinition {
                    graph_id: ComponentBatch::GRAPH_ID.into(),
                    id: id(name),
                    query: format!("MATCH (n:{label}) RETURN n.id AS id,n.value AS value"),
                    language: ComputationQueryLanguage::Cypher,
                    output_stream: stream(name),
                    outbox_capacity: NonZeroUsize::new(1).expect("positive capacity"),
                },
                indexes.clone(),
                QueryOptions::default(),
                Default::default(),
                None,
            )
            .await?
            .with_source_progress(owners[index].clone())?
            .with_recovery_scope("mssql-coverage")?
            .with_bootstrap(provider);
            results.push(query.results());
            let drain = format!("{name}-drain");
            builder = builder
                .query(Box::new(query))
                .sink(Box::new(Drain {
                    descriptor: ComponentDescriptor::try_new(
                        id(&drain),
                        vec![PortDescriptor::new(
                            PortId::try_new("in")?,
                            PortDirection::Input,
                            QueryChangeCodec::schema().descriptor().clone(),
                            PipeRequirements::default(),
                        )],
                    )?,
                    gate: (name == "a").then(|| gate.clone()),
                }))
                .bind_stream(endpoint(name, "out"), stream(name))
                .connect(
                    EdgeDefinition::new(endpoint("items", "out"), endpoint(name, "in")),
                    Box::new(BoundedPipeConfig { capacity: 1 }),
                )
                .connect(
                    EdgeDefinition::new(endpoint(name, "out"), endpoint(&drain, "in")),
                    Box::new(BoundedPipeConfig { capacity: 1 }),
                );
        }
        let factory = native[0].factories()[0].clone();
        let mut specification =
            factory.specification(id("items"), json!({"stream":stream("items"),"settings":{}}))?;
        specification.configuration.insert(
            "settings".into(),
            ConfigurationValue::Reference {
                resource: resource("settings"),
                key: "source".into(),
                secret: true,
            },
        );
        specification
            .dependencies
            .insert("consumer_progress".into(), vec![resource("consumers")]);
        for (name, role, handle) in [
            (
                "settings",
                ResourceRole::SecretStore,
                ResourceHandle::new(
                    ResourceRole::SecretStore,
                    Arc::new(ConfigurationResolverResource(Arc::new(Settings(
                        BTreeMap::from([("source".into(), settings)]),
                    )))),
                ),
            ),
            (
                "consumers",
                ResourceRole::Checkpoint,
                ResourceHandle::new(
                    ResourceRole::Checkpoint,
                    Arc::new(ConsumerProgressSet::new(
                        ComponentBatch::GRAPH_ID,
                        id("items"),
                        owners.clone(),
                    )?),
                ),
            ),
        ] {
            builder = builder
                .declare_resource(ResourceSpecification {
                    id: resource(name),
                    role,
                    ownership: ResourceOwnership::Borrowed,
                    binding: name.into(),
                })?
                .provide_resource(resource(name), handle)?;
        }
        let core = DrasiLib::builder()
            .with_id("mssql-coverage")
            .with_components(
                builder
                    .component(specification, factory)
                    .bind_stream(endpoint("items", "out"), stream("items"))
                    .build()?,
            )
            .build()
            .await?;
        Ok(Self {
            core,
            results,
            owners,
            gate,
        })
    }
    async fn start(&self) -> Result<()> {
        self.core.start().await?;
        for name in ["a", "b"] {
            tokio::time::timeout(
                Duration::from_secs(30),
                self.core.computation_component(name)?.wait_started(),
            )
            .await??;
        }
        Ok(())
    }
    async fn shutdown(&self) -> Result<()> {
        self.gate.release();
        self.core.shutdown().await?;
        Ok(())
    }
    async fn failure(&self, expected: &[&str]) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(25), async {
            loop {
                let state = self.core.computation_component("items")?.observed()?;
                if state.lifecycle == ComponentLifecycle::Failed {
                    let failure = format!("{:?}", state.failure);
                    ensure!(
                        expected
                            .iter()
                            .any(|message| failure.to_lowercase().contains(message)),
                        "source failed for an unexpected reason: {failure}"
                    );
                    return Ok::<_, anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .context("expected real SQL Server source failure did not occur")?
    }
}

async fn suite(dynamic: bool) -> Result<()> {
    super::credentials::actual_override_and_refresh(dynamic).await?;
    snapshots::least_privilege(dynamic).await?;
    snapshots::concurrent_cut_and_cleanup(dynamic).await?;
    snapshots::large_snapshots_and_tiny_cdc_pages(dynamic).await?;
    sink_effects::parameters_and_failure(dynamic).await?;
    sink_effects::retained_work(dynamic).await?;
    recovery::partial_persistent(dynamic).await?;
    recovery::catalog_changes(dynamic).await?;
    Ok(())
}

#[tokio::test]
async fn static_current_thread_real_contract_matrix() -> Result<()> {
    suite(false).await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn static_multi_thread_real_contract_matrix() -> Result<()> {
    suite(false).await
}
#[tokio::test]
async fn dynamic_current_thread_real_contract_matrix() -> Result<()> {
    suite(true).await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dynamic_multi_thread_real_contract_matrix() -> Result<()> {
    suite(true).await
}
