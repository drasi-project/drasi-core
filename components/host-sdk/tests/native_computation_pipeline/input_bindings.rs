// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use tokio::sync::mpsc;

struct BindingSource {
    descriptor: ComponentDescriptor,
    receiver: mpsc::Receiver<ChangeEnvelope>,
}

impl BindingSource {
    fn new(name: &str) -> Result<(Self, mpsc::Sender<ChangeEnvelope>)> {
        let (sender, receiver) = mpsc::channel(2);
        Ok((
            Self {
                descriptor: ComponentDescriptor::try_new(
                    id(name),
                    vec![PortDescriptor::new(
                        PortId::try_new("out")?,
                        PortDirection::Output,
                        GraphChangeCodec::schema().descriptor().clone(),
                        PipeRequirements::default(),
                    )],
                )?,
                receiver,
            },
            sender,
        ))
    }
}

#[async_trait]
impl ComputationComponent for BindingSource {
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
impl EnvelopeSource for BindingSource {
    async fn next(&mut self) -> Result<Option<OutputEnvelope>> {
        Ok(self.receiver.recv().await.map(|envelope| OutputEnvelope {
            port: PortId::try_new("out").expect("fixture port"),
            envelope,
        }))
    }
}

fn event(stream_name: &str, sequence: u64) -> Result<ChangeEnvelope> {
    GraphChangeCodec::encode_change(
        SourceChange::Insert {
            element: Element::Node {
                metadata: metadata(),
                properties: ElementPropertyMap::from(json!({"value":sequence})),
            },
        },
        stream(stream_name),
        sequence,
        None,
    )
    .map_err(Into::into)
}

fn capture(name: &str) -> Result<Box<dyn EnvelopeSink>> {
    let mut sink = HostCapture::new(
        Arc::new(Mutex::new(Vec::new())),
        Arc::new(AtomicBool::new(false)),
    );
    sink.descriptor = ComponentDescriptor::try_new(
        id(name),
        vec![PortDescriptor::new(
            PortId::try_new("in")?,
            PortDirection::Input,
            GraphChangeCodec::schema().descriptor().clone(),
            PipeRequirements::default(),
        )],
    )?;
    Ok(Box::new(sink))
}

async fn observations(path: &Path, count: usize) -> Result<Vec<Value>> {
    tokio::time::timeout(TIMEOUT, async {
        loop {
            match std::fs::read_to_string(path) {
                Ok(contents) if contents.ends_with('\n') => {
                    let lines: Vec<_> = contents.lines().collect();
                    if lines.len() >= count {
                        return lines
                            .into_iter()
                            .map(|line| serde_json::from_str(line).map_err(Into::into))
                            .collect();
                    }
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await?
}

async fn separately_loaded_transformer_receives_live_input_bindings_case() -> Result<()> {
    let plugin = shared::consumer_plugin()?;
    let factory = plugin
        .factories()
        .iter()
        .find(|factory| factory.metadata().implementation.name.as_ref() == "fixture/input-bindings")
        .context("missing input-binding transformer in native recovery fixture")?
        .clone();
    assert!(factory.metadata().capabilities.input_binding_updates);
    assert!(!factory.metadata().capabilities.binds_inputs);
    let directory = directory()?;
    let path = directory.path().join("input-bindings.jsonl");
    let (left, left_sender) = BindingSource::new("left")?;
    let (right, right_sender) = BindingSource::new("right")?;
    let mut graph = ComputationGraph::builder("native-input-bindings")
        .source(Box::new(left))
        .source(Box::new(right))
        .component(
            factory.specification(id("transformer"), json!({"observations":path}))?,
            factory,
        )
        .sink(capture("spare")?)
        .sink(capture("terminal")?)
        .connect(
            edge("left", "transformer"),
            Box::new(BoundedPipeConfig { capacity: 2 }),
        )
        .connect(
            edge("left", "spare"),
            Box::new(BoundedPipeConfig { capacity: 2 }),
        )
        .connect(
            edge("right", "spare"),
            Box::new(BoundedPipeConfig { capacity: 2 }),
        )
        .connect(
            edge("transformer", "terminal"),
            Box::new(BoundedPipeConfig { capacity: 2 }),
        )
        .bind_stream(endpoint("left", "out"), stream("left/out"))
        .bind_stream(endpoint("right", "out"), stream("right/out"))
        .bind_stream(endpoint("transformer", "out"), stream("transformer/out"))
        .build()?;
    let run = graph.start()?;
    let control = run.control();
    let driver = async {
        let result = async {
            let startup = control.startup_report().await?;
            assert_eq!(
                startup.summary,
                OperationSummary::Completed,
                "{startup:?}; observed: {:#?}",
                control.observed()
            );
            let generation = control.observed().components[&id("transformer")].generation;
            left_sender.send(event("left/out", 1)?).await?;
            let first = observations(&path, 1).await?;
            assert_eq!(
                first[0],
                json!({
                    "stream":"left/out",
                    "bindings":[{
                        "input_port":"in",
                        "output":{"component":"left","port":"out"},
                        "stream":"left/out"
                    }]
                })
            );
            let preview = control
                .preview(
                    control.desired_snapshot().revision,
                    vec![
                        DesiredMutation::Unbind {
                            edge: edge("left", "transformer"),
                            policy: RemovalPolicy::Reject,
                        },
                        DesiredMutation::Bind(DesiredRelationship {
                            definition: edge("right", "transformer"),
                            policy: RelationshipPolicy::default(),
                            pipe: DesiredPipe::Bounded { capacity: 2 },
                        }),
                    ],
                )
                .await?;
            assert!(!preview.replaced().contains(&id("transformer")));
            let report = control
                .reconcile(preview, TopologyBindings::default())
                .await?;
            assert!(report.committed && report.failures.is_empty(), "{report:?}");
            assert_eq!(
                control.observed().components[&id("transformer")].generation,
                generation,
                "live input binding must not reconstruct the transformer"
            );
            right_sender.send(event("right/out", 1)?).await?;
            let actual = observations(&path, 2).await?;
            assert_eq!(
                actual[1],
                json!({
                    "stream":"right/out",
                    "bindings":[{
                        "input_port":"in",
                        "output":{"component":"right","port":"out"},
                        "stream":"right/out"
                    }]
                })
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        control.cancel();
        result
    };
    let (outcome, driven) =
        tokio::time::timeout(TIMEOUT, async { tokio::join!(run, driver) }).await?;
    driven?;
    assert!(matches!(outcome, Err(GraphError::Cancelled)), "{outcome:?}");
    graph.shutdown().await?;
    Ok(())
}

async fn separately_loaded_consumer_receives_input_bindings_case() -> Result<()> {
    use drasi_computation_plugin_sdk::Scope;
    use drasi_core::interface::FailureMode;

    let plugin = shared::consumer_plugin()?;
    let factory = plugin
        .factories()
        .iter()
        .find(|factory| {
            factory.metadata().implementation.name.as_ref() == "fixture/input-binding-consumer"
        })
        .context("missing input-binding consumer in native recovery fixture")?;
    let directory = directory()?;
    let path = directory.path().join("consumer-bindings.json");
    let mut consumer = factory
        .create_consumer(
            id("consumer"),
            json!({"observations":path}),
            Scope {
                instance_id: "input-bindings".into(),
                graph_id: "input-bindings".into(),
                generation: 1,
            },
            storage(&directory.path().join("consumer-state")),
            DeliveryOptions {
                scope: RecoveryScope::Failure(FailureMode::ProcessRestart),
                max_streams: NonZeroUsize::new(2).expect("nonzero stream limit"),
                receipts_per_stream: NonZeroUsize::new(2).expect("nonzero receipt limit"),
                retry: DeliveryRetryPolicy::default(),
            },
        )
        .await?;
    assert!(consumer.binds_inputs());
    for (producer, stream_name, before_start) in
        [("left", "left/out", true), ("right", "right/out", false)]
    {
        let bindings = vec![InputBinding {
            input_port: PortId::try_new("in")?,
            output: OutputEndpoint::new(id(producer), PortId::try_new("out")?),
            stream: stream(stream_name),
        }];
        consumer.bind_inputs(&bindings).await?;
        let actual: Vec<InputBinding> = serde_json::from_slice(&std::fs::read(&path)?)?;
        assert_eq!(actual, bindings);
        if before_start {
            consumer.start().await?;
        }
    }
    consumer.stop().await?;
    Ok(())
}

#[test]
fn transaction_steps_reject_required_input_binding_updates() -> Result<()> {
    let plugin = shared::consumer_plugin()?;
    let factory = plugin
        .transactional_factories()
        .into_iter()
        .find(|factory| {
            factory.implementation().name.as_ref() == "fixture/input-binding-transactional"
        })
        .context("missing input-binding transaction fixture")?;
    let definition = TransactionStepDefinition {
        id: id("step"),
        implementation: factory.implementation(),
        configuration_version: factory.configuration_version(),
        configuration: json!({"observations": "unused"}),
    };
    let error = match factory.create(&definition) {
        Ok(_) => anyhow::bail!("transaction step unexpectedly accepted binding updates"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("native transaction steps cannot receive graph input-binding updates"),
        "{error:#}"
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn separately_loaded_transformer_receives_live_input_bindings_current_thread() -> Result<()> {
    separately_loaded_transformer_receives_live_input_bindings_case().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn separately_loaded_transformer_receives_live_input_bindings_multi_thread() -> Result<()> {
    separately_loaded_transformer_receives_live_input_bindings_case().await
}

#[tokio::test(flavor = "current_thread")]
async fn separately_loaded_consumer_receives_input_bindings_current_thread() -> Result<()> {
    separately_loaded_consumer_receives_input_bindings_case().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn separately_loaded_consumer_receives_input_bindings_multi_thread() -> Result<()> {
    separately_loaded_consumer_receives_input_bindings_case().await
}
