// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! A downstream native query bootstraps from an upstream native query's
//! committed snapshot over the data connection, then continues from its live output
//! without counting the snapshot's results twice.

use super::*;

struct Seed;

#[async_trait]
impl ComputationBootstrapProvider for Seed {
    async fn snapshot(&self) -> Result<ComputationBootstrapSnapshot> {
        let change = GraphChangeCodec::encode_change(
            SourceChange::Insert {
                element: item_with_id("seed", 1, 1),
            },
            StreamId::try_new("seed")?,
            1,
            None,
        )?;
        Ok(ComputationBootstrapSnapshot {
            changes: Box::pin(futures_util::stream::iter([Ok(change)])),
            watermarks: Vec::new(),
        })
    }
}

/// Emits one change once the test releases it.
struct GatedSource {
    descriptor: ComponentDescriptor,
    envelope: Option<ChangeEnvelope>,
    release: Arc<Notify>,
}

#[async_trait]
impl ComputationComponent for GatedSource {
    fn description(&self) -> drasi_lib::computation::v1::ComponentDescription {
        drasi_lib::computation::v1::ComponentDescription::undeclared()
    }
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    drasi_lib::passive_component_lifecycle!();
}

#[async_trait]
impl EnvelopeSource for GatedSource {
    async fn next(&mut self) -> Result<Option<OutputEnvelope>> {
        let Some(envelope) = self.envelope.take() else {
            return Ok(None);
        };
        self.release.notified().await;
        Ok(Some(OutputEnvelope {
            execution_context: Default::default(),
            port: PortId::try_new("out")?,
            envelope,
        }))
    }
}

fn total(variables: &drasi_core::evaluation::context::QueryVariables) -> Option<i64> {
    variables.get("total")?.as_i64()
}

/// The `(before, after)` totals of every aggregate result the sink received.
fn totals(received: &Mutex<Vec<ChangeEnvelope>>) -> Result<Vec<(Option<i64>, Option<i64>)>> {
    let received = received
        .lock()
        .map_err(|_| anyhow::anyhow!("query test output poisoned"))?;
    let mut totals = Vec::new();
    for envelope in received.iter() {
        for result in QueryChangeCodec::decode_evaluation(envelope)? {
            if let QueryPartEvaluationContext::Aggregation { before, after, .. } = result {
                totals.push((before.as_ref().and_then(total), total(&after)));
            }
        }
    }
    Ok(totals)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_native_query_bootstraps_from_an_upstream_native_query_snapshot() -> Result<()> {
    let plugin = computation::load(library())?;
    let factory = plugin
        .factories()
        .iter()
        .find(|factory| factory.metadata().implementation.name.as_ref() == "drasi.query/indexed")
        .context("missing query factory")?
        .clone();
    let source = ComponentId::try_new("0f6f6b1e-7a47-5b8e-9a43-1b4f3d0c2a01")?;
    let provider = ComponentId::try_new("0f6f6b1e-7a47-5b8e-9a43-1b4f3d0c2a02")?;
    let upstream = ComponentId::try_new("0f6f6b1e-7a47-5b8e-9a43-1b4f3d0c2a03")?;
    let downstream = ComponentId::try_new("0f6f6b1e-7a47-5b8e-9a43-1b4f3d0c2a04")?;
    let sink = ComponentId::try_new("0f6f6b1e-7a47-5b8e-9a43-1b4f3d0c2a05")?;
    let endpoint = |component: &ComponentId, port: &str| -> Result<Endpoint> {
        Ok(Endpoint::new(component.clone(), PortId::try_new(port)?))
    };
    let query = |id: &ComponentId, text: &str, stream: &str| -> Result<ComponentSpecification> {
        let mut specification = factory.specification(
            id.clone(),
            json!({"query": text, "stream": stream, "volatile": true, "bootstrap": true}),
        )?;
        specification.dependencies.insert(
            "processing".into(),
            vec![ResourceId::try_new(format!("indexes-{id}"))?],
        );
        Ok(specification)
    };
    let mut builder = ComputationGraph::builder("upstream-query-snapshot");
    for id in [&upstream, &downstream] {
        let resource = ResourceId::try_new(format!("indexes-{id}"))?;
        builder = builder
            .declare_resource(ResourceSpecification {
                id: resource.clone(),
                role: ResourceRole::StateStore,
                ownership: ResourceOwnership::Borrowed,
                binding: "processing".into(),
            })?
            .provide_resource(
                resource,
                ResourceHandle::new(
                    ResourceRole::StateStore,
                    Arc::new(computation::NativeProcessingResource::new(Arc::new(
                        drasi_core::computation::InMemoryComputationProvider,
                    ))),
                ),
            )?;
    }
    let release = Arc::new(Notify::new());
    let received = Arc::new(Mutex::new(Vec::new()));
    let notification = Arc::new(Notify::new());
    let seed: BootstrapProviderFactory =
        Arc::new(
            |_: &BootstrapRequest| Ok(Arc::new(Seed) as Arc<dyn ComputationBootstrapProvider>),
        );
    let pipe = || Box::new(BoundedPipeConfig { capacity: 4 }) as Box<dyn PipeProvider>;
    let mut graph = builder
        .source(
            ComponentName::try_new("source")?,
            Box::new(GatedSource {
                descriptor: ComponentDescriptor::try_new(
                    source.clone(),
                    vec![port("out", PortDirection::Output)?],
                )?,
                envelope: Some(
                    input(
                        SourceChange::Insert {
                            element: item_with_id("live", 2, 2),
                        },
                        1,
                    )?
                    .envelope,
                ),
                release: release.clone(),
            }),
        )
        .bootstrap_provider(
            ComponentName::try_new("provider")?,
            Box::new(BootstrapProviderComponent::new(provider.clone(), seed)?),
        )
        .component(
            ComponentName::try_new("upstream")?,
            query(
                &upstream,
                "MATCH (n:Item) RETURN n.value AS value",
                "upstream/out",
            )?,
            factory.clone(),
        )
        .component(
            ComponentName::try_new("downstream")?,
            query(
                &downstream,
                "MATCH (n) RETURN count(n) AS total",
                "downstream/out",
            )?,
            factory.clone(),
        )
        .sink(
            ComponentName::try_new("sink")?,
            Box::new(Sink {
                descriptor: ComponentDescriptor::try_new(
                    sink.clone(),
                    vec![port("in", PortDirection::Input)?],
                )?,
                received: received.clone(),
                notification: notification.clone(),
            }),
        )
        .connect(
            EdgeDefinition::new(endpoint(&source, "out")?, endpoint(&upstream, "in")?),
            pipe(),
        )
        .connect(
            EdgeDefinition::new(
                endpoint(&provider, "snapshot")?,
                endpoint(&upstream, "bootstrap")?,
            ),
            pipe(),
        )
        .connect(
            EdgeDefinition::new(endpoint(&upstream, "out")?, endpoint(&downstream, "in")?),
            pipe(),
        )
        .connect(
            EdgeDefinition::new(endpoint(&downstream, "out")?, endpoint(&sink, "in")?),
            pipe(),
        )
        .bind_stream(endpoint(&source, "out")?, StreamId::try_new("source/out")?)
        .bind_stream(
            endpoint(&upstream, "out")?,
            StreamId::try_new("upstream/out")?,
        )
        .bind_stream(
            endpoint(&downstream, "out")?,
            StreamId::try_new("downstream/out")?,
        )
        .build()?;
    let run = graph.run()?;
    let control = run.control();
    let (result, checked) = tokio::join!(run, async {
        control.deployment_report().await?;
        control
            .start_requested(control.desired_snapshot().revision, GraphSelection::All)
            .await?;
        // Both queries have bootstrapped: the downstream one from the upstream
        // query's committed output, which holds the seeded row.
        for id in [&upstream, &downstream] {
            tokio::time::timeout(
                Duration::from_secs(20),
                control
                    .component_handle_by_id(id)
                    .context("query handle")?
                    .wait_started(),
            )
            .await
            .with_context(|| format!("{id} did not start: {:#?}", control.observed()))??;
        }
        release.notify_one();
        tokio::time::timeout(Duration::from_secs(20), async {
            while !totals(&received)?
                .iter()
                .any(|(_, after)| *after == Some(2))
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await
        .context("downstream query produced no live result")??;
        control.cancel();
        Ok::<_, anyhow::Error>(())
    });
    checked?;
    assert!(matches!(result, Err(GraphError::Cancelled)), "{result:?}");
    graph.dispose().await?;
    // The live row is counted once on top of the bootstrapped row.
    assert_eq!(totals(&received)?.last(), Some(&(Some(1), Some(2))));
    Ok(())
}
