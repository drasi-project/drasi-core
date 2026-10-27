// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use crate::{
    client::Client,
    protocol::{
        read_message, write_message, DataHello, DataRequest, DataResponse, Lane, CONTROL_BYTES,
        PROTOCOL_VERSION,
    },
    state::{LogicalInput, SenderState},
    transport::connect,
    AcceptanceBoundary, BridgeError, BridgeSink, BridgeSource, ErrorCode, PortableEnvelopeCodec,
};
use async_trait::async_trait;
use drasi_lib::DrasiLib;
use tokio::sync::mpsc;

struct Capture {
    descriptor: ComponentDescriptor,
    output: mpsc::Sender<ChangeEnvelope>,
    gate: Option<Arc<tokio::sync::Notify>>,
}

impl Capture {
    fn new(schema: PortableDelivery) -> Result<(Self, mpsc::Receiver<ChangeEnvelope>)> {
        let (output, receiver) = mpsc::channel(16);
        Ok((
            Self {
                descriptor: ComponentDescriptor::try_new(
                    ComponentId::try_new("capture")?,
                    vec![PortDescriptor::new(
                        PortId::try_new("in")?,
                        PortDirection::Input,
                        schema.schema().descriptor().clone(),
                        PipeRequirements::default(),
                    )],
                )?,
                output,
                gate: None,
            },
            receiver,
        ))
    }
}

#[async_trait]
impl ComputationComponent for Capture {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn configuration(&self) -> Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    async fn start(&mut self) -> Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSink for Capture {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, input: InputEnvelope) -> Result<()> {
        self.output.send(input.envelope).await?;
        if let Some(gate) = &self.gate {
            gate.notified().await;
        }
        Ok(())
    }
}

async fn receiver_core(
    config: &BridgeConfig,
    path: &Path,
) -> Result<(
    DrasiLib,
    Option<Arc<QosChannel>>,
    mpsc::Receiver<ChangeEnvelope>,
)> {
    receiver_core_controlled(config, path, None, None).await
}

async fn receiver_core_controlled(
    config: &BridgeConfig,
    path: &Path,
    control: Option<Arc<crate::BridgeIngressControl>>,
    gate: Option<Arc<tokio::sync::Notify>>,
) -> Result<(
    DrasiLib,
    Option<Arc<QosChannel>>,
    mpsc::Receiver<ChangeEnvelope>,
)> {
    let (mut capture, captured) = Capture::new(config.delivery)?;
    capture.gate = gate;
    let mut specification = config.specification(ComponentRole::Source)?;
    let mut builder = ComponentBatch::builder().sink(Box::new(capture));
    if let Some(control) = control {
        let resource = ResourceId::try_new("ingress-control")?;
        specification
            .dependencies
            .insert("control".into(), vec![resource.clone()]);
        let mut declaration = declaration("ingress-control", ResourceRole::Component);
        declaration.ownership = ResourceOwnership::Graph;
        builder = builder
            .declare_resource(declaration)?
            .provide_resource(resource, control.resource())?;
    }
    let channel = if config.profile == Profile::Durable {
        let channel = channel(path, "receiver/in", "right", "bridge-in", "capture", 4).await?;
        let resource = ResourceId::try_new("receiver-journal")?;
        specification
            .dependencies
            .insert("admission".into(), vec![resource.clone()]);
        builder = builder
            .declare_resource(declaration("receiver-journal", ResourceRole::StateStore))?
            .provide_resource(resource.clone(), channel.resource())?
            .connect(
                EdgeDefinition::new(endpoint("bridge-in", "out"), endpoint("capture", "in")),
                Box::new(channel.definition().pipe(resource, "capture")),
            );
        Some(channel)
    } else {
        builder = builder.connect(
            EdgeDefinition::new(endpoint("bridge-in", "out"), endpoint("capture", "in")),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        );
        None
    };
    let batch = builder
        .component(specification, crate::factories()[0].clone())
        .bind_stream(
            endpoint("bridge-in", "out"),
            StreamId::try_new("receiver/in")?,
        )
        .build()?;
    let core = DrasiLib::builder()
        .with_id("right")
        .with_components(batch)
        .build()
        .await?;
    core.start().await?;
    Ok((core, channel, captured))
}

struct InputSource {
    descriptor: ComponentDescriptor,
    receive: mpsc::Receiver<ChangeEnvelope>,
}

#[async_trait]
impl ComputationComponent for InputSource {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn configuration(&self) -> Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    async fn start(&mut self) -> Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSource for InputSource {
    async fn next(&mut self) -> Result<Option<OutputEnvelope>> {
        Ok(self.receive.recv().await.map(|envelope| OutputEnvelope {
            port: PortId::try_new("out").expect("output port ID"),
            envelope,
        }))
    }
}

struct AdmissionOnly {
    descriptor: ComponentDescriptor,
    admission: Arc<SourceAdmission>,
}

#[async_trait]
impl ComputationComponent for AdmissionOnly {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn configuration(&self) -> Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    async fn start(&mut self) -> Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSource for AdmissionOnly {
    fn admission(&self) -> Option<Arc<SourceAdmission>> {
        Some(self.admission.clone())
    }
    async fn next(&mut self) -> Result<Option<OutputEnvelope>> {
        anyhow::bail!("admission source must use graph-owned publication")
    }
}

fn source_descriptor(schema: PortableDelivery) -> Result<ComponentDescriptor> {
    Ok(ComponentDescriptor::try_new(
        ComponentId::try_new("input")?,
        vec![PortDescriptor::new(
            PortId::try_new("out")?,
            PortDirection::Output,
            schema.schema().descriptor().clone(),
            PipeRequirements::default(),
        )],
    )?)
}

async fn wait_ack(channel: &QosChannel, subscriber: &str, count: u64) -> Result<()> {
    tokio::time::timeout(DEADLINE, async {
        loop {
            if channel.progress().await?.processed[subscriber] == count {
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await?
}

#[tokio::test(flavor = "current_thread")]
async fn factories_exchange_each_portable_schema_between_two_normal_instances() -> Result<()> {
    for (schema, envelope) in [
        (PortableDelivery::Events, graph_input(1, 42)?),
        (PortableDelivery::Events, query_input(1)?),
        (
            PortableDelivery::CompleteTransactions,
            transaction_input(1)?,
        ),
    ] {
        let directory = Directory::new()?;
        let source_config = configuration(&directory, true, Profile::Volatile, schema);
        let sink_config = configuration(&directory, false, Profile::Volatile, schema);
        let (right, _, mut captured) =
            receiver_core(&source_config, &directory.path().join("right")).await?;
        let (send, receive) = mpsc::channel(1);
        let batch = ComponentBatch::builder()
            .source(Box::new(InputSource {
                descriptor: source_descriptor(schema)?,
                receive,
            }))
            .component(
                sink_config.specification(ComponentRole::Sink)?,
                crate::factories()[1].clone(),
            )
            .connect(
                EdgeDefinition::new(endpoint("input", "out"), endpoint("bridge-out", "in")),
                Box::new(BoundedPipeConfig { capacity: 1 }),
            )
            .bind_stream(endpoint("input", "out"), StreamId::try_new("sender/out")?)
            .build()?;
        let left = DrasiLib::builder()
            .with_id("left")
            .with_components(batch)
            .build()
            .await?;
        left.start().await?;
        send.send(envelope.clone()).await?;
        let output = tokio::time::timeout(DEADLINE, captured.recv())
            .await?
            .expect("bridge output");
        assert_eq!(output.system().stream().as_str(), "receiver/in");
        assert_eq!(output.system().sequence(), 1);
        assert_eq!(output.system().timestamp(), envelope.system().timestamp());
        assert_eq!(
            output.system().source_position(),
            envelope.system().source_position()
        );
        assert_eq!(
            output.changes().operations(),
            envelope.changes().operations()
        );
        assert_eq!(output.lineage().unwrap().envelope_id(), envelope.id());
        assert!(!GraphProducerProgress::from_envelope(&output)?
            .unwrap()
            .identity()
            .persistent());
        tokio::time::timeout(DEADLINE, left.shutdown()).await??;
        tokio::time::timeout(DEADLINE, right.shutdown()).await??;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn durable_factories_use_real_acknowledged_pipes_and_preserve_complete_transactions(
) -> Result<()> {
    let directory = Directory::new()?;
    let schema = PortableDelivery::CompleteTransactions;
    let source_config = configuration(&directory, true, Profile::Durable, schema);
    let sink_config = configuration(&directory, false, Profile::Durable, schema);
    let (right, receiver_journal, mut captured) =
        receiver_core(&source_config, &directory.path().join("right")).await?;
    let receiver_journal = receiver_journal.unwrap();
    let upstream = channel(
        &directory.path().join("upstream"),
        "sender/out",
        "left",
        "input",
        "bridge-out",
        4,
    )
    .await?;
    let admission = SourceAdmission::new(upstream.clone(), PortId::try_new("out")?).await?;
    let mut specification = sink_config.specification(ComponentRole::Sink)?;
    specification
        .dependencies
        .insert("state".into(), vec![ResourceId::try_new("sender-mapping")?]);
    let batch = ComponentBatch::builder()
        .source(Box::new(AdmissionOnly {
            descriptor: source_descriptor(schema)?,
            admission: admission.clone(),
        }))
        .component(specification, crate::factories()[1].clone())
        .declare_resource(declaration("upstream", ResourceRole::StateStore))?
        .provide_resource(ResourceId::try_new("upstream")?, upstream.resource())?
        .declare_resource(declaration("sender-mapping", ResourceRole::IndexBackend))?
        .provide_resource(
            ResourceId::try_new("sender-mapping")?,
            ResourceHandle::new(
                ResourceRole::IndexBackend,
                Arc::new(QueryIndexProviderResource(provider(
                    &directory.path().join("sender"),
                ))),
            ),
        )?
        .connect(
            EdgeDefinition::new(endpoint("input", "out"), endpoint("bridge-out", "in")),
            Box::new(
                upstream
                    .definition()
                    .pipe(ResourceId::try_new("upstream")?, "bridge-out"),
            ),
        )
        .bind_stream(endpoint("input", "out"), StreamId::try_new("sender/out")?)
        .build()?;
    let left = DrasiLib::builder()
        .with_id("left")
        .with_components(batch)
        .build()
        .await?;
    left.start().await?;
    let session = admission
        .register_producer(ComponentId::try_new("producer")?)
        .await?;
    let original = transaction_input(44)?;
    admission.admit(&session, 1, &original).await?;
    let output = tokio::time::timeout(DEADLINE, captured.recv())
        .await?
        .expect("complete transaction output");
    assert_eq!(
        output.changes().operations(),
        original.changes().operations()
    );
    assert_eq!(
        output.system().source_position(),
        original.system().source_position()
    );
    assert_eq!(output.system().timestamp(), original.system().timestamp());
    let limits = SourceTransactionLimits {
        max_changes: NonZeroUsize::new(2).unwrap(),
        max_bytes: NonZeroUsize::new(65_536).unwrap(),
        max_duration_ms: std::num::NonZeroU64::new(1000).unwrap(),
    };
    assert_eq!(
        SourceTransactionCodec::decode(&output, limits)?
            .into_changes()
            .len(),
        2
    );
    wait_ack(&upstream, "bridge-out", 1).await?;
    wait_ack(&receiver_journal, "capture", 1).await?;
    assert_eq!(
        receiver_journal.progress().await?.accepted,
        1,
        "one atomic envelope, not two row admissions"
    );
    left.shutdown().await?;
    right.shutdown().await?;
    upstream.shutdown().await?;
    receiver_journal.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn full_volatile_queue_keeps_receipts_and_control_live_and_retries_fifo() -> Result<()> {
    let directory = Directory::new()?;
    let source_config = configuration(
        &directory,
        true,
        Profile::Volatile,
        PortableDelivery::Events,
    );
    let sink_config = configuration(
        &directory,
        false,
        Profile::Volatile,
        PortableDelivery::Events,
    );
    let mut source = BridgeSource::new(source_config, GRAPH.into(), None, None)?;
    let mut sink = BridgeSink::new(sink_config, GRAPH.into(), None, None)?;
    let control = sink.control();
    source.start().await?;
    sink.start().await?;
    let first = graph_input(5, 5)?;
    sink.handle(input(first.clone())).await?;
    let first_receipt = sink.last_receipt().unwrap().clone();
    assert_eq!(first_receipt.boundary, AcceptanceBoundary::VolatileQueue);
    assert_eq!(control.status().await?.accepted, 1);
    assert_eq!(
        control.receipt(&first_receipt.admission.session, 1).await?,
        Some(first_receipt)
    );
    let second = graph_input(99, 99)?;
    let error = sink.handle(input(second.clone())).await.unwrap_err();
    assert_eq!(
        error.downcast_ref::<BridgeError>().unwrap().code,
        ErrorCode::Busy
    );
    let first_output = source.next().await?.unwrap().envelope;
    assert_eq!(first_output.system().sequence(), 1);
    assert_eq!(
        first_output.changes().operations(),
        first.changes().operations()
    );
    sink.handle(input(second.clone())).await?;
    let second_receipt = sink.last_receipt().unwrap().clone();
    assert_eq!(
        second_receipt.admission.sequence, 2,
        "upstream gaps map to consecutive admission"
    );
    let second_output = source.next().await?.unwrap().envelope;
    assert_eq!(second_output.system().sequence(), 2);
    assert_eq!(
        second_output.changes().operations(),
        second.changes().operations()
    );
    sink.handle(input(second)).await?;
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(20), source.next())
            .await
            .is_err()
    );
    assert_eq!(
        sink.control()
            .receipt(&second_receipt.admission.session, 1)
            .await
            .unwrap_err()
            .code,
        ErrorCode::Expired
    );
    sink.stop().await?;
    source.stop().await?;
    Ok(())
}

async fn upstream_input(path: &Path, envelope: &ChangeEnvelope) -> Result<ChangeEnvelope> {
    let journal = channel(path, "sender/out", "left", "input", "caller", 4).await?;
    let session = journal
        .register_producer(ComponentId::try_new("upstream")?)
        .await?;
    journal.admit(&session, 1, envelope).await?;
    let id = ResourceId::try_new("upstream")?;
    let mut pipe = journal
        .definition()
        .pipe(id.clone(), "caller")
        .create_with_resources(&BTreeMap::from([(id, journal.resource())]))?;
    let mut receive = pipe.pipe.take_receiver()?;
    let result = receive
        .receive()
        .await?
        .expect("durable upstream envelope")
        .envelope()
        .clone();
    // No acknowledgement: the real upstream journal still owns this obligation.
    pipe.control.cancel();
    journal.shutdown().await?;
    Ok(result)
}

fn replay_transport(envelope: &ChangeEnvelope, sequence: u64) -> Result<ChangeEnvelope> {
    let codec = envelope_codec()?;
    let mut frame: serde_json::Value = serde_json::from_slice(&codec.encode(envelope)?)?;
    let id = emission_id(envelope.system().stream(), sequence)?;
    frame["system"]["sequence"] = serde_json::json!(sequence);
    frame["id"]["namespace"] = serde_json::json!(id.namespace());
    frame["id"]["value"] = serde_json::json!(id.value().as_ref());
    Ok(codec.decode(&serde_json::to_vec(&frame)?)?)
}

#[tokio::test(flavor = "current_thread")]
async fn lost_wire_ack_and_real_two_sided_reopen_resolve_exact_persisted_mapping() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let directory = Directory::new()?;
    let source_config = configuration(&directory, true, Profile::Durable, PortableDelivery::Events);
    let mut sink_config = configuration(
        &directory,
        false,
        Profile::Durable,
        PortableDelivery::Events,
    );
    let real_path = match &source_config.transport {
        Transport::Unix { data_path, .. } => data_path.clone(),
        _ => unreachable!(),
    };
    let proxy_path = directory.path().join("proxy.sock");
    let listener = tokio::net::UnixListener::bind(&proxy_path)?;
    std::fs::set_permissions(&proxy_path, std::fs::Permissions::from_mode(0o600))?;
    if let Transport::Unix { data_path, .. } = &mut sink_config.transport {
        *data_path = proxy_path.to_string_lossy().into();
    }
    let (right, receiver_journal, mut captured) =
        receiver_core(&source_config, &directory.path().join("receiver")).await?;
    let receiver_journal = receiver_journal.unwrap();
    let original =
        upstream_input(&directory.path().join("upstream"), &graph_input(10, 42)?).await?;
    let proxy = tokio::spawn(async move {
        let (mut from_sender, _) = listener.accept().await?;
        let mut to_receiver = tokio::net::UnixStream::connect(real_path).await?;
        let hello: DataHello =
            read_message(&mut from_sender, Lane::DataHello, CONTROL_BYTES).await?;
        write_message(&mut to_receiver, Lane::DataHello, &hello, CONTROL_BYTES).await?;
        let ready: DataResponse =
            read_message(&mut to_receiver, Lane::Receipt, CONTROL_BYTES).await?;
        write_message(&mut from_sender, Lane::Receipt, &ready, CONTROL_BYTES).await?;
        let request: DataRequest =
            read_message(&mut from_sender, Lane::Data, (1 << 20) + CONTROL_BYTES).await?;
        write_message(
            &mut to_receiver,
            Lane::Data,
            &request,
            (1 << 20) + CONTROL_BYTES,
        )
        .await?;
        let receipt: DataResponse =
            read_message(&mut to_receiver, Lane::Receipt, CONTROL_BYTES).await?;
        // Both sockets close without forwarding the actual committed receipt.
        anyhow::ensure!(matches!(&receipt, DataResponse::Receipt(_)));
        Ok::<_, anyhow::Error>(receipt)
    });
    let mut sink = BridgeSink::new(
        sink_config.clone(),
        GRAPH.into(),
        Some(provider(&directory.path().join("sender"))),
        None,
    )?;
    sink.start().await?;
    let error = sink.handle(input(original.clone())).await.unwrap_err();
    assert_eq!(
        error.downcast_ref::<BridgeError>().unwrap().code,
        ErrorCode::AcceptanceUnknown
    );
    let DataResponse::Receipt(remote_receipt) = tokio::time::timeout(DEADLINE, proxy).await???
    else {
        panic!("committed receipt");
    };
    assert!(sink.last_receipt().is_none());
    let received = tokio::time::timeout(DEADLINE, captured.recv())
        .await?
        .expect("one received envelope");
    assert_eq!(
        received.changes().operations(),
        original.changes().operations()
    );
    wait_ack(&receiver_journal, "capture", 1).await?;
    sink.stop().await?;
    right.shutdown().await?;
    receiver_journal.shutdown().await?;
    drop(sink);
    drop(receiver_journal);
    drop(right);
    std::fs::remove_file(proxy_path)?;
    if let Transport::Unix { data_path, .. } = &mut sink_config.transport {
        if let Transport::Unix {
            data_path: original,
            ..
        } = &source_config.transport
        {
            *data_path = original.clone();
        }
    }
    let (right, reopened, mut captured) =
        receiver_core(&source_config, &directory.path().join("receiver")).await?;
    let reopened = reopened.unwrap();
    let mut sink = BridgeSink::new(
        sink_config,
        GRAPH.into(),
        Some(provider(&directory.path().join("sender"))),
        None,
    )?;
    sink.start().await?;
    sink.handle(input(replay_transport(&original, u64::MAX)?))
        .await?;
    assert_eq!(sink.last_receipt(), Some(&remote_receipt));
    assert_eq!(reopened.progress().await?.accepted, 1);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(30), captured.recv())
            .await
            .is_err()
    );
    let mut conflict = original.clone();
    conflict.append_annotation(ContextEntry::try_new(
        ComponentId::try_new("test")?,
        "content-changed",
        ContextValue::Bool(true),
    )?)?;
    assert_eq!(
        sink.handle(input(conflict))
            .await
            .unwrap_err()
            .downcast_ref::<BridgeError>()
            .unwrap()
            .code,
        ErrorCode::Conflict
    );
    sink.stop().await?;
    right.shutdown().await?;
    reopened.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn receiver_state_loss_and_missing_sender_state_never_create_fresh_input() -> Result<()> {
    let directory = Directory::new()?;
    let source_config = configuration(&directory, true, Profile::Durable, PortableDelivery::Events);
    let sink_config = configuration(
        &directory,
        false,
        Profile::Durable,
        PortableDelivery::Events,
    );
    let (right, journal, mut captured) =
        receiver_core(&source_config, &directory.path().join("receiver")).await?;
    let journal = journal.unwrap();
    let original = upstream_input(&directory.path().join("upstream"), &graph_input(1, 1)?).await?;
    let mut sink = BridgeSink::new(
        sink_config.clone(),
        GRAPH.into(),
        Some(provider(&directory.path().join("sender"))),
        None,
    )?;
    sink.start().await?;
    sink.handle(input(original.clone())).await?;
    tokio::time::timeout(DEADLINE, captured.recv())
        .await?
        .expect("one input");
    wait_ack(&journal, "capture", 1).await?;
    let mut empty_sender = BridgeSink::new(
        sink_config,
        GRAPH.into(),
        Some(provider(&directory.path().join("lost-sender"))),
        None,
    )?;
    empty_sender.start().await?;
    assert_eq!(
        empty_sender
            .handle(input(original.clone()))
            .await
            .unwrap_err()
            .downcast_ref::<BridgeError>()
            .unwrap()
            .code,
        ErrorCode::StateLost
    );
    empty_sender.stop().await?;
    right.shutdown().await?;
    journal.shutdown().await?;
    drop(right);
    drop(journal);
    let (right, empty, _) =
        receiver_core(&source_config, &directory.path().join("lost-receiver")).await?;
    let empty = empty.unwrap();
    assert_eq!(
        sink.handle(input(original))
            .await
            .unwrap_err()
            .downcast_ref::<BridgeError>()
            .unwrap()
            .code,
        ErrorCode::StateLost
    );
    assert_eq!(empty.progress().await?.accepted, 0);
    sink.stop().await?;
    right.shutdown().await?;
    empty.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn malformed_peer_and_stale_connections_do_not_stop_the_receiver_or_control_lane(
) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let directory = Directory::new()?;
    let source_config = configuration(
        &directory,
        true,
        Profile::Volatile,
        PortableDelivery::Events,
    );
    let sink_config = configuration(
        &directory,
        false,
        Profile::Volatile,
        PortableDelivery::Events,
    );
    let mut source = BridgeSource::new(source_config, GRAPH.into(), None, None)?;
    source.start().await?;
    let mut client = Client::new(sink_config.clone(), None);
    let welcome = client.connect(None).await?;
    let mut io = connect(&sink_config.transport, false, None, DEADLINE).await?;
    let hello = DataHello {
        version: PROTOCOL_VERSION,
        generation: welcome.generation,
        session: welcome.session.clone(),
        contract: welcome.contract,
    };
    write_message(&mut io, Lane::DataHello, &hello, CONTROL_BYTES).await?;
    assert!(matches!(
        read_message::<_, DataResponse>(&mut io, Lane::Receipt, CONTROL_BYTES).await?,
        DataResponse::Ready { .. }
    ));
    let malformed = DataRequest {
        generation: welcome.generation,
        session: welcome.session.clone(),
        sequence: 1,
        envelope: super::codec::hostile_envelope(20_000, 0)?,
    };
    write_message(&mut io, Lane::Data, &malformed, (1 << 20) + CONTROL_BYTES).await?;
    assert!(matches!(
        read_message::<_, DataResponse>(&mut io, Lane::Receipt, CONTROL_BYTES).await?,
        DataResponse::Failure(BridgeError {
            code: ErrorCode::Invalid,
            ..
        })
    ));
    // A partial data frame occupies the sole data reader but cannot block control.
    let mut header = b"DRBG".to_vec();
    header.extend_from_slice(&PROTOCOL_VERSION.to_be_bytes());
    header.extend_from_slice(&[Lane::Data as u8, 0]);
    header.extend_from_slice(&1000u32.to_be_bytes());
    io.write_all(&header).await?;
    io.write_all(&[0x81]).await?;
    assert_eq!(
        tokio::time::timeout(DEADLINE, client.control().status())
            .await??
            .accepted,
        0
    );
    let mut replacement = Client::new(sink_config.clone(), None);
    let replacement_welcome = replacement.connect(Some(&welcome.session)).await?;
    assert!(replacement_welcome.generation > welcome.generation);
    let mut stale = connect(&sink_config.transport, false, None, DEADLINE).await?;
    write_message(&mut stale, Lane::DataHello, &hello, CONTROL_BYTES).await?;
    let stale_reply: DataResponse = read_message(&mut stale, Lane::Receipt, CONTROL_BYTES).await?;
    assert!(matches!(
        stale_reply,
        DataResponse::Failure(BridgeError {
            code: ErrorCode::StaleGeneration,
            ..
        })
    ));
    drop(stale);
    drop(io);
    let codec = PortableEnvelopeCodec::new(
        sink_config.delivery,
        sink_config.namespace.clone(),
        sink_config.limits.clone(),
    )?;
    let input = graph_input(1, 1)?;
    let receipt = replacement
        .send(&welcome.session, 1, &codec.encode(&input)?)
        .await?;
    assert_eq!(receipt.admission.sequence, 1);
    let mut conflict = input.clone();
    conflict.append_annotation(ContextEntry::try_new(
        ComponentId::try_new("peer")?,
        "conflict",
        ContextValue::Bool(true),
    )?)?;
    assert_eq!(
        replacement
            .send(&welcome.session, 1, &codec.encode(&conflict)?)
            .await
            .unwrap_err()
            .code,
        ErrorCode::Conflict
    );
    assert_eq!(
        replacement
            .send(&welcome.session, 3, &codec.encode(&input)?)
            .await
            .unwrap_err()
            .code,
        ErrorCode::Gap
    );
    assert_eq!(
        source
            .next()
            .await?
            .unwrap()
            .envelope
            .changes()
            .operations(),
        input.changes().operations()
    );
    replacement.invalidate().await;
    client.invalidate().await;
    source.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn transactional_sender_mapping_persists_gaps_rejects_expiry_and_configuration_changes(
) -> Result<()> {
    let directory = Directory::new()?;
    let config = configuration(
        &directory,
        false,
        Profile::Durable,
        PortableDelivery::Events,
    );
    let session = ProducerSession {
        incarnation: uuid::Uuid::new_v4(),
        producer: ComponentId::try_new("test")?,
        epoch: 1,
    };
    let mut state = SenderState::new(
        config.contract(false),
        Some(indexes(&directory.path().join("mapping")).await?),
    )?;
    state.load().await?;
    state.bind_session(session.clone()).await?;
    let first = LogicalInput {
        stream: "logical".into(),
        sequence: 9,
        digest: [1; 32],
    };
    state.prepare(first.clone(), vec![1]).await?;
    let saved = state.last().unwrap().clone();
    assert_eq!(saved.client_sequence, 1);
    state.shutdown().await?;
    drop(state);
    let mut state = SenderState::new(
        config.contract(false),
        Some(indexes(&directory.path().join("mapping")).await?),
    )?;
    state.load().await?;
    assert_eq!(state.last().unwrap().envelope, vec![1]);
    let receipt = crate::Receipt {
        admission: AdmissionReceipt {
            session,
            sequence: 1,
            position: 7,
        },
        boundary: AcceptanceBoundary::DurableAdmission {
            failure_scope: FailureMode::ProcessRestart,
        },
        contract: config.contract(false).fingerprint(),
    };
    state.complete(receipt).await?;
    state
        .prepare(
            LogicalInput {
                sequence: 80,
                digest: [2; 32],
                ..first.clone()
            },
            vec![2],
        )
        .await?;
    assert_eq!(state.last().unwrap().client_sequence, 2);
    assert_eq!(
        state
            .prepare(first, vec![1])
            .await
            .unwrap_err()
            .downcast_ref::<BridgeError>()
            .unwrap()
            .code,
        ErrorCode::Expired
    );
    state.shutdown().await?;
    drop(state);
    let mut changed = config;
    changed.namespace.source_prefixes.push("other/".into());
    let mut invalid = SenderState::new(
        changed.contract(false),
        Some(indexes(&directory.path().join("mapping")).await?),
    )?;
    assert!(invalid.load().await.is_err());
    invalid.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn sealed_volatile_cut_requires_actual_queue_handoff_and_revokes_old_lifetimes() -> Result<()>
{
    let directory = Directory::new()?;
    let source_config = configuration(
        &directory,
        true,
        Profile::Volatile,
        PortableDelivery::Events,
    );
    let sink_config = configuration(
        &directory,
        false,
        Profile::Volatile,
        PortableDelivery::Events,
    );
    let mut source = BridgeSource::new(source_config, GRAPH.into(), None, None)?;
    let ingress = source.ingress_control();
    let mut sink = BridgeSink::new(sink_config, GRAPH.into(), None, None)?;
    let egress = sink.egress_control();
    source.start().await?;
    sink.start().await?;
    assert!(ingress.sealed_cut().await?.is_none());
    sink.handle(input(graph_input(1, 1)?)).await?;
    let generation = sink.control().status().await?.generation;
    let cut = egress.finish(DEADLINE).await?;
    assert_eq!(cut.position, 1);
    assert_eq!(
        sink.control().status().await?.generation,
        generation,
        "observing control cannot replace data ownership"
    );
    assert_eq!(ingress.sealed_cut().await?, Some(cut.clone()));
    let status = ingress.drain_status(&cut).await?;
    assert!(!status.drained);
    assert_eq!(status.queued, 1);
    assert!(ingress
        .wait_drained(&cut, std::time::Duration::from_millis(10))
        .await
        .is_err());
    assert_eq!(
        sink.handle(input(graph_input(2, 2)?))
            .await
            .unwrap_err()
            .downcast_ref::<BridgeError>()
            .unwrap()
            .code,
        ErrorCode::Sealed
    );
    source
        .next()
        .await?
        .expect("accepted envelope still retained");
    assert!(ingress.wait_drained(&cut, DEADLINE).await?.drained);
    assert!(
        ResourceCleanup::shutdown(ingress.as_ref()).await.is_err(),
        "live dependency must retain its control resource"
    );
    sink.stop().await?;
    source.stop().await?;
    assert!(ingress.sealed_cut().await.is_err());
    source.start().await?;
    assert_eq!(
        ingress.drain_status(&cut).await.unwrap_err().code,
        ErrorCode::StaleGeneration
    );
    source.stop().await?;
    ResourceCleanup::shutdown(ingress.as_ref()).await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn durable_cut_waits_for_actual_required_subscriber_acknowledgement() -> Result<()> {
    let directory = Directory::new()?;
    let source_config = configuration(&directory, true, Profile::Durable, PortableDelivery::Events);
    let sink_config = configuration(
        &directory,
        false,
        Profile::Durable,
        PortableDelivery::Events,
    );
    let ingress = Arc::new(crate::BridgeIngressControl::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let (right, journal, mut captured) = receiver_core_controlled(
        &source_config,
        &directory.path().join("receiver"),
        Some(ingress.clone()),
        Some(release.clone()),
    )
    .await?;
    let journal = journal.unwrap();
    let original = upstream_input(&directory.path().join("upstream"), &graph_input(1, 1)?).await?;
    let mut sink = BridgeSink::new(
        sink_config,
        GRAPH.into(),
        Some(provider(&directory.path().join("sender"))),
        None,
    )?;
    sink.start().await?;
    sink.handle(input(original)).await?;
    tokio::time::timeout(DEADLINE, captured.recv())
        .await?
        .expect("consumer is handling");
    let cut = sink.egress_control().finish(DEADLINE).await?;
    assert_eq!(cut.required_subscribers, vec!["capture"]);
    assert!(!ingress.drain_status(&cut).await?.drained);
    release.notify_one();
    assert!(ingress.wait_drained(&cut, DEADLINE).await?.drained);
    sink.stop().await?;
    right.shutdown().await?;
    journal.shutdown().await?;
    assert!(ingress.sealed_cut().await.is_err());
    Ok(())
}
