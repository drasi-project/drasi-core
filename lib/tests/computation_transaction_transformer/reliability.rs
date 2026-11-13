// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use anyhow::Context;
use std::collections::BTreeMap;
use std::time::Duration;

struct IdenticalRecipeFactory {
    inner: TransactionTransformerFactory,
    descriptor: FactoryDescriptor,
    initial: ComponentSpecification,
}

impl IdenticalRecipeFactory {
    fn new(mut initial: ComponentSpecification) -> TestResult<Self> {
        let inner = TransactionTransformerFactory::default();
        let mut descriptor = inner.descriptor().clone();
        descriptor.implementation =
            ImplementationIdentity::try_new("test/identical-transaction-reconstruction", "1")?;
        initial.implementation = descriptor.implementation.clone();
        inner.validate(&initial)?;
        Ok(Self {
            inner,
            descriptor,
            initial,
        })
    }
}

#[async_trait]
impl ComponentFactory for IdenticalRecipeFactory {
    fn descriptor(&self) -> &FactoryDescriptor {
        &self.descriptor
    }

    fn validate(&self, specification: &ComponentSpecification) -> TestResult<()> {
        self.inner.validate(specification)
    }

    fn validate_scope(
        &self,
        graph_id: &str,
        specification: &ComponentSpecification,
        declarations: &BTreeMap<ResourceId, ResourceSpecification>,
        resources: &BTreeMap<ResourceId, ResourceHandle>,
    ) -> TestResult<()> {
        self.inner
            .validate_scope(graph_id, specification, declarations, resources)
    }

    fn plan_update(
        &self,
        current: &ComponentSpecification,
        proposed: &ComponentSpecification,
    ) -> TestResult<ComponentUpdatePlan> {
        anyhow::ensure!(
            current == &self.initial && proposed == &self.initial,
            "fixture only supports reconstruction of its exact initial recipe"
        );
        self.inner.validate(current)?;
        self.inner.validate(proposed)?;
        Ok(ComponentUpdatePlan {
            mode: ComponentUpdateMode::Recreate,
            data: ComponentUpdateData::Preserve,
        })
    }

    async fn create(
        &self,
        context: ConstructionContext,
    ) -> std::result::Result<ConstructedComponent, ComponentCreationError> {
        if context.specification.as_ref() != &self.initial {
            return Err(ComponentCreationError::terminal(anyhow::anyhow!(
                "fixture construction requires its exact initial recipe"
            )));
        }
        self.inner.create(context).await
    }
}

struct Source {
    descriptor: ComponentDescriptor,
    input: tokio::sync::mpsc::Receiver<ChangeEnvelope>,
    transport: u64,
}
#[async_trait]
impl ComputationComponent for Source {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> TestResult<()> {
        Ok(())
    }
    async fn stop(&mut self) -> TestResult<()> {
        Ok(())
    }
}
#[async_trait]
impl EnvelopeSource for Source {
    async fn next(&mut self) -> TestResult<Option<OutputEnvelope>> {
        let Some(original) = self.input.recv().await else {
            return Ok(None);
        };
        self.transport += 1;
        let change = GraphChangeCodec::decode_changes(&original)?.remove(0);
        let event = drasi_lib::channels::SourceEventWrapper::new(
            id("source"),
            drasi_lib::channels::SourceEvent::Change(change),
            chrono::DateTime::from_timestamp_millis(1000).unwrap(),
            original.system().sequence(),
        );
        let envelope = GraphChangeCodec::encode_source_event(
            Arc::new(event),
            &id("source"),
            stream("source/out"),
            self.transport,
            None,
        )?;
        Ok(Some(OutputEnvelope {
            port: port("out"),
            envelope,
        }))
    }
}

async fn wait_output(output: &Mutex<Vec<ChangeEnvelope>>, count: usize) -> TestResult<()> {
    tokio::time::timeout(Duration::from_secs(5), async {
        while output.lock().unwrap().len() < count {
            tokio::task::yield_now().await;
        }
    })
    .await
    .context("expected output did not arrive")?;
    Ok(())
}

async fn draining_replacement() -> TestResult<()> {
    for committed in [false, true] {
        let directory = tempfile::tempdir()?;
        let probe = Arc::new(Probe::default());
        let registry = registry(probe.clone());
        let counts = Arc::new(Counts::default());
        let provider: Arc<dyn ComputationIndexProvider> = Arc::new(CountProvider {
            inner: provider(directory.path()),
            counts: counts.clone(),
        });
        let indexes = ResourceId::try_new("indexes")?;
        let transformers = ResourceId::try_new("transformers")?;
        let delivery = ResourceId::try_new("delivery")?;
        let mut codec = EnvelopeCodec::new(size(64 * 1024 * 1024));
        codec.register_schema(GraphChangeCodec::schema())?;
        let store = Arc::new(IndexedEnvelopeStore::try_new(
            provider
                .create_indexes("transactions", &id("delivery"))
                .await?,
            Arc::new(codec),
            "transaction-delivery",
            size(4),
            RetentionPolicy::Backpressure,
        )?);
        let spec = definition().specification(&registry, transformers.clone(), indexes.clone())?;
        assert!(TransactionTransformerFactory::default()
            .plan_update(&spec, &spec)
            .is_err());
        let factory = Arc::new(IdenticalRecipeFactory::new(spec)?);
        let spec = factory.initial.clone();
        assert_eq!(
            factory.plan_update(&spec, &spec)?,
            ComponentUpdatePlan {
                mode: ComponentUpdateMode::Recreate,
                data: ComponentUpdateData::Preserve,
            }
        );
        let mut changed_configuration = spec.clone();
        let mut changed_definition = definition();
        changed_definition.steps[0].configuration = json!({"add": 3});
        changed_configuration.configuration.insert(
            "transaction".into(),
            ConfigurationValue::Literal(serde_json::to_value(changed_definition)?),
        );
        let mut changed_dependencies = spec.clone();
        changed_dependencies
            .dependencies
            .insert("indexes".into(), vec![delivery.clone()]);
        let mut changed_implementation = spec.clone();
        changed_implementation.implementation = TransactionTransformerFactory::default()
            .descriptor()
            .implementation
            .clone();
        let mut changed_descriptor = spec.clone();
        changed_descriptor.descriptor = descriptor(id("sequence"), true);
        for changed in [
            changed_configuration,
            changed_dependencies,
            changed_implementation,
            changed_descriptor,
        ] {
            assert!(factory.plan_update(&spec, &changed).is_err());
            assert!(factory.plan_update(&changed, &spec).is_err());
            assert!(factory.plan_update(&changed, &changed).is_err());
        }
        let output = Arc::new(Mutex::new(Vec::new()));
        let (send, receive) = tokio::sync::mpsc::channel(4);
        let mut graph = ComputationGraph::builder("transactions")
            .declare_resource(ResourceSpecification {
                id: indexes.clone(),
                role: ResourceRole::IndexBackend,
                ownership: ResourceOwnership::Borrowed,
                binding: "indexes".into(),
            })?
            .provide_resource(
                indexes,
                ResourceHandle::new(
                    ResourceRole::IndexBackend,
                    Arc::new(QueryIndexProviderResource(provider)),
                ),
            )?
            .declare_resource(ResourceSpecification {
                id: transformers.clone(),
                role: ResourceRole::Component,
                ownership: ResourceOwnership::Borrowed,
                binding: "transformers".into(),
            })?
            .provide_resource(
                transformers,
                ResourceHandle::new(
                    ResourceRole::Component,
                    Arc::new(TransactionalTransformerRegistryResource(registry)),
                ),
            )?
            .declare_resource(ResourceSpecification {
                id: delivery.clone(),
                role: ResourceRole::StateStore,
                ownership: ResourceOwnership::Borrowed,
                binding: "delivery".into(),
            })?
            .provide_resource(
                delivery.clone(),
                ResourceHandle::new(
                    ResourceRole::StateStore,
                    Arc::new(RetainedStoreResource(store.clone())),
                ),
            )?
            .fixture_source(Box::new(Source {
                descriptor: ComponentDescriptor::try_new(
                    id("source"),
                    vec![PortDescriptor::new(
                        port("out"),
                        PortDirection::Output,
                        GraphChangeCodec::schema().descriptor().clone(),
                        PipeRequirements::default(),
                    )],
                )?,
                input: receive,
                transport: 0,
            }))
            .component(fixture_support::component_name(), spec, factory.clone())
            .fixture_sink(Box::new(Collector {
                descriptor: ComponentDescriptor::try_new(
                    id("collector"),
                    vec![PortDescriptor::new(
                        port("in"),
                        PortDirection::Input,
                        GraphChangeCodec::schema().descriptor().clone(),
                        PipeRequirements::default(),
                    )],
                )?,
                output: output.clone(),
            }))
            .bind_stream(endpoint("source", "out"), stream("source/out"))
            .bind_stream(endpoint("sequence", "out"), stream("sequence/out"))
            .connect(
                EdgeDefinition::new(endpoint("source", "out"), endpoint("sequence", "in")),
                Box::new(BoundedPipeConfig { capacity: 4 }),
            )
            .connect(
                EdgeDefinition::new(endpoint("sequence", "out"), endpoint("collector", "in")),
                Box::new(RetainedPipeConfig {
                    resource: delivery,
                    capacity: size(4),
                    durable: true,
                    retention: RetentionPolicy::Backpressure,
                    gap_policy: ReplayGapPolicy::Strict,
                }),
            )
            .build()?;
        let run = graph.run()?;
        let control = run.control();
        let (running, checked) = tokio::join!(run, async {
            let result = async {
                control.start_components(GraphRevision(1), GraphSelection::All).await?;
                let before = control.observed();
                if committed { counts.wait_after_commit.store(true, Ordering::Release); }
                else { counts.wait_before_commit.store(true, Ordering::Release); }
                send.send(input(1, 3).envelope).await?;
                tokio::time::timeout(Duration::from_secs(5), async {
                    if committed { counts.committed.notified().await; }
                    else { counts.committing.notified().await; }
                }).await.context("transaction did not reach the commit barrier")?;
                assert!(output.lock().unwrap().is_empty());
                let stop = control.stop_components(GraphRevision(1), GraphSelection::Exact(vec![id("sequence")]));
                tokio::pin!(stop);
                let mut observed = control.subscribe_observed();
                tokio::select! {
                    result = &mut stop => panic!("stop interrupted an active commit: {result:?}"),
                    state = observed.wait_for(|state| state.components[&id("sequence")].lifecycle == ComponentLifecycle::Quiescing) => { state?; }
                }
                assert!(output.lock().unwrap().is_empty());
                counts.release_commit.notify_one();
                assert_eq!(stop.await?.summary, OperationSummary::Completed);
                wait_output(&output, 1).await?;
                let desired = control.desired_snapshot().select(GraphSelection::Exact(vec![id("sequence")]))?
                    .components.into_iter().find(|node| node.descriptor.id() == &id("sequence")).unwrap();
                let preview = control.preview(
                    control.desired_snapshot().revision,
                    vec![DesiredMutation::ReplaceComponent(desired)],
                ).await?;
                let mut bindings = TopologyBindings::default();
                bindings.factories.register(factory.clone())?;
                let replaced = control.reconcile(preview, bindings).await?;
                anyhow::ensure!(replaced.committed && replaced.failures.is_empty(), "replacement failed: {replaced:?}");
                control.start_components(control.desired_snapshot().revision, GraphSelection::Exact(vec![id("sequence")])).await?;
                let after = control.observed();
                assert_ne!(before.components[&id("sequence")].generation, after.components[&id("sequence")].generation);
                for name in ["source", "collector"] {
                    assert_eq!(before.components[&id(name)].generation, after.components[&id(name)].generation);
                }
                wait_output(&output, 1).await.with_context(|| format!(
                    "first output after replacement (committed={committed}), observed={:?}, calls={:?}",
                    control.observed(), probe.trace.lock().unwrap()
                ))?;
                send.send(input(2, 3).envelope).await?;
                wait_output(&output, 2).await.context("second input after replacement")?;
                let received = output.lock().unwrap();
                assert_eq!(received.len(), 2);
                for (offset, envelope) in received.iter().enumerate() {
                    assert_eq!(values(envelope, "first_count"), [offset as i64 + 1]);
                    assert_eq!(values(envelope, "second_count"), [offset as i64 + 1]);
                    assert_eq!(values(envelope, "value"), [20]);
                }
                assert_eq!(probe.trace.lock().unwrap().len(), 4, "the drained input is not recomputed after replacement");
                assert_eq!(probe.standalone_calls.load(Ordering::Acquire), 0);
                Ok::<_, anyhow::Error>(())
            }.await;
            control.cancel();
            result
        });
        graph.dispose().await?;
        store.shutdown().await?;
        checked?;
        assert!(
            matches!(running, Ok(()) | Err(GraphError::Cancelled)),
            "{running:?}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn live_replacement_waits_for_commit_and_preserves_transaction_state_current_thread(
) -> TestResult<()> {
    tokio::time::timeout(Duration::from_secs(30), draining_replacement()).await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_replacement_waits_for_commit_and_preserves_transaction_state_multi_thread(
) -> TestResult<()> {
    tokio::time::timeout(Duration::from_secs(30), draining_replacement()).await?
}
