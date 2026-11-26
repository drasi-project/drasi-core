// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use crate::computation::v1::*;
use bytes::Bytes;
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
};
use futures::{stream, FutureExt};

fn quota(bytes: u64) -> NonZeroU64 {
    NonZeroU64::new(bytes).unwrap()
}

fn row(i: u64) -> anyhow::Result<ChangeEnvelope> {
    Ok(GraphChangeCodec::encode_change(
        SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("orders", &i.to_string()),
                    labels: Arc::from([]),
                    effective_from: i,
                },
                properties: ElementPropertyMap::default(),
            },
        },
        StreamId::try_new("orders/out")?,
        i,
        None,
    )?)
}

fn marker(id: BootstrapSessionId, event: Event) -> anyhow::Result<ChangeEnvelope> {
    BootstrapMarker::new(id, event).envelope(
        StreamId::try_new("session")?,
        1,
        GraphChangeCodec::schema().descriptor().clone(),
    )
}

fn completion() -> Event {
    Event::Complete {
        watermarks: vec![],
        state: Some(Bytes::from_static(b"complete")),
    }
}

fn framed(id: BootstrapSessionId, i: u64) -> anyhow::Result<ChangeEnvelope> {
    BootstrapMarker::tag_record(id, &row(i)?)
}

fn wire_bytes(e: &ChangeEnvelope) -> anyhow::Result<Vec<u8>> {
    let mut codec = BinaryEnvelopeCodec::new(SnapshotBatchLimits::default().max_bytes);
    codec.register_schema(GraphChangeCodec::schema())?;
    Ok(codec.encode(e)?)
}

fn addressed(
    bytes: u64,
) -> anyhow::Result<(
    BootstrapSessionReader,
    BootstrapReply,
    BootstrapRequest,
    Arc<dyn PipeControl>,
)> {
    let mut lane = file_lane(4, quota(bytes), GraphChangeCodec::schema())?;
    let ports = BootstrapPorts::default();
    let consumer = crate::test_helpers::component_id(2);
    let input = BootstrapInputEdge {
        producer: OutputEndpoint::new(
            crate::test_helpers::component_id(1),
            PortId::try_new("snapshot")?,
        ),
        input_port: PortId::try_new("bootstrap")?,
        local: false,
    };
    ports.bind_session_pipe(
        &ports,
        0,
        BootstrapOutputEdge {
            output_port: input.producer.port.clone(),
            consumer: consumer.clone(),
            input_port: input.input_port.clone(),
        },
        input.clone(),
        (lane.pipe.sender(), lane.pipe.take_receiver()?),
        None,
    );
    let reader = BootstrapSessionReader::new(ports.clone(), input.clone());
    let request = BootstrapRequest {
        session: reader.session(),
        consumer,
        input_port: input.input_port,
        output_port: input.producer.port,
        persistent: true,
        state: None,
        origin: None,
        resume: false,
        replay_after: None,
        labels: None,
    };
    Ok((
        reader,
        BootstrapReply::open(&ports, &request)?,
        request,
        lane.control,
    ))
}

struct Provider {
    rows: u64,
    fail: bool,
}

#[async_trait]
impl ComputationBootstrapProvider for Provider {
    async fn prepare_with_state(
        &self,
        state: &dyn BootstrapState,
    ) -> anyhow::Result<BootstrapPreparation> {
        state.write(Bytes::from_static(b"intent")).await?;
        Ok(BootstrapPreparation::Ready)
    }
    async fn snapshot(&self) -> anyhow::Result<ComputationBootstrapSnapshot> {
        let fail = self.fail;
        let n = self.rows;
        Ok(ComputationBootstrapSnapshot {
            changes: stream::iter((1..=n).map(move |i| {
                anyhow::ensure!(!fail || i != n, "provider stopped mid-scan");
                row(i)
            }))
            .boxed(),
            watermarks: vec![BootstrapWatermark {
                stream: StreamId::try_new("orders/out")?,
                source_id: Some(crate::test_helpers::component_id(1)),
                sequence: 40,
                position: Some(Bytes::from_static(b"at-40")),
                producer: None,
            }],
        })
    }
}

#[tokio::test]
async fn framed_provider_publishes_an_empty_or_complete_view_or_reports_failure(
) -> anyhow::Result<()> {
    for (n, fail, bytes) in
        [(0, false, 1 << 20), (20, false, 1 << 20), (4, true, 1 << 20), (20, false, 512)]
    {
        let (mut reader, mut reply, req, control) = addressed(bytes)?;
        let provider = Provider { rows: n, fail };
        let serve = serve_bootstrap_session(&mut reply, &req, &provider);
        tokio::pin!(serve);
        let intent = tokio::select! {
            result = &mut serve => anyhow::bail!("scan finished without a durable intent acknowledgement: {result:?}"),
            item = reader.next() => match item? {
                BootstrapSessionItem::State(intent) => intent,
                item => anyhow::bail!("expected intent, got {item:?}"),
            }
        };
        assert_eq!(intent.state().as_ref(), b"intent");
        assert!(
            futures::poll!(&mut serve).is_pending(),
            "receiving intent is not a commit"
        );
        intent.acknowledge()?;
        // Only one sealed reference is needed regardless of the record count.
        let served = tokio::time::timeout(std::time::Duration::from_secs(5), serve).await?;
        if fail || bytes == 512 {
            assert!(served.is_err());
            let reason = reader.next().await.unwrap_err().to_string();
            assert!(
                reason.contains(if fail { "mid-scan" } else { "quota" }),
                "{reason}"
            );
        } else {
            served?;
            for i in 1..=n {
                let BootstrapSessionItem::Record(record) = reader.next().await? else {
                    anyhow::bail!("expected row {i}")
                };
                assert_eq!(wire_bytes(&record)?, wire_bytes(&row(i)?)?);
                assert!(!control.is_idle().await?);
            }
            let BootstrapSessionItem::Complete { watermarks, state } = reader.next().await? else {
                anyhow::bail!("missing completion")
            };
            assert_eq!(&state.unwrap()[..], b"intent");
            assert_eq!(watermarks[0].sequence, 40);
            assert_eq!(&watermarks[0].position.as_ref().unwrap()[..], b"at-40");
        }
        assert!(control.is_idle().await?);
    }
    Ok(())
}

#[tokio::test]
async fn an_intent_can_commit_while_baseline_rows_are_still_spooling() -> anyhow::Result<()> {
    let (mut lane, control) = endpoints(2, quota(1 << 20), GraphChangeCodec::schema())?;
    let mut rx = lane.receiver.take().unwrap();
    let tx = lane.sender();
    let id = BootstrapSessionId::new();
    tx.send(marker(id, Event::Begin)?).await?;
    rx.receive().await?.unwrap();
    let mut first = framed(id, 1)?;
    first.append_annotation(ContextEntry::try_new(
        crate::test_helpers::component_id(1),
        "tag",
        ContextValue::Unsigned(17),
    )?)?;
    let frames = [
        first,
        marker(
            id,
            Event::State {
                state: Bytes::from_static(b"between"),
            },
        )?,
        framed(id, 2)?,
        marker(id, completion())?,
    ];
    tx.send(frames[0].clone()).await?;
    assert!(rx.rx.is_empty(), "no unsealed prefix crosses the pipe");
    tx.send(frames[1].clone()).await?;
    let intent = rx.receive().await?.unwrap();
    assert_eq!(wire_bytes(intent.envelope())?, wire_bytes(&frames[1])?);
    tx.send(frames[2].clone()).await?;
    assert!(rx.rx.is_empty());
    tx.send(frames[3].clone()).await?;
    assert_eq!(rx.rx.len(), 1);
    control.close();
    for frame in [&frames[0], &frames[2], &frames[3]] {
        let got = rx.receive().await?.unwrap();
        assert_eq!(wire_bytes(got.envelope())?, wire_bytes(frame)?);
    }
    assert!(rx.receive().await?.is_none());
    assert!(control.is_idle().await?);
    Ok(())
}

#[tokio::test]
async fn replay_uses_the_bounded_queue_and_keeps_original_control_and_typed_failure(
) -> anyhow::Result<()> {
    let (mut lane, _) = endpoints(1, quota(1), GraphChangeCodec::schema())?;
    let mut rx = lane.receiver.take().unwrap();
    let tx = lane.sender();
    let id = BootstrapSessionId::new();
    tx.send(marker(id, Event::Begin)?).await?;
    rx.receive().await?;
    let retained = record_control(
        &row(41)?,
        Some(ControlMessage::try_new(
            "source/txn",
            Bytes::from_static(b"txn"),
        )?),
    )?;
    tx.send(BootstrapMarker::tag_replay(id, &retained)?).await?;
    let got = rx.receive().await?.unwrap();
    let Event::Replay { control } = BootstrapMarker::of(got.envelope())?.unwrap().event else {
        anyhow::bail!("missing replay marker")
    };
    assert_eq!(
        wire_bytes(&BootstrapMarker::untag_replay(got.envelope(), control))?,
        wire_bytes(&retained)?
    );
    let gap = Event::ReplayFailed(BootstrapReplayError::Gap {
        requested: 40,
        oldest: Some(45),
        latest: 50,
    });
    tx.send(marker(id, gap.clone())?).await?;
    assert_eq!(
        BootstrapMarker::of(rx.receive().await?.unwrap().envelope())?
            .unwrap()
            .event,
        gap
    );
    Ok(())
}

#[tokio::test]
async fn replacement_and_cancellation_cannot_publish_an_old_file_or_completion(
) -> anyhow::Result<()> {
    let (mut lane, control) = endpoints(1, quota(1 << 20), GraphChangeCodec::schema())?;
    let mut rx = lane.receiver.take().unwrap();
    let tx = lane.sender();
    let old = BootstrapSessionId::new();
    tx.send(marker(old, Event::Begin)?).await?;
    tx.send(framed(old, 1)?).await?;
    let mut seal = Box::pin(tx.send(marker(old, completion())?));
    assert!(seal.as_mut().now_or_never().is_none());
    drop(seal); // interrupted seal while the capacity-one lane is occupied
    rx.receive().await?;
    assert!(tx.send(marker(old, completion())?).await.is_err());
    let id = BootstrapSessionId::new();
    tx.send(marker(id, Event::Begin)?).await?;
    rx.receive().await?;
    tx.send(framed(id, 2)?).await?;
    assert!(tx.send(framed(old, 3)?).await.is_err());
    tx.send(marker(id, completion())?).await?;
    assert_eq!(rx.receive().await?.unwrap().envelope().id(), row(2)?.id());
    control.cancel(); // accepted file is being imported, completion still held
    assert!(rx.receive().await?.is_none());
    assert!(control.is_idle().await?);
    assert!(matches!(
        tx.send(marker(id, completion())?).await.unwrap_err().error,
        PipeError::Closed
    ));
    Ok(())
}

#[tokio::test]
async fn damaged_file_errors_at_the_receiver_instead_of_delivering_rows_or_end(
) -> anyhow::Result<()> {
    let (mut lane, control) = endpoints(2, quota(1 << 20), GraphChangeCodec::schema())?;
    let mut rx = lane.receiver.take().unwrap();
    let id = BootstrapSessionId::new();
    lane.sender.send(marker(id, Event::Begin)?).await?;
    rx.receive().await?;
    lane.sender.send(framed(id, 1)?).await?;
    lane.sender.send(marker(id, completion())?).await?;
    let packet = rx.rx.try_recv()?;
    packet.file.as_ref().unwrap().truncate_for_test().await?;
    lane.sender
        .tx
        .try_send(packet)
        .map_err(|_| anyhow::anyhow!("failed to return test packet"))?;
    control.close();
    let Err(PipeError::Backend(error)) = rx.receive().await else {
        anyhow::bail!("corrupt file accepted")
    };
    assert!(error.to_string().contains("length mismatch"));
    assert!(rx.receive().await?.is_none());
    assert!(control.is_idle().await?);
    Ok(())
}
