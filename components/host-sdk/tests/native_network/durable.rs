// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use async_trait::async_trait;
use drasi_computation_source_grpc::proto::admission as proto;
use drasi_core::{computation::ComputationIndexProvider, interface::FailureMode};
use drasi_index_rocksdb::RocksDbIndexProvider;
use drasi_lib::observability::ExecutionContext;
use std::{collections::BTreeMap, future::Future, num::NonZeroUsize};

const SOURCE_ID: &str = "4ac260d3-4506-5495-a1bf-d59b5c7dba85";
const CLIENT_ID: &str = "00000000-0000-0000-0000-000000000603";

fn durable_event(value: i64) -> source::SourceChange {
    proto_event_for(
        &ComponentId::try_new(SOURCE_ID).expect("durable source UUID"),
        value,
    )
}

struct CommitGate {
    armed: std::sync::atomic::AtomicBool,
    before: bool,
    entered: Notify,
    resume: Notify,
}
struct GatedSession {
    inner: Arc<dyn drasi_core::interface::SessionControl>,
    gate: Arc<CommitGate>,
}
#[async_trait]
impl drasi_core::interface::SessionControl for GatedSession {
    async fn begin(&self) -> std::result::Result<(), drasi_core::interface::IndexError> {
        self.inner.begin().await
    }
    async fn commit(&self) -> std::result::Result<(), drasi_core::interface::IndexError> {
        let armed = self.gate.armed.swap(false, Ordering::AcqRel);
        if armed && self.gate.before {
            self.gate.entered.notify_one();
            self.gate.resume.notified().await;
        }
        self.inner.commit().await?;
        if armed && !self.gate.before {
            self.gate.entered.notify_one();
            self.gate.resume.notified().await;
        }
        Ok(())
    }
    fn rollback(&self) -> std::result::Result<(), drasi_core::interface::IndexError> {
        self.inner.rollback()
    }
}
fn gated_indexes(
    original: drasi_core::computation::ComputationIndexes,
    gate: Arc<CommitGate>,
) -> Result<drasi_core::computation::ComputationIndexes> {
    use drasi_core::{
        computation::{ComputationIndexes, ComputationResource, TransactionDomain},
        interface::IndexSet,
    };
    let control: Arc<dyn drasi_core::interface::SessionControl> = Arc::new(GatedSession {
        inner: original.indexes().session_control.clone(),
        gate,
    });
    let domain = TransactionDomain::new(control.clone());
    let set = original.indexes();
    Ok(ComputationIndexes::try_new(
        IndexSet {
            element_index: set.element_index.clone(),
            archive_index: set.archive_index.clone(),
            result_index: set.result_index.clone(),
            future_queue: set.future_queue.clone(),
            session_control: control,
        },
        Some(domain.clone()),
        Some(ComputationResource::participating(
            original
                .checkpoint_store()
                .expect("checkpoint store")
                .clone(),
            &domain,
        )),
        Some(ComputationResource::participating(
            original.outbox_writer().expect("outbox").clone(),
            &domain,
        )),
        Some(ComputationResource::participating(
            original
                .live_results_writer()
                .expect("live results")
                .clone(),
            &domain,
        )),
    )?
    .with_cleanup(original.cleanup().expect("cleanup").clone())
    .with_durability(original.durability()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CommitStop {
    RequestTimeout,
    Force,
    Graceful,
    RetryAfterDeadline,
}

async fn network_commit_case(grpc: bool, before: bool, stop: CommitStop) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let gate = Arc::new(CommitGate {
        armed: std::sync::atomic::AtomicBool::new(false),
        before,
        entered: Notify::new(),
        resume: Notify::new(),
    });
    let port = free_port()?;
    // A graceful stop must outwait the held commit, so its admission deadline
    // is long; the other cases rely on a short one.
    let strong = stop == CommitStop::Graceful;
    let mut harness = Harness::with_gate(
        directory.path(),
        port,
        grpc,
        Some((gate.clone(), if strong { 10_000 } else { 100 })),
        "facilities-db",
    )
    .await?;
    let channel = harness.channel.clone();
    let saved = harness
        .drive(false, |control| {
            let gate = gate.clone();
            let channel = channel.clone();
            async move {
                let mut saved = None;
                let replied = Notify::new();
                let send = async {
                    if grpc {
                        let mut client = Client::connect(format!("http://127.0.0.1:{port}")).await?;
                        let session = register(&mut client).await?;
                        saved = Some(ProducerSession {
                            incarnation: session.incarnation.parse()?,
                            producer: ComponentId::try_new(session.producer.clone())?,
                            epoch: session.epoch,
                        });
                        gate.armed.store(true, Ordering::Release);
                        match client.admit(request(&session, 1, 1)).await {
                            // A graceful stop lets the commit finish and
                            // answers with its receipt.
                            Ok(response) if strong => {
                                let result = response.into_inner().result;
                                assert!(matches!(
                                    &result,
                                    Some(proto::admit_response::Result::Receipt(receipt))
                                        if receipt.position == 1
                                ), "{result:?}");
                            }
                            Ok(response) => assert!(matches!(
                                response.into_inner().result,
                                Some(proto::admit_response::Result::Error(error))
                                    if error.kind == proto::ErrorKind::AcceptanceUnknown as i32
                            )),
                            Err(error) => assert!(
                                !strong && [
                                    tonic::Code::Cancelled,
                                    tonic::Code::DeadlineExceeded,
                                    tonic::Code::Unavailable,
                                ]
                                .contains(&error.code()),
                                "{error:?}"
                            ),
                        }
                    } else {
                        let client = reqwest::Client::builder().timeout(DEADLINE).build()?;
                        let base =
                            format!("http://127.0.0.1:{port}/sources/{SOURCE_ID}/admission/v1");
                        let (_, session) =
                            post(&client, &base, "producers/register", json!({"producer":CLIENT_ID}))
                                .await?;
                        saved = Some(serde_json::from_value::<ProducerSession>(session.clone())?);
                        gate.armed.store(true, Ordering::Release);
                        let (_, outcome) = post(
                            &client,
                            &base,
                            "events",
                            json!({"session":session,"sequence":1,"event":http_event(1)}),
                        )
                        .await?;
                        if strong {
                            assert!(outcome["error"].is_null(), "{outcome}");
                            assert_eq!(outcome["receipts"][0]["position"], 1, "{outcome}");
                        } else {
                            assert_eq!(outcome["error"]["kind"], "AcceptanceUnknown", "{outcome}");
                            assert_eq!(outcome["receipts"], json!([]));
                        }
                    }
                    replied.notify_one();
                    Ok::<(), anyhow::Error>(())
                };
                let interrupt = async {
                    gate.entered.notified().await;
                    let source = ComponentId::try_new(SOURCE_ID)?;
                    let selection = || GraphSelection::Exact(vec![source.clone()]);
                    match stop {
                        CommitStop::RequestTimeout => {}
                        CommitStop::Force => {
                            replied.notified().await;
                            let report = control
                                .force_cancel_components(GraphRevision(1), GraphSelection::All)
                                .await?;
                            assert_eq!(report.cleanup.summary, OperationSummary::Completed, "{report:?}");
                            assert!(report.interrupted.contains_key(&source), "{report:?}");
                        }
                        CommitStop::Graceful => {
                            let mut observed = control.subscribe_observed();
                            let stopping = control.stop_components(GraphRevision(1), selection());
                            tokio::pin!(stopping);
                            let waiting = async {
                                observed
                                    .wait_for(|state| {
                                        state.components[&source].lifecycle
                                            == ComponentLifecycle::Quiescing
                                    })
                                    .await?;
                                Ok::<(), anyhow::Error>(())
                            };
                            tokio::select! {
                                result = &mut stopping => {
                                    anyhow::bail!("graceful stop passed the held commit: {result:?}");
                                }
                                result = waiting => result?,
                            }
                            gate.resume.notify_one();
                            replied.notified().await;
                            let report = stopping.await?;
                            assert_eq!(report.summary, OperationSummary::Completed, "{report:?}");
                        }
                        CommitStop::RetryAfterDeadline => {
                            let error = control
                                .stop_components(GraphRevision(1), selection())
                                .await
                                .expect_err("held commit must prevent graceful stop");
                            assert!(matches!(&error, GraphError::ReconciliationTimeout), "{error:?}");
                            gate.resume.notify_one();
                            assert_eq!(channel.progress().await?.accepted, 1);
                            let report =
                                control.stop_components(GraphRevision(1), selection()).await?;
                            assert_eq!(report.summary, OperationSummary::Completed, "{report:?}");
                        }
                    }
                    Ok::<(), anyhow::Error>(())
                };
                tokio::try_join!(send, interrupt)?;
                if stop == CommitStop::Force {
                    assert!(
                        channel.progress().await.is_err(),
                        "interrupted commit must fence its owner"
                    );
                } else {
                    if stop == CommitStop::RequestTimeout {
                        gate.resume.notify_one();
                    }
                    assert_eq!(
                        channel.progress().await?.accepted,
                        1,
                        "request timeout and graceful stop must not roll back graph work"
                    );
                }
                saved.context("registered client session")
            }
        })
        .await?;
    drop(channel);
    harness.shutdown().await?;

    let port = free_port()?;
    let mut harness = Harness::new(directory.path(), port, grpc).await?;
    assert_eq!(
        harness.channel.progress().await?.accepted,
        u64::from(stop != CommitStop::Force || !before),
        "grpc={grpc} before={before} stop={stop:?}"
    );
    let channel = harness.channel.clone();
    let seen = harness.seen.clone();
    harness
        .drive(true, |_| async {
            if grpc {
                let mut client = Client::connect(format!("http://127.0.0.1:{port}")).await?;
                let session: proto::ProducerSession = saved.into();
                assert_eq!(register(&mut client).await?, session);
                assert!(matches!(
                    admit(&mut client, request(&session, 1, 1)).await?,
                    proto::admit_response::Result::Receipt(value) if value.position == 1
                ));
            } else {
                let client = reqwest::Client::new();
                let base = format!("http://127.0.0.1:{port}/sources/{SOURCE_ID}/admission/v1");
                let (_, session) = post(
                    &client,
                    &base,
                    "producers/register",
                    json!({"producer":CLIENT_ID}),
                )
                .await?;
                assert_eq!(
                    serde_json::from_value::<ProducerSession>(session.clone())?,
                    saved
                );
                let (_, outcome) = post(
                    &client,
                    &base,
                    "events",
                    json!({"session":session,"sequence":1,"event":http_event(1)}),
                )
                .await?;
                assert_eq!(outcome["receipts"][0]["position"], 1, "{outcome}");
            }
            while channel.progress().await?.processed["consumer"] != 1 {
                tokio::task::yield_now().await;
            }
            assert_eq!(channel.progress().await?.accepted, 1);
            assert_eq!(*seen.lock().expect("capture lock"), [1]);
            Ok(())
        })
        .await?;
    drop(channel);
    harness.shutdown().await
}

#[tokio::test(flavor = "current_thread")]
async fn network_timeouts_and_source_stop_resolve_real_commits_without_duplicate_acceptance(
) -> Result<()> {
    for grpc in [false, true] {
        for before in [false, true] {
            for stop in [CommitStop::RequestTimeout, CommitStop::Force] {
                network_commit_case(grpc, before, stop).await?;
            }
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn network_graceful_stop_finishes_submitted_commits() -> Result<()> {
    for grpc in [false, true] {
        for before in [false, true] {
            network_commit_case(grpc, before, CommitStop::Graceful).await?;
        }
    }
    Ok(())
}

async fn graceful_stop_retries_after_deadline() -> Result<()> {
    for grpc in [false, true] {
        for before in [false, true] {
            network_commit_case(grpc, before, CommitStop::RetryAfterDeadline).await?;
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn network_graceful_stop_retries_after_boundary_timeout() -> Result<()> {
    graceful_stop_retries_after_deadline().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn network_graceful_stop_retries_after_boundary_timeout_multi_thread() -> Result<()> {
    graceful_stop_retries_after_deadline().await
}

const CONSUMER_ID: &str = "484abdae-6a47-5e92-acbc-64b4bf53fcc2";

fn only(id: &str) -> Result<GraphSelection> {
    Ok(GraphSelection::Exact(vec![ComponentId::try_new(id)?]))
}

/// One standard-route (server-sequenced) submission: `Ok(true)` on success,
/// `Ok(false)` when the source refused it as unavailable.
async fn standard_submit(grpc: bool, port: u16, value: i64) -> Result<bool> {
    if grpc {
        let mut client = source::source_service_client::SourceServiceClient::connect(format!(
            "http://127.0.0.1:{port}"
        ))
        .await?;
        match client
            .submit_event(source::SubmitEventRequest {
                event: Some(durable_event(value)),
            })
            .await
        {
            Ok(response) => Ok(response.into_inner().success),
            Err(status) if status.code() == tonic::Code::Unavailable => Ok(false),
            Err(status) => anyhow::bail!("unexpected status {status}"),
        }
    } else {
        let response = reqwest::Client::builder()
            .timeout(DEADLINE)
            .build()?
            .post(format!(
                "http://127.0.0.1:{port}/sources/{SOURCE_ID}/events"
            ))
            .json(&http_event(value))
            .send()
            .await?;
        match response.status() {
            StatusCode::OK => Ok(true),
            StatusCode::SERVICE_UNAVAILABLE => Ok(false),
            code => anyhow::bail!("unexpected status {code}: {}", response.text().await?),
        }
    }
}

/// Submits once admission is active; activation can trail listener startup.
async fn standard_submit_when_active(grpc: bool, port: u16, value: i64) -> Result<()> {
    for _ in 0..200 {
        if standard_submit(grpc, port, value).await? {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    anyhow::bail!("event {value} was never accepted")
}

/// A draining stop lets an in-flight server-sequenced commit finish. The
/// client receives success, so it never retries, and the reopened journal
/// holds the event exactly once.
#[tokio::test(flavor = "current_thread")]
async fn graceful_stop_answers_in_flight_server_sequenced_commits() -> Result<()> {
    for grpc in [false, true] {
        let directory = tempfile::tempdir()?;
        let gate = Arc::new(CommitGate {
            armed: std::sync::atomic::AtomicBool::new(false),
            before: true,
            entered: Notify::new(),
            resume: Notify::new(),
        });
        let port = free_port()?;
        let mut harness = Harness::with_gate(
            directory.path(),
            port,
            grpc,
            Some((gate.clone(), 10_000)),
            "facilities-db",
        )
        .await?;
        let channel = harness.channel.clone();
        harness
            .drive(false, |control| {
                let gate = gate.clone();
                let channel = channel.clone();
                async move {
                    // Registers the server-sequenced producer before arming.
                    standard_submit_when_active(grpc, port, 0).await?;
                    gate.armed.store(true, Ordering::Release);
                    let send = async {
                        anyhow::ensure!(
                            standard_submit(grpc, port, 1).await?,
                            "graceful stop must answer the in-flight commit with success"
                        );
                        Ok(())
                    };
                    let interrupt = async {
                        gate.entered.notified().await;
                        let source = ComponentId::try_new(SOURCE_ID)?;
                        let mut observed = control.subscribe_observed();
                        let stopping = control.stop_components(GraphRevision(1), only(SOURCE_ID)?);
                        tokio::pin!(stopping);
                        tokio::select! {
                            result = &mut stopping => {
                                anyhow::bail!("graceful stop passed the held commit: {result:?}");
                            }
                            result = observed.wait_for(|state| {
                                state.components[&source].lifecycle == ComponentLifecycle::Quiescing
                            }) => { result?; }
                        }
                        gate.resume.notify_one();
                        let report = stopping.await?;
                        assert_eq!(report.summary, OperationSummary::Completed, "{report:?}");
                        Ok(())
                    };
                    tokio::try_join!(send, interrupt)?;
                    assert_eq!(channel.progress().await?.accepted, 2);
                    Ok(())
                }
            })
            .await?;
        drop(channel);
        harness.shutdown().await?;

        let port = free_port()?;
        let mut harness = Harness::new(directory.path(), port, grpc).await?;
        let channel = harness.channel.clone();
        let seen = harness.seen.clone();
        harness
            .drive(true, |_| async {
                while channel.progress().await?.processed["consumer"] != 2 {
                    tokio::task::yield_now().await;
                }
                assert_eq!(channel.progress().await?.accepted, 2, "grpc={grpc}");
                assert_eq!(*seen.lock().expect("capture lock"), [0, 1], "grpc={grpc}");
                Ok(())
            })
            .await?;
        drop(channel);
        harness.shutdown().await?;
    }
    Ok(())
}

/// A full journal applies back-pressure instead of committing a prefix: the
/// HTTP batch and the gRPC stream wait for capacity and then complete whole,
/// so the client has nothing to retry and nothing is duplicated.
#[tokio::test(flavor = "current_thread")]
async fn full_journal_back_pressure_commits_whole_batches() -> Result<()> {
    for grpc in [false, true] {
        let directory = tempfile::tempdir()?;
        let port = free_port()?;
        let mut harness = Harness::new(directory.path(), port, grpc).await?;
        let channel = harness.channel.clone();
        let seen = harness.seen.clone();
        harness
            .drive(false, |control| async move {
                // Admission activation can trail listener startup.
                let mut client = None;
                for _ in 0..200 {
                    let base = format!("http://127.0.0.1:{port}");
                    if grpc {
                        let mut candidate =
                            source::source_service_client::SourceServiceClient::connect(base)
                                .await?;
                        if candidate.health_check(()).await.is_ok() {
                            client = Some(candidate);
                            break;
                        }
                    } else if reqwest::get(format!("{base}/health"))
                        .await?
                        .status()
                        .is_success()
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                let send = async {
                    if let Some(client) = &mut client {
                        let events = futures_util::stream::iter([1, 2, 3].map(durable_event));
                        let mut responses = client.stream_events(events).await?.into_inner();
                        let mut counts = Vec::new();
                        while let Some(response) = responses.message().await? {
                            assert!(response.success, "{response:?}");
                            counts.push(response.events_processed);
                        }
                        assert_eq!(counts, [1, 2, 3], "the stream is never torn down");
                    } else {
                        let response = reqwest::Client::builder()
                            .timeout(DEADLINE)
                            .build()?
                            .post(format!(
                                "http://127.0.0.1:{port}/sources/{SOURCE_ID}/events/batch"
                            ))
                            .json(&json!({"events":[http_event(1),http_event(2),http_event(3)]}))
                            .send()
                            .await?;
                        assert_eq!(response.status(), StatusCode::OK);
                        let body: Value = response.json().await?;
                        assert_eq!(body["events_processed"], 3, "{body}");
                    }
                    Ok::<(), anyhow::Error>(())
                };
                let release = async {
                    while channel.progress().await?.accepted < 2 {
                        tokio::task::yield_now().await;
                    }
                    // The journal (capacity 2) is full; the third event waits.
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    assert_eq!(channel.progress().await?.accepted, 2);
                    let report = control
                        .start_components(GraphRevision(1), only(CONSUMER_ID)?)
                        .await?;
                    assert_eq!(report.summary, OperationSummary::Completed, "{report:?}");
                    Ok(())
                };
                tokio::try_join!(send, release)?;
                while channel.progress().await?.processed["consumer"] != 3 {
                    tokio::task::yield_now().await;
                }
                assert_eq!(channel.progress().await?.accepted, 3);
                assert_eq!(*seen.lock().expect("capture lock"), [1, 2, 3]);
                Ok(())
            })
            .await?;
        harness.shutdown().await?;
    }
    Ok(())
}

/// While the graph has the source parked, producer metadata (status and
/// receipts) is answered, data admission is refused cleanly (no commit, a
/// retryable `Busy` or 503) and Resume continues the same sequence.
#[tokio::test(flavor = "current_thread")]
async fn paused_durable_sources_answer_metadata_and_resume_the_sequence() -> Result<()> {
    for grpc in [false, true] {
        let directory = tempfile::tempdir()?;
        let port = free_port()?;
        let mut harness = Harness::new(directory.path(), port, grpc).await?;
        let channel = harness.channel.clone();
        harness
            .drive(true, |control| async move {
                let pause = || async {
                    let parked = control
                        .quiesce_components(GraphRevision(1), only(SOURCE_ID)?)
                        .await?;
                    assert_eq!(parked, [ComponentId::try_new(SOURCE_ID)?]);
                    Ok::<(), anyhow::Error>(())
                };
                let resume = || async {
                    let report = control
                        .start_components(GraphRevision(1), only(SOURCE_ID)?)
                        .await?;
                    assert_eq!(report.summary, OperationSummary::Completed, "{report:?}");
                    Ok::<(), anyhow::Error>(())
                };
                if grpc {
                    let mut client = Client::connect(format!("http://127.0.0.1:{port}")).await?;
                    let session = register(&mut client).await?;
                    assert!(matches!(
                        admit(&mut client, request(&session, 1, 1)).await?,
                        proto::admit_response::Result::Receipt(value) if value.position == 1
                    ));
                    pause().await?;
                    let status = client
                        .producer_status(proto::SessionRequest {
                            source_id: SOURCE_ID.into(),
                            session: Some(session.clone()),
                        })
                        .await?
                        .into_inner()
                        .result;
                    assert!(
                        matches!(
                            &status,
                            Some(proto::status_response::Result::Status(status))
                                if status.next_sequence == Some(2)
                        ),
                        "{status:?}"
                    );
                    let receipt = client
                        .admission_receipt(proto::ReceiptRequest {
                            source_id: SOURCE_ID.into(),
                            session: Some(session.clone()),
                            sequence: 1,
                        })
                        .await?
                        .into_inner()
                        .result;
                    assert!(
                        matches!(
                            &receipt,
                            Some(proto::receipt_response::Result::Receipt(receipt))
                                if receipt.position == 1
                        ),
                        "{receipt:?}"
                    );
                    let refused = client
                        .admit(request(&session, 2, 2))
                        .await?
                        .into_inner()
                        .result;
                    assert!(
                        matches!(
                            &refused,
                            Some(proto::admit_response::Result::Error(error))
                                if error.kind == proto::ErrorKind::Busy as i32
                        ),
                        "{refused:?}"
                    );
                    assert!(!standard_submit(grpc, port, 9).await?);
                    assert_eq!(channel.progress().await?.accepted, 1);
                    resume().await?;
                    assert!(matches!(
                        admit(&mut client, request(&session, 2, 2)).await?,
                        proto::admit_response::Result::Receipt(value) if value.position == 2
                    ));
                } else {
                    let client = reqwest::Client::builder().timeout(DEADLINE).build()?;
                    let base = format!("http://127.0.0.1:{port}/sources/{SOURCE_ID}/admission/v1");
                    let (_, session) = post(
                        &client,
                        &base,
                        "producers/register",
                        json!({"producer":CLIENT_ID}),
                    )
                    .await?;
                    let (_, outcome) = post(
                        &client,
                        &base,
                        "events",
                        json!({"session":session,"sequence":1,"event":http_event(1)}),
                    )
                    .await?;
                    assert_eq!(outcome["receipts"][0]["position"], 1, "{outcome}");
                    pause().await?;
                    let status = client
                        .post(format!("{base}/producers/status"))
                        .json(&json!({"session":session}))
                        .send()
                        .await?;
                    assert_eq!(status.status(), StatusCode::OK);
                    assert_eq!(status.json::<Value>().await?["next_sequence"], 2);
                    let receipt = client
                        .post(format!("{base}/receipts"))
                        .json(&json!({"session":session,"sequence":1}))
                        .send()
                        .await?;
                    assert_eq!(receipt.status(), StatusCode::OK);
                    assert_eq!(receipt.json::<Value>().await?["position"], 1);
                    let refused = client
                        .post(format!("{base}/events"))
                        .json(&json!({"session":session,"sequence":2,"event":http_event(2)}))
                        .send()
                        .await?;
                    assert_eq!(refused.status(), StatusCode::TOO_MANY_REQUESTS);
                    let refused: Value = refused.json().await?;
                    assert_eq!(refused["error"]["kind"], "Busy", "{refused}");
                    assert_eq!(refused["receipts"], json!([]));
                    assert!(!standard_submit(grpc, port, 9).await?);
                    assert_eq!(channel.progress().await?.accepted, 1);
                    resume().await?;
                    let (_, outcome) = post(
                        &client,
                        &base,
                        "events",
                        json!({"session":session,"sequence":2,"event":http_event(2)}),
                    )
                    .await?;
                    assert_eq!(outcome["receipts"][0]["position"], 2, "{outcome}");
                }
                assert_eq!(channel.progress().await?.accepted, 2);
                Ok(())
            })
            .await?;
        harness.shutdown().await?;
    }
    Ok(())
}

/// Durable webhooks commit across a restart: implicit deliveries resume the
/// server sequence, and an explicit-session delivery retried before and after
/// the restart returns its original receipt instead of a second event.
#[tokio::test(flavor = "current_thread")]
async fn durable_webhooks_resume_and_dedupe_explicit_session_retries() -> Result<()> {
    let webhooks = json!({"webhooks":{"routes":[{
        "path":"/hook","methods":["POST"],
        "mappings":[{"operation":"insert","elementType":"node",
            "effectiveFrom":{"value":"{{payload.time}}","format":"unix_millis"},
            "template":{"id":"row-{{payload.value}}","labels":["Room"],"properties":{"value":"{{payload.value}}"}}}]
    }]}});
    async fn deliver(
        port: u16,
        value: i64,
        session: Option<&Value>,
    ) -> Result<(StatusCode, Value)> {
        let mut request = reqwest::Client::builder()
            .timeout(DEADLINE)
            .build()?
            .post(format!("http://127.0.0.1:{port}/hook"))
            .json(&json!({"value":value,"time":1234567}));
        if let Some(session) = session {
            request = request
                .header("x-drasi-producer-session", session.to_string())
                .header("x-drasi-first-sequence", "1");
        }
        let response = request.send().await?;
        Ok((response.status(), response.json().await?))
    }
    let directory = tempfile::tempdir()?;
    let mut saved = None;
    for reopened in [false, true] {
        let port = free_port()?;
        let mut harness = Harness::with_config(
            directory.path(),
            port,
            false,
            None,
            "facilities-db",
            webhooks.clone(),
        )
        .await?;
        let channel = harness.channel.clone();
        let seen = harness.seen.clone();
        saved = Some(
            harness
                .drive(reopened, |_| async {
                    let client = reqwest::Client::builder().timeout(DEADLINE).build()?;
                    let base = format!("http://127.0.0.1:{port}/sources/{SOURCE_ID}/admission/v1");
                    let (_, session) = post(
                        &client,
                        &base,
                        "producers/register",
                        json!({"producer":CLIENT_ID}),
                    )
                    .await?;
                    if !reopened {
                        let (code, implicit) = deliver(port, 1, None).await?;
                        assert_eq!(code, StatusCode::OK, "{implicit}");
                        assert_eq!(implicit["events_processed"], 1);
                        let (code, first) = deliver(port, 2, Some(&session)).await?;
                        assert_eq!(code, StatusCode::OK, "{first}");
                        let (_, retry) = deliver(port, 2, Some(&session)).await?;
                        assert_eq!(retry["receipts"], first["receipts"], "{retry}");
                        assert_eq!(channel.progress().await?.accepted, 2);
                        return Ok((session, first["receipts"].clone()));
                    }
                    let (session_before, receipts) = saved.clone().context("first run")?;
                    assert_eq!(session, session_before);
                    let (_, retry) = deliver(port, 2, Some(&session)).await?;
                    assert_eq!(retry["receipts"], receipts, "{retry}");
                    let (code, implicit) = deliver(port, 3, None).await?;
                    assert_eq!(code, StatusCode::OK, "{implicit}");
                    while channel.progress().await?.processed["consumer"] != 3 {
                        tokio::task::yield_now().await;
                    }
                    assert_eq!(channel.progress().await?.accepted, 3);
                    assert_eq!(*seen.lock().expect("capture lock"), [1, 2, 3]);
                    Ok((session, receipts))
                })
                .await?,
        );
        drop(channel);
        harness.shutdown().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_network_admission_crash_child() -> Result<()> {
    let Some(path) = std::env::var_os("DRASI_NETWORK_ADMISSION_CRASH") else {
        return Ok(());
    };
    let path = PathBuf::from(path);
    let grpc = std::env::var("DRASI_NETWORK_ADMISSION_GRPC")? == "1";
    let port = free_port()?;
    let mut harness = Harness::new(&path, port, grpc).await?;
    harness.drive(false, |_| async {
        let saved;
        if grpc {
            let mut client = Client::connect(format!("http://127.0.0.1:{port}")).await?;
            let session = register(&mut client).await?;
            saved = json!({"incarnation":session.incarnation,"producer":session.producer,"epoch":session.epoch});
            std::fs::write(path.join("client-session.json"), serde_json::to_vec(&saved)?)?;
            assert!(matches!(admit(&mut client, request(&session, 1, 1)).await?, proto::admit_response::Result::Receipt(value) if value.position == 1));
        } else {
            let client = reqwest::Client::new();
            let base = format!("http://127.0.0.1:{port}/sources/{SOURCE_ID}/admission/v1");
            let (_, session) = post(&client, &base, "producers/register", json!({"producer":CLIENT_ID})).await?;
            saved = session.clone();
            std::fs::write(path.join("client-session.json"), serde_json::to_vec(&saved)?)?;
            let (_, outcome) = post(&client, &base, "events", json!({"session":session,"sequence":1,"event":http_event(1)})).await?;
            assert_eq!(outcome["receipts"][0]["position"], 1, "{outcome}");
        }
        // Neither the server nor the client runs cleanup, and the client never
        // records its receipt. Only the session and intended next input survive.
        std::process::exit(94)
    }).await
}

#[tokio::test(flavor = "current_thread")]
async fn both_native_network_protocols_recover_after_process_exit_without_client_receipt(
) -> Result<()> {
    for grpc in [false, true] {
        let directory = tempfile::tempdir()?;
        let output = std::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "durable_tests::native_network_admission_crash_child",
                "--nocapture",
            ])
            .env("DRASI_NETWORK_ADMISSION_CRASH", directory.path())
            .env("DRASI_NETWORK_ADMISSION_GRPC", if grpc { "1" } else { "0" })
            .output()?;
        assert_eq!(
            output.status.code(),
            Some(94),
            "child did not reach its crash point:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let saved: ProducerSession = serde_json::from_slice(&std::fs::read(
            directory.path().join("client-session.json"),
        )?)?;
        let port = free_port()?;
        let mut harness = Harness::new(directory.path(), port, grpc).await?;
        let channel = harness.channel.clone();
        let seen = harness.seen.clone();
        harness.drive(true, |_| async {
            if grpc {
                let mut client = Client::connect(format!("http://127.0.0.1:{port}")).await?;
                let session: proto::ProducerSession = saved.into();
                assert_eq!(register(&mut client).await?, session);
                assert!(matches!(admit(&mut client, request(&session, 1, 1)).await?, proto::admit_response::Result::Receipt(value) if value.position == 1));
            } else {
                let client = reqwest::Client::new();
                let base = format!("http://127.0.0.1:{port}/sources/{SOURCE_ID}/admission/v1");
                let (_, session) = post(&client, &base, "producers/register", json!({"producer":CLIENT_ID})).await?;
                assert_eq!(serde_json::from_value::<ProducerSession>(session.clone())?, saved);
                let (_, outcome) = post(&client, &base, "events", json!({"session":session,"sequence":1,"event":http_event(1)})).await?;
                assert_eq!(outcome["receipts"][0]["position"], 1, "{outcome}");
            }
            while channel.progress().await?.processed["consumer"] != 1 { tokio::task::yield_now().await; }
            assert_eq!(channel.progress().await?.accepted, 1);
            assert_eq!(*seen.lock().unwrap(), [1]);
            Ok(())
        }).await?;
        drop(channel);
        harness.shutdown().await?;
    }
    Ok(())
}

struct Capture {
    descriptor: ComponentDescriptor,
    seen: Arc<Mutex<Vec<i64>>>,
    contexts: Arc<Mutex<Vec<ExecutionContext>>>,
}
#[async_trait]
impl ComputationComponent for Capture {
    fn description(&self) -> drasi_lib::computation::v1::ComponentDescription {
        drasi_lib::computation::v1::ComponentDescription::undeclared()
    }
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    drasi_lib::passive_component_lifecycle!();
}
#[async_trait]
impl EnvelopeSink for Capture {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, input: InputEnvelope) -> Result<()> {
        assert!(GraphProducerProgress::from_envelope(&input.envelope)?
            .context("producer")?
            .identity()
            .persistent());
        self.seen
            .lock()
            .expect("capture lock")
            .push(source_value(&input.envelope)?);
        self.contexts
            .lock()
            .expect("context capture lock")
            .push(input.execution_context);
        Ok(())
    }
}
struct Harness {
    graph: ComputationGraph,
    channel: Arc<QosChannel>,
    seen: Arc<Mutex<Vec<i64>>>,
    contexts: Arc<Mutex<Vec<ExecutionContext>>>,
}
impl Harness {
    async fn new(path: &Path, port: u16, grpc: bool) -> Result<Self> {
        Self::with_gate(path, port, grpc, None, "facilities-db").await
    }
    /// `gate` holds the next armed commit; its admission deadline (ms) is
    /// given with it. Ungated harnesses use 2000 ms.
    async fn with_gate(
        path: &Path,
        port: u16,
        grpc: bool,
        gate: Option<(Arc<CommitGate>, u64)>,
        configured_stream: &str,
    ) -> Result<Self> {
        Self::with_config(path, port, grpc, gate, configured_stream, json!({})).await
    }
    /// As [`Self::with_gate`], with `extra` merged into the source configuration.
    async fn with_config(
        path: &Path,
        port: u16,
        grpc: bool,
        gate: Option<(Arc<CommitGate>, u64)>,
        configured_stream: &str,
        extra: Value,
    ) -> Result<Self> {
        let timeout_ms = gate.as_ref().map_or(2000, |(_, timeout)| *timeout);
        let gate = gate.map(|(gate, _)| gate);
        let indexes = LegacyIndexProviderAdapter::new(Arc::new(RocksDbIndexProvider::new(
            path, false, false,
        )));
        let definition = QosChannelDefinition {
            stream: stream("facilities-db"),
            capacity: NonZeroUsize::new(2).expect("capacity"),
            durable: true,
            retention: RetentionPolicy::Backpressure,
            subscribers: BTreeMap::from([("consumer".into(), SubscriptionStart::Earliest)]),
        };
        let original = indexes
            .create_indexes(
                "durable-network",
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000127")
                    .expect("fixture UUID"),
            )
            .await?;
        let indexes = match &gate {
            Some(gate) => gated_indexes(original, gate.clone())?,
            None => original,
        };
        let channel = QosChannel::persistent(
            definition.clone(),
            indexes,
            FactoryRegistry::standard()
                .envelope_codec(NonZeroUsize::new(1024 * 1024).expect("codec limit"))?,
            "journal",
        )
        .await?;
        channel
            .enable_admission(AdmissionOptions {
                construction_scope: "durable-network".into(),
                graph_id: "durable-network".into(),
                component_id: ComponentId::try_new(SOURCE_ID)?,
                failure_scope: FailureMode::ProcessRestart,
                max_producers: NonZeroUsize::new(2).expect("producer limit"),
                receipts_per_producer: NonZeroUsize::new(2).expect("receipt limit"),
            })
            .await?;
        let plugin = plugin(if grpc { GRPC_SOURCE } else { HTTP_SOURCE })?;
        let factory = plugin
            .factories()
            .iter()
            .find(|factory| {
                factory.metadata().implementation.name.as_ref()
                    == if grpc { GRPC_SOURCE } else { HTTP_SOURCE }
            })
            .context("source factory")?
            .clone();
        let resource = ResourceId::try_new("journal")?;
        let mut configuration = json!({"stream":configured_stream,"host":"127.0.0.1","port":port,"timeoutMs":timeout_ms,"ingressCapacity":4});
        if let (Some(configuration), Some(extra)) =
            (configuration.as_object_mut(), extra.as_object())
        {
            configuration.extend(extra.clone());
        }
        let mut specification =
            factory.specification(ComponentId::try_new(SOURCE_ID)?, configuration)?;
        specification
            .dependencies
            .insert("admission".into(), vec![resource.clone()]);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let contexts = Arc::new(Mutex::new(Vec::new()));
        let source = Endpoint::new(ComponentId::try_new(SOURCE_ID)?, PortId::try_new("out")?);
        let target = Endpoint::new(
            ComponentId::try_new("484abdae-6a47-5e92-acbc-64b4bf53fcc2")?,
            PortId::try_new("in")?,
        );
        let graph = ComputationGraph::builder("durable-network")
            .declare_resource(ResourceSpecification {
                id: resource.clone(),
                role: ResourceRole::StateStore,
                ownership: ResourceOwnership::Borrowed,
                binding: "journal".into(),
            })?
            .provide_resource(resource.clone(), channel.resource())?
            .component(
                drasi_lib::computation::v1::ComponentName::try_new("fixture-component-1")
                    .expect("fixture component name"),
                specification,
                factory,
            )
            .sink(
                drasi_lib::computation::v1::ComponentName::try_new("fixture-sink-1")
                    .expect("fixture component name"),
                Box::new(Capture {
                    descriptor: ComponentDescriptor::try_new(
                        ComponentId::try_new("484abdae-6a47-5e92-acbc-64b4bf53fcc2")?,
                        vec![PortDescriptor::new(
                            PortId::try_new("in")?,
                            PortDirection::Input,
                            GraphChangeCodec::schema().descriptor().clone(),
                            PipeRequirements::default(),
                        )],
                    )?,
                    seen: seen.clone(),
                    contexts: contexts.clone(),
                }),
            )
            .bind_stream(source.clone(), definition.stream.clone())
            .connect(
                EdgeDefinition::new(source, target),
                Box::new(definition.pipe(resource, "consumer")),
            )
            .build()?;
        Ok(Self {
            graph,
            channel,
            seen,
            contexts,
        })
    }
    async fn drive<F, W, T>(&mut self, consumers: bool, work: F) -> Result<T>
    where
        F: FnOnce(GraphControl) -> W,
        W: Future<Output = Result<T>>,
    {
        let run = self.graph.run()?;
        let control = run.control();
        let client = async {
            let result = async {
                assert_eq!(
                    control.deployment_report().await?.summary,
                    OperationSummary::Completed
                );
                let selection = if consumers {
                    GraphSelection::All
                } else {
                    GraphSelection::Exact(vec![ComponentId::try_new(SOURCE_ID)?])
                };
                let report = control
                    .start_components(GraphRevision(1), selection)
                    .await?;
                assert_eq!(report.summary, OperationSummary::Completed, "{report:?}");
                work(control.clone()).await
            }
            .await;
            control.cancel();
            result
        };
        let (run, result) =
            tokio::time::timeout(Duration::from_secs(30), async { tokio::join!(run, client) })
                .await?;
        assert!(matches!(run, Err(GraphError::Cancelled)), "{run:?}");
        result
    }
    async fn shutdown(mut self) -> Result<()> {
        self.graph.shutdown().await?;
        drop(self.graph);
        self.channel.shutdown().await?;
        Ok(())
    }
}

#[tokio::test(flavor = "current_thread")]
async fn durable_source_rejects_configured_stream_mismatch_before_listening() -> Result<()> {
    for grpc in [false, true] {
        let directory = tempfile::tempdir()?;
        let port = free_port()?;
        let mut harness =
            Harness::with_gate(directory.path(), port, grpc, None, "wrong-stream").await?;
        let run = harness.graph.run()?;
        let control = run.control();
        let inspect = async {
            let report = control.deployment_report().await;
            control.cancel();
            report
        };
        let (run, report) =
            tokio::time::timeout(DEADLINE, async { tokio::join!(run, inspect) }).await?;
        assert!(matches!(run, Err(GraphError::Cancelled)), "{run:?}");
        let report = report?;
        assert_eq!(report.summary, OperationSummary::CompletedWithFailures);
        let failure = &report.components[&ComponentId::try_new(SOURCE_ID)?];
        assert!(
            matches!(failure, CreationOutcome::CreationFailed(_)),
            "{failure:?}"
        );
        assert!(
            format!("{failure:?}")
                .contains("native source identity or stream does not match its admission channel"),
            "{failure:?}"
        );
        assert_eq!(harness.channel.progress().await?.accepted, 0);
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
        drop(listener);
        harness.shutdown().await?;
    }
    Ok(())
}

/// Standard routes and the published `SourceService` commit through the
/// source's own server-sequenced producer, and a reopened journal resumes its
/// sequence rather than conflicting with committed events.
#[tokio::test(flavor = "current_thread")]
async fn standard_routes_commit_through_the_server_sequenced_producer() -> Result<()> {
    async fn submit(grpc: bool, port: u16, value: i64) -> Result<()> {
        for _ in 0..200 {
            let accepted = if grpc {
                let mut client = source::source_service_client::SourceServiceClient::connect(
                    format!("http://127.0.0.1:{port}"),
                )
                .await?;
                match client
                    .submit_event(source::SubmitEventRequest {
                        event: Some(durable_event(value)),
                    })
                    .await
                {
                    Ok(response) => response.into_inner().success,
                    Err(status) if status.code() == tonic::Code::Unavailable => false,
                    Err(status) => anyhow::bail!("unexpected status {status}"),
                }
            } else {
                let response = reqwest::Client::builder()
                    .timeout(DEADLINE)
                    .build()?
                    .post(format!(
                        "http://127.0.0.1:{port}/sources/{SOURCE_ID}/events"
                    ))
                    .json(&http_event(value))
                    .send()
                    .await?;
                match response.status() {
                    StatusCode::OK => true,
                    StatusCode::SERVICE_UNAVAILABLE => false,
                    code => anyhow::bail!("unexpected status {code}: {}", response.text().await?),
                }
            };
            if accepted {
                return Ok(());
            }
            // Admission activation can trail listener startup; retry.
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        anyhow::bail!("event {value} was never accepted")
    }
    for grpc in [false, true] {
        let directory = tempfile::tempdir()?;
        for reopened in [false, true] {
            let port = free_port()?;
            let mut harness = Harness::new(directory.path(), port, grpc).await?;
            let channel = harness.channel.clone();
            let seen = harness.seen.clone();
            harness
                .drive(true, |_| async {
                    let values: &[i64] = if reopened { &[3] } else { &[1, 2] };
                    for value in values {
                        submit(grpc, port, *value).await?;
                    }
                    let expected = if reopened { 3 } else { 2 };
                    while channel.progress().await?.accepted != expected {
                        tokio::task::yield_now().await;
                    }
                    // Every committed value reaches the consumer; a reopened
                    // journal may also redeliver earlier unacknowledged ones.
                    while !values
                        .iter()
                        .all(|value| seen.lock().unwrap().contains(value))
                    {
                        tokio::task::yield_now().await;
                    }
                    Ok(())
                })
                .await?;
            drop(channel);
            harness.shutdown().await?;
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn incoming_durable_network_context_survives_admission_callbacks() -> Result<()> {
    let parent = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-00";
    let expected =
        ExecutionContext::from_w3c_headers(Some(parent.as_bytes()), Some(b"vendor=value"));
    for grpc in [false, true] {
        let directory = tempfile::tempdir()?;
        let port = free_port()?;
        let mut harness = Harness::new(directory.path(), port, grpc).await?;
        let contexts = harness.contexts.clone();
        let channel = harness.channel.clone();
        harness
            .drive(true, |_| async {
                if grpc {
                    let mut client = Client::connect(format!("http://127.0.0.1:{port}")).await?;
                    let session = register(&mut client).await?;
                    let mut incoming = tonic::Request::new(request(&session, 1, 1));
                    incoming
                        .metadata_mut()
                        .insert("traceparent", parent.parse()?);
                    incoming
                        .metadata_mut()
                        .insert("tracestate", "vendor=value".parse()?);
                    assert!(matches!(
                        client.admit(incoming).await?.into_inner().result,
                        Some(proto::admit_response::Result::Receipt(_))
                    ));
                    let response = client.admit(request(&session, 2, 2)).await?.into_inner();
                    assert!(matches!(
                        response.result,
                        Some(proto::admit_response::Result::Receipt(_))
                    ));
                } else {
                    let client = reqwest::Client::builder().timeout(DEADLINE).build()?;
                    let base = format!("http://127.0.0.1:{port}/sources/{SOURCE_ID}/admission/v1");
                    let (_, session) = post(
                        &client,
                        &base,
                        "producers/register",
                        json!({"producer":CLIENT_ID}),
                    )
                    .await?;
                    let outcome = client
                        .post(format!("{base}/events"))
                        .header("traceparent", parent)
                        .header("tracestate", "vendor=value")
                        .json(&json!({"session":session,"sequence":1,"event":http_event(1)}))
                        .send()
                        .await?
                        .error_for_status()?
                        .json::<Value>()
                        .await?;
                    assert!(outcome["error"].is_null(), "{outcome}");
                    let (status, outcome) = post(
                        &client,
                        &base,
                        "events",
                        json!({"session":session,"sequence":2,"event":http_event(2)}),
                    )
                    .await?;
                    assert_eq!(status, StatusCode::OK, "{outcome}");
                }
                while channel.progress().await?.processed["consumer"] != 2 {
                    tokio::task::yield_now().await;
                }
                assert_eq!(
                    *contexts.lock().expect("context capture lock"),
                    vec![expected.clone(), ExecutionContext::default()]
                );
                Ok(())
            })
            .await?;
        harness.shutdown().await?;
    }
    Ok(())
}

async fn post(
    client: &reqwest::Client,
    base: &str,
    path: &str,
    body: Value,
) -> Result<(StatusCode, Value)> {
    loop {
        let response = client
            .post(format!("{base}/{path}"))
            .json(&body)
            .send()
            .await?;
        let code = response.status();
        let body: Value = response.json().await?;
        if body["error"]["kind"] == "Busy" || body["error"]["kind"] == "Closed" {
            tokio::task::yield_now().await;
        } else {
            return Ok((code, body));
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn http_durable_receipts_partial_batch_reconstruction_and_retirement() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut saved_session = Value::Null;
    let mut saved_receipts = Value::Null;
    for reopened in [false, true] {
        let port = free_port()?;
        let mut harness = Harness::new(directory.path(), port, false).await?;
        let channel = harness.channel.clone();
        let seen = harness.seen.clone();
        let (session, receipts) = harness.drive(reopened, |_| async {
            let client = reqwest::Client::builder().timeout(DEADLINE).build()?;
            let base = format!("http://127.0.0.1:{port}/sources/{SOURCE_ID}/admission/v1");
            let (code, session) = post(&client, &base, "producers/register", json!({"producer":CLIENT_ID})).await?;
            assert_eq!(code, StatusCode::OK, "{session}");
            let (_, duplicate) = post(&client, &base, "producers/register", json!({"producer":CLIENT_ID})).await?;
            assert_eq!(session, duplicate);
            if !reopened {
                let mut event = http_event(1);
                event.as_object_mut().unwrap().remove("timestamp");
                let (code, _) = post(&client, &base, "events", json!({"session":session,"sequence":1,"event":event})).await?;
                assert_eq!(code, StatusCode::BAD_REQUEST);
                let (code, _) = post(&client, &base, "events/batch",
                    json!({"session":session,"first_sequence":1,"events":[http_event(1),{"operation":"delete","timestamp":1,"id":""}]})).await?;
                assert_eq!(code, StatusCode::BAD_REQUEST);
                assert_eq!(channel.progress().await?.accepted, 0);
                let (code, outcome) = post(&client, &base, "events/batch",
                    json!({"session":session,"first_sequence":1,"events":[http_event(1),http_event(2),http_event(3)]})).await?;
                // The full journal applies back-pressure until the request
                // deadline, so the third outcome is unknown; the receipt query
                // below resolves it as not accepted.
                assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE, "{outcome}");
                assert_eq!(outcome["error"]["kind"], "AcceptanceUnknown");
                assert_eq!(outcome["receipts"].as_array().unwrap().len(), 2);
                assert_eq!(channel.progress().await?.accepted, 2);
                assert!(seen.lock().unwrap().is_empty());
                let (_, receipt) = post(&client, &base, "receipts", json!({"session":session,"sequence":3})).await?;
                assert!(receipt.is_null());
                let (code, outcome) = post(&client, &base, "producers/retire", json!({"session":session})).await?;
                assert_eq!(code, StatusCode::CONFLICT);
                assert_eq!(outcome["error"]["kind"], "PendingObligations");
                let (_, retry) = post(&client, &base, "events", json!({"session":session,"sequence":1,"event":http_event(1)})).await?;
                assert_eq!(retry["receipts"][0]["position"], 1);
                let (_, receipts) = post(&client, &base, "events/batch",
                    json!({"session":session,"first_sequence":1,"events":[http_event(1),http_event(2)]})).await?;
                return Ok((session, receipts["receipts"].clone()));
            }
            assert_eq!(session, saved_session);
            while channel.progress().await?.processed["consumer"] != 2 { tokio::task::yield_now().await; }
            let (_, retry) = post(&client, &base, "events/batch",
                json!({"session":session,"first_sequence":1,"events":[http_event(1),http_event(2)]})).await?;
            assert_eq!(retry["receipts"], saved_receipts);
            assert!(retry["error"].is_null(), "{retry}");
            let (code, conflict) = post(&client, &base, "events",
                json!({"session":session,"sequence":2,"event":http_event(20)})).await?;
            assert_eq!(code, StatusCode::CONFLICT);
            assert_eq!(conflict["error"]["kind"], "PayloadConflict");
            let (code, outcome) = post(&client, &base, "events", json!({"session":session,"sequence":3,"event":http_event(3)})).await?;
            assert_eq!(code, StatusCode::OK, "{outcome}");
            assert_eq!(outcome["receipts"][0]["position"], 3);
            while channel.progress().await?.processed["consumer"] != 3 { tokio::task::yield_now().await; }
            assert_eq!(*seen.lock().unwrap(), [1,2,3]);
            let (_, status) = post(&client, &base, "producers/status", json!({"session":session})).await?;
            assert_eq!(status["next_sequence"], 4);
            assert_eq!(status["earliest_receipt"], 2);
            let (code, expired) = post(&client, &base, "events", json!({"session":session,"sequence":1,"event":http_event(1)})).await?;
            assert_eq!(code, StatusCode::GONE);
            assert_eq!(expired["error"]["kind"], "ReceiptExpired");
            assert_eq!(channel.progress().await?.accepted, 3);
            let (_, retired) = post(&client, &base, "producers/retire", json!({"session":session})).await?;
            assert_eq!(retired["retired"], true);
            let (_, replacement) = post(&client, &base, "producers/register", json!({"producer":CLIENT_ID})).await?;
            assert_ne!(replacement["epoch"], session["epoch"]);
            Ok((session, saved_receipts.clone()))
        }).await?;
        saved_session = session;
        saved_receipts = receipts;
        drop(channel);
        harness.shutdown().await?;
    }
    Ok(())
}

type Client = proto::admission_service_client::AdmissionServiceClient<tonic::transport::Channel>;
async fn register(client: &mut Client) -> Result<proto::ProducerSession> {
    loop {
        match client
            .register_producer(proto::RegisterRequest {
                source_id: SOURCE_ID.into(),
                producer: CLIENT_ID.into(),
            })
            .await?
            .into_inner()
            .result
            .context("registration result")?
        {
            proto::register_response::Result::Session(session) => return Ok(session),
            proto::register_response::Result::Error(error)
                if [
                    proto::ErrorKind::Busy as i32,
                    proto::ErrorKind::Closed as i32,
                ]
                .contains(&error.kind) =>
            {
                tokio::task::yield_now().await
            }
            result => anyhow::bail!("unexpected registration: {result:?}"),
        }
    }
}
fn request(session: &proto::ProducerSession, sequence: u64, value: i64) -> proto::AdmitRequest {
    proto::AdmitRequest {
        source_id: SOURCE_ID.into(),
        session: Some(session.clone()),
        sequence,
        event: Some(durable_event(value)),
    }
}
async fn admit(
    client: &mut Client,
    request: proto::AdmitRequest,
) -> Result<proto::admit_response::Result> {
    loop {
        let result = client
            .admit(request.clone())
            .await?
            .into_inner()
            .result
            .context("admission result")?;
        match &result {
            proto::admit_response::Result::Error(error)
                if error.kind == proto::ErrorKind::Busy as i32 =>
            {
                tokio::task::yield_now().await
            }
            _ => return Ok(result),
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn grpc_durable_stream_reconstruction_receipts_and_retirement() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut saved = None;
    for reopened in [false, true] {
        let port = free_port()?;
        let mut harness = Harness::new(directory.path(), port, true).await?;
        let channel = harness.channel.clone();
        let seen = harness.seen.clone();
        let session = harness.drive(reopened, |_| async {
            let address = format!("http://127.0.0.1:{port}");
            let mut client = Client::connect(address.clone()).await?;
            let session = register(&mut client).await?;
            assert_eq!(register(&mut client).await?, session);
            if !reopened {
                let input = tokio_stream::iter((1..=3).map(|sequence| request(&session, sequence, sequence as i64)).collect::<Vec<_>>());
                let mut output = client.stream_events(input).await?.into_inner();
                for sequence in 1..=2 {
                    let result = output.message().await?.context("stream receipt")?.result.context("result")?;
                    let proto::admit_response::Result::Receipt(receipt) = result else { anyhow::bail!("{result:?}"); };
                    assert_eq!((receipt.sequence, receipt.position), (sequence, sequence));
                    assert_eq!(receipt.session.as_ref(), Some(&session));
                }
                // The full journal applies back-pressure until the admission
                // deadline; the third outcome is then unknown, not committed.
                let result = output.message().await?.context("full-channel response")?.result.context("result")?;
                assert!(matches!(&result, proto::admit_response::Result::Error(error) if error.kind == proto::ErrorKind::AcceptanceUnknown as i32), "{result:?}");
                assert!(output.message().await?.is_none(), "error terminates stream without admitting later input");
                assert!(seen.lock().unwrap().is_empty());
                assert_eq!(channel.progress().await?.accepted, 2);
                let result = client.retire_producer(proto::SessionRequest { source_id:SOURCE_ID.into(), session:Some(session.clone()) }).await?.into_inner().result;
                assert!(matches!(result, Some(proto::retire_response::Result::Error(error)) if error.kind == proto::ErrorKind::PendingObligations as i32));
                return Ok(session);
            }
            assert_eq!(Some(&session), saved.as_ref());
            while channel.progress().await?.processed["consumer"] != 2 { tokio::task::yield_now().await; }
            assert!(matches!(admit(&mut client, request(&session, 2, 20)).await?,
                proto::admit_response::Result::Error(error) if error.kind == proto::ErrorKind::PayloadConflict as i32));
            let original = admit(&mut client, request(&session, 2, 2)).await?;
            let lookup = client.admission_receipt(proto::ReceiptRequest { source_id:SOURCE_ID.into(), session:Some(session.clone()), sequence:2 }).await?.into_inner().result;
            let proto::admit_response::Result::Receipt(original) = original else { anyhow::bail!("{original:?}"); };
            assert_eq!(lookup, Some(proto::receipt_response::Result::Receipt(original)));
            assert!(matches!(admit(&mut client, request(&session, 3, 3)).await?, proto::admit_response::Result::Receipt(value) if value.position == 3));
            while channel.progress().await?.processed["consumer"] != 3 { tokio::task::yield_now().await; }
            assert_eq!(*seen.lock().unwrap(), [1,2,3]);
            let status = client.producer_status(proto::SessionRequest { source_id:SOURCE_ID.into(), session:Some(session.clone()) }).await?.into_inner().result;
            assert!(matches!(status, Some(proto::status_response::Result::Status(value)) if value.next_sequence == Some(4) && value.earliest_receipt == Some(2)));
            let expired = client.admission_receipt(proto::ReceiptRequest { source_id:SOURCE_ID.into(), session:Some(session.clone()), sequence:1 }).await?.into_inner().result;
            assert!(matches!(expired, Some(proto::receipt_response::Result::Error(error)) if error.kind == proto::ErrorKind::ReceiptExpired as i32));
            let retired = client.retire_producer(proto::SessionRequest { source_id:SOURCE_ID.into(), session:Some(session.clone()) }).await?.into_inner().result;
            assert_eq!(retired, Some(proto::retire_response::Result::Retired(true)));
            assert_ne!(register(&mut client).await?.epoch, session.epoch);
            Ok(session)
        }).await?;
        saved = Some(session);
        drop(channel);
        harness.shutdown().await?;
    }
    Ok(())
}
