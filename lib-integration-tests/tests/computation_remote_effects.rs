// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result};
use axum::{extract::State, http::StatusCode, routing::post, Json, Router};
use drasi_index_rocksdb::RocksDbIndexProvider;
use drasi_lib::{
    channels::{ComponentStatus, ResultDiff},
    reactions::ReactionCheckpoint,
    sources::{CapacityPolicy, DurabilityConfig},
    DrasiLib, Query, ReactionRecoveryPolicy, StateStoreProvider, StorageBackendRef,
};
use drasi_reaction_grpc::{
    proto::drasi_v1::{
        reaction_service_server::{ReactionService, ReactionServiceServer},
        ProcessResultsResponse,
    },
    GrpcReaction, ProcessResultsRequest,
};
use drasi_reaction_http::HttpReaction;
use drasi_source_application::{
    ApplicationSource, ApplicationSourceConfig, ApplicationSourceHandle, PropertyMapBuilder,
};
use drasi_state_store_redb::RedbStateStoreProvider;
use drasi_wal_redb::RedbWalProvider;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{oneshot, watch};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::{Request, Response, Status};

const QUERY: &str = "members";
const REACTION: &str = "deliver";
const SOURCE: &str = "source";
const DEADLINE: Duration = Duration::from_secs(15);

#[derive(Clone, Copy, Debug)]
enum Transport {
    Http,
    HttpBatch,
    Grpc,
    GrpcAdaptive,
}

impl Transport {
    fn name(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::HttpBatch => "http-batch",
            Self::Grpc => "grpc",
            Self::GrpcAdaptive => "grpc-adaptive",
        }
    }
    fn parse(value: &str) -> Result<Self> {
        match value {
            "http" => Ok(Self::Http),
            "http-batch" => Ok(Self::HttpBatch),
            "grpc" => Ok(Self::Grpc),
            "grpc-adaptive" => Ok(Self::GrpcAdaptive),
            _ => anyhow::bail!("unknown transport {value}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fault {
    None,
    PartialFailure,
    LostReply,
    ZeroSuccess,
    ShortSuccess,
    OverstatedSuccess,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Effect {
    query: String,
    sequence: u64,
    row: Value,
    signature: Option<u64>,
}

struct Recorder {
    path: PathBuf,
    writer: Mutex<File>,
    fault: watch::Sender<Fault>,
    requests: watch::Sender<Vec<Vec<Effect>>>,
}

impl Recorder {
    fn new(path: &Path, fault: Fault) -> Result<Arc<Self>> {
        Ok(Arc::new(Self {
            path: path.to_owned(),
            writer: Mutex::new(OpenOptions::new().create(true).append(true).open(path)?),
            fault: watch::channel(fault).0,
            requests: watch::channel(Vec::new()).0,
        }))
    }

    fn effects(&self) -> Result<Vec<Effect>> {
        let _writer = self.writer.lock().expect("effect journal");
        BufReader::new(File::open(&self.path)?)
            .lines()
            .map(|line| Ok(serde_json::from_str(&line?)?))
            .collect()
    }

    async fn accept(&self, items: Vec<Effect>) -> Result<(bool, usize)> {
        anyhow::ensure!(!items.is_empty(), "empty receiver batch");
        let fault = *self.fault.borrow();
        let prior_requests = self.requests.borrow().len();
        let completed = match fault {
            Fault::ZeroSuccess => 0,
            Fault::PartialFailure | Fault::ShortSuccess if items.len() > 1 => 1,
            Fault::PartialFailure if prior_requests > 0 => 0,
            _ => items.len(),
        };
        {
            let mut writer = self.writer.lock().expect("effect journal");
            for item in items.iter().take(completed) {
                serde_json::to_writer(&mut *writer, item)?;
                writeln!(writer)?;
            }
            writer.sync_all()?;
        }
        self.requests
            .send_modify(|requests| requests.push(items.clone()));
        if fault == Fault::LostReply {
            // The independently persisted effect is visible before the caller
            // receives any acknowledgement or can checkpoint the query result.
            self.fault
                .subscribe()
                .wait_for(|fault| *fault == Fault::None)
                .await?;
        }
        Ok((
            fault != Fault::PartialFailure || completed == items.len(),
            if fault == Fault::OverstatedSuccess {
                items.len() + 1
            } else {
                completed
            },
        ))
    }
}

struct RecordingService(Arc<Recorder>);

#[tonic::async_trait]
impl ReactionService for RecordingService {
    async fn process_results(
        &self,
        request: Request<ProcessResultsRequest>,
    ) -> std::result::Result<Response<ProcessResultsResponse>, Status> {
        let result: Result<_> = async {
            let batch = request
                .into_inner()
                .results
                .context("missing query batch")?;
            let mut effects = Vec::new();
            for item in batch.results {
                anyhow::ensure!(item.item_type == 1 && item.before.is_none(), "expected ADD");
                let row = item.after.context("missing after image")?;
                anyhow::ensure!(row.fields.len() == 1, "unexpected result columns");
                let Some(prost_types::value::Kind::StringValue(name)) =
                    row.fields.get("name").and_then(|value| value.kind.as_ref())
                else {
                    anyhow::bail!("missing string name");
                };
                effects.push(Effect {
                    query: batch.query_id.clone(),
                    sequence: item.sequence,
                    row: json!({"name": name}),
                    signature: Some(item.row_signature),
                });
            }
            self.0.accept(effects).await
        }
        .await;
        let (success, completed) =
            result.map_err(|error| Status::internal(format!("{error:#}")))?;
        Ok(Response::new(ProcessResultsResponse {
            success,
            message: "recorded".into(),
            error: if success {
                String::new()
            } else {
                "injected failure after a successful prefix".into()
            },
            items_processed: u32::try_from(completed)
                .map_err(|_| Status::internal("invalid completed count"))?,
        }))
    }
}

async fn http_receive(
    State(recorder): State<Arc<Recorder>>,
    Json(body): Json<Value>,
) -> std::result::Result<StatusCode, (StatusCode, String)> {
    let result: Result<_> = async {
        let rows = match body.get("batch") {
            Some(batch) => batch.as_array().context("batch must be an array")?.clone(),
            None => vec![body.clone()],
        };
        let items = rows
            .into_iter()
            .map(|row| {
                anyhow::ensure!(
                    row["operation"] == "ADD" && row.get("before").is_none(),
                    "expected ADD"
                );
                Ok(Effect {
                    query: row["queryId"].as_str().context("query identity")?.into(),
                    sequence: row["sequenceId"].as_u64().context("query sequence")?,
                    row: row.get("after").context("after image")?.clone(),
                    signature: None,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        recorder.accept(items).await
    }
    .await;
    let (success, _) =
        result.map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, format!("{error:#}")))?;
    Ok(if success {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    })
}

struct RecordingServer {
    endpoint: String,
    recorder: Arc<Recorder>,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<Result<()>>>,
}

impl RecordingServer {
    async fn start(path: &Path, transport: Transport, fault: Fault) -> Result<Self> {
        let recorder = Recorder::new(path, fault)?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let (shutdown, receive) = oneshot::channel();
        let service = recorder.clone();
        let grpc = matches!(transport, Transport::Grpc | Transport::GrpcAdaptive);
        let task = tokio::spawn(async move {
            if grpc {
                tonic::transport::Server::builder()
                    .add_service(ReactionServiceServer::new(RecordingService(service)))
                    .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async {
                        let _ = receive.await;
                    })
                    .await?;
            } else {
                let routes = Router::new()
                    .route("/changes/{query}", post(http_receive))
                    .route("/batch", post(http_receive))
                    .with_state(service);
                axum::serve(listener, routes)
                    .with_graceful_shutdown(async {
                        let _ = receive.await;
                    })
                    .await?;
            }
            Ok(())
        });
        // A real accepted TCP connection verifies the owned server is listening.
        let probe = tokio::net::TcpStream::connect(address).await?;
        drop(probe);
        Ok(Self {
            endpoint: format!("{}://{address}", if grpc { "grpc" } else { "http" }),
            recorder,
            shutdown: Some(shutdown),
            task: Some(task),
        })
    }

    async fn close(mut self) -> Result<()> {
        self.recorder.fault.send_replace(Fault::None);
        if let Some(shutdown) = self.shutdown.take() {
            shutdown
                .send(())
                .map_err(|_| anyhow::anyhow!("receiver stopped unexpectedly"))?;
        }
        tokio::time::timeout(DEADLINE, self.task.take().context("receiver task")?).await???;
        Ok(())
    }
}

impl Drop for RecordingServer {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

struct Pipeline {
    core: DrasiLib,
    source: ApplicationSourceHandle,
    checkpoints: Arc<RedbStateStoreProvider>,
}

async fn pipeline(root: &Path, endpoint: &str, transport: Transport) -> Result<Pipeline> {
    configured_pipeline(root, endpoint, transport, false).await
}

async fn configured_pipeline(
    root: &Path,
    endpoint: &str,
    transport: Transport,
    direct: bool,
) -> Result<Pipeline> {
    let (source, handle) = ApplicationSource::new(
        SOURCE,
        ApplicationSourceConfig {
            properties: HashMap::new(),
            durability: Some(DurabilityConfig {
                enabled: true,
                max_events: 128,
                capacity_policy: CapacityPolicy::RejectIncoming,
            }),
        },
    )?;
    let checkpoints = Arc::new(RedbStateStoreProvider::new(root.join("checkpoints.redb"))?);
    let query = Query::cypher(QUERY)
        .query("MATCH (g:Group)-[:HAS]->(p:Person) WHERE g.active = true RETURN p.name AS name")
        .from_source(SOURCE)
        .enable_bootstrap(false)
        .with_outbox_capacity(16)
        .with_storage_backend(StorageBackendRef::Named("rocks".into()))
        .build();
    let builder = DrasiLib::builder()
        .with_id("remote-effects")
        .with_index_provider(
            "rocks",
            Arc::new(RocksDbIndexProvider::new(
                root.join("indexes"),
                false,
                false,
            )),
        )
        .with_wal_provider(Arc::new(RedbWalProvider::new(root.join("wal"))))
        .with_state_store_provider(checkpoints.clone());
    let adaptive = drasi_lib::reactions::common::AdaptiveBatchConfig {
        adaptive_min_batch_size: 3,
        adaptive_max_batch_size: 3,
        adaptive_window_size: 10,
        adaptive_batch_timeout_ms: 100,
    };
    let core = if direct {
        let core = builder.build().await?;
        core.start().await?;
        core.add_source(source).await?;
        tokio::time::timeout(DEADLINE, core.computation_component(SOURCE)?.wait_started())
            .await??;
        core.add_query(query).await?;
        core
    } else {
        builder
            .with_source(source)
            .with_query(query)
            .build()
            .await?
    };
    match transport {
        Transport::Http | Transport::HttpBatch => {
            let mut reaction = HttpReaction::builder(REACTION)
                .from_query(QUERY)
                .with_base_url(endpoint)
                .with_timeout_ms(1000)
                .with_recovery_policy(ReactionRecoveryPolicy::Strict);
            if matches!(transport, Transport::HttpBatch) {
                reaction = reaction
                    .with_adaptive(adaptive)
                    .with_batch_endpoint("/batch");
            }
            core.add_reaction(reaction.build()?).await?;
        }
        Transport::Grpc | Transport::GrpcAdaptive => {
            let mut reaction = GrpcReaction::builder(REACTION)
                .from_query(QUERY)
                .with_endpoint(endpoint)
                .with_timeout_ms(1000)
                .with_max_retries(0)
                .with_connection_retry_attempts(1)
                .with_fixed_batching(3, 100)
                .with_recovery_policy(ReactionRecoveryPolicy::Strict);
            if matches!(transport, Transport::GrpcAdaptive) {
                reaction = reaction.with_adaptive_batching(adaptive);
            }
            core.add_reaction(reaction.build()?).await?;
        }
    }
    if !direct {
        core.start().await?;
    }
    tokio::time::timeout(DEADLINE, core.computation_component(QUERY)?.wait_started()).await??;
    tokio::time::timeout(
        DEADLINE,
        core.computation_component(REACTION)?.wait_started(),
    )
    .await??;
    Ok(Pipeline {
        core,
        source: handle,
        checkpoints,
    })
}

async fn seed(source: &ApplicationSourceHandle) -> Result<()> {
    source
        .send_node_insert(
            "group",
            vec!["Group"],
            PropertyMapBuilder::new().with_bool("active", false).build(),
        )
        .await?;
    for name in ["alpha", "beta", "gamma"] {
        source
            .send_node_insert(
                name,
                vec!["Person"],
                PropertyMapBuilder::new().with_string("name", name).build(),
            )
            .await?;
        source
            .send_relation_insert(
                format!("member-{name}"),
                vec!["HAS"],
                PropertyMapBuilder::new().build(),
                "group",
                name,
            )
            .await?;
    }
    source
        .send_node_update(
            "group",
            vec!["Group"],
            PropertyMapBuilder::new().with_bool("active", true).build(),
        )
        .await?;
    Ok(())
}

async fn checkpoint(pipeline: &Pipeline) -> Result<Option<ReactionCheckpoint>> {
    pipeline
        .checkpoints
        .get(REACTION, &format!("checkpoint:{QUERY}"))
        .await?
        .map(|bytes| bincode::deserialize(&bytes).map_err(Into::into))
        .transpose()
}

async fn wait_failed(pipeline: &Pipeline) -> Result<()> {
    tokio::time::timeout(DEADLINE, async {
        loop {
            let progress = checkpoint(pipeline)
                .await?
                .map_or(0, |saved| saved.sequence);
            anyhow::ensure!(
                progress == 0,
                "checkpoint skipped unfinished remote effects: {progress}"
            );
            if pipeline.core.get_reaction_status(REACTION).await? == ComponentStatus::Error {
                break;
            }
            tokio::task::yield_now().await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

async fn wait_complete(pipeline: &Pipeline) -> Result<()> {
    wait_sequence(pipeline, 1).await
}

async fn wait_sequence(pipeline: &Pipeline, expected: u64) -> Result<()> {
    tokio::time::timeout(DEADLINE, async {
        loop {
            if checkpoint(pipeline)
                .await?
                .is_some_and(|saved| saved.sequence == expected)
            {
                break;
            }
            anyhow::ensure!(
                pipeline.core.get_reaction_status(REACTION).await? != ComponentStatus::Error,
                "reaction failed during recovery"
            );
            tokio::task::yield_now().await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

async fn expected_effects(core: &DrasiLib, transport: Transport) -> Result<Vec<Effect>> {
    let query = core
        .query_manager()
        .get_query_instance(QUERY)
        .await
        .map_err(anyhow::Error::msg)?;
    let outbox = query.fetch_outbox(0).await?;
    anyhow::ensure!(
        outbox.latest_sequence == 1 && outbox.results.len() == 1,
        "expected one multi-row query emission: {outbox:?}"
    );
    let result = &outbox.results[0];
    anyhow::ensure!(
        result.results.len() == 3,
        "expected three diffs in one query result"
    );
    let mut expected = Vec::new();
    for diff in &result.results {
        let ResultDiff::Add {
            data,
            row_signature,
        } = diff
        else {
            anyhow::bail!("expected ADD: {diff:?}");
        };
        expected.push(Effect {
            query: QUERY.into(),
            sequence: 1,
            row: data.clone(),
            signature: matches!(transport, Transport::Grpc | Transport::GrpcAdaptive)
                .then_some(*row_signature),
        });
    }
    let mut names = expected
        .iter()
        .map(|effect| effect.row["name"].as_str().context("expected name"))
        .collect::<Result<Vec<_>>>()?;
    names.sort();
    anyhow::ensure!(
        names == ["alpha", "beta", "gamma"],
        "wrong query result: {names:?}"
    );
    Ok(expected)
}

async fn partial_failure(transport: Transport, fault: Fault) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let server =
        RecordingServer::start(&directory.path().join("receiver.jsonl"), transport, fault).await?;
    let first = pipeline(directory.path(), &server.endpoint, transport).await?;
    seed(&first.source).await?;
    wait_failed(&first).await?;
    let expected = expected_effects(&first.core, transport).await?;
    let prefix = server.recorder.effects()?;
    anyhow::ensure!(
        fault == Fault::ZeroSuccess || !prefix.is_empty(),
        "the receiver must complete a real effect before failure"
    );
    if fault != Fault::OverstatedSuccess {
        anyhow::ensure!(
            prefix.iter().all(|effect| effect == &expected[0]),
            "only the successful prefix may be recorded: {prefix:?}"
        );
    }
    assert_eq!(
        checkpoint(&first).await?.map_or(0, |saved| saved.sequence),
        0
    );
    first.core.shutdown().await?;
    drop(first);
    server.recorder.fault.send_replace(Fault::None);
    let recovered = pipeline(directory.path(), &server.endpoint, transport).await?;
    wait_complete(&recovered).await?;
    let actual = server.recorder.effects()?;
    assert_eq!(
        &actual[..prefix.len()],
        prefix.as_slice(),
        "durable receiver history changed"
    );
    assert_eq!(
        &actual[prefix.len()..],
        expected.as_slice(),
        "all unfinished result items must replay in their original order"
    );
    assert_eq!(
        expected_effects(&recovered.core, transport).await?,
        expected,
        "query output was recomputed or renumbered"
    );
    recovered.core.shutdown().await?;
    server.close().await?;
    Ok(())
}

#[tokio::test]
async fn partial_http_and_grpc_effects_replay_without_skipping_the_unfinished_query_result(
) -> Result<()> {
    for transport in [
        Transport::Http,
        Transport::HttpBatch,
        Transport::Grpc,
        Transport::GrpcAdaptive,
    ] {
        partial_failure(transport, Fault::PartialFailure)
            .await
            .with_context(|| format!("{transport:?}"))?;
    }
    Ok(())
}

#[tokio::test]
async fn grpc_success_requires_confirmation_of_every_item_before_checkpointing() -> Result<()> {
    for transport in [Transport::Grpc, Transport::GrpcAdaptive] {
        for fault in [
            Fault::ZeroSuccess,
            Fault::ShortSuccess,
            Fault::OverstatedSuccess,
        ] {
            partial_failure(transport, fault)
                .await
                .with_context(|| format!("{transport:?}/{fault:?}"))?;
        }
    }
    Ok(())
}

#[tokio::test]
#[ignore = "remote-effect crash worker invoked by the parent test"]
async fn remote_effect_crash_worker() -> Result<()> {
    let directory = PathBuf::from(std::env::var("DRASI_REMOTE_EFFECT_ROOT")?);
    let endpoint = std::env::var("DRASI_REMOTE_EFFECT_ENDPOINT")?;
    let transport = Transport::parse(&std::env::var("DRASI_REMOTE_EFFECT_TRANSPORT")?)?;
    let pipeline = pipeline(&directory, &endpoint, transport).await?;
    seed(&pipeline.source).await?;
    std::future::pending::<Result<()>>().await
}

#[tokio::test]
async fn process_death_after_remote_effect_before_reply_replays_from_durable_query_output(
) -> Result<()> {
    for transport in [
        Transport::Http,
        Transport::HttpBatch,
        Transport::Grpc,
        Transport::GrpcAdaptive,
    ] {
        let directory = tempfile::tempdir()?;
        let server = RecordingServer::start(
            &directory.path().join("receiver.jsonl"),
            transport,
            Fault::LostReply,
        )
        .await?;
        let child_log = File::create(directory.path().join("worker.log"))?;
        let mut child = tokio::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "remote_effect_crash_worker",
                "--ignored",
                "--nocapture",
            ])
            .env("DRASI_REMOTE_EFFECT_ROOT", directory.path())
            .env("DRASI_REMOTE_EFFECT_ENDPOINT", &server.endpoint)
            .env("DRASI_REMOTE_EFFECT_TRANSPORT", transport.name())
            .stdout(child_log.try_clone()?)
            .stderr(child_log)
            .kill_on_drop(true)
            .spawn()?;
        let mut requests = server.recorder.requests.subscribe();
        tokio::time::timeout(DEADLINE, async {
            tokio::select! {
                status = child.wait() => anyhow::bail!("worker exited before the durable effect: {status:?}; {}", std::fs::read_to_string(directory.path().join("worker.log"))?),
                request = requests.wait_for(|requests| !requests.is_empty()) => { drop(request?); Ok(()) },
            }
        }).await??;
        child.start_kill()?;
        let status = tokio::time::timeout(DEADLINE, child.wait()).await??;
        assert!(!status.success());
        let prefix = server.recorder.effects()?;
        anyhow::ensure!(
            !prefix.is_empty(),
            "effect was not saved before process death"
        );
        server.recorder.fault.send_replace(Fault::None);
        let recovered = pipeline(directory.path(), &server.endpoint, transport).await?;
        let expected = expected_effects(&recovered.core, transport).await?;
        wait_complete(&recovered).await?;
        assert_eq!(
            &server.recorder.effects()?[prefix.len()..],
            expected.as_slice(),
            "{transport:?}: unfinished acknowledgement must replay the complete query result"
        );
        recovered.core.shutdown().await?;
        server.close().await?;
    }
    Ok(())
}

async fn append_member(pipeline: &Pipeline, name: &str) -> Result<()> {
    pipeline
        .source
        .send_node_insert(
            name,
            vec!["Person"],
            PropertyMapBuilder::new().with_string("name", name).build(),
        )
        .await?;
    pipeline
        .source
        .send_relation_insert(
            format!("member-{name}"),
            vec!["HAS"],
            PropertyMapBuilder::new().build(),
            "group",
            name,
        )
        .await?;
    Ok(())
}

#[tokio::test]
async fn direct_additions_and_each_stage_restart_preserve_real_effects_and_saved_positions(
) -> Result<()> {
    for transport in [
        Transport::Http,
        Transport::HttpBatch,
        Transport::Grpc,
        Transport::GrpcAdaptive,
    ] {
        tokio::time::timeout(Duration::from_secs(60), direct_lifecycle(transport)).await??
    }
    Ok(())
}

async fn direct_lifecycle(transport: Transport) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let server = RecordingServer::start(
        &directory.path().join("receiver.jsonl"),
        transport,
        Fault::None,
    )
    .await?;
    let first = configured_pipeline(directory.path(), &server.endpoint, transport, true).await?;
    seed(&first.source).await?;
    wait_complete(&first).await?;
    let original = expected_effects(&first.core, transport).await?;

    first.core.stop_source(SOURCE).await?;
    assert_eq!(
        first.core.get_source_status(SOURCE).await?,
        ComponentStatus::Stopped
    );
    append_member(&first, "while-source-stopped").await?;
    assert_eq!(
        checkpoint(&first)
            .await?
            .context("saved reaction checkpoint")?
            .sequence,
        1
    );
    first.core.start_source(SOURCE).await?;
    tokio::time::timeout(
        DEADLINE,
        first.core.computation_component(SOURCE)?.wait_started(),
    )
    .await??;
    wait_sequence(&first, 2).await?;

    first.core.stop_query(QUERY).await?;
    append_member(&first, "while-query-stopped").await?;
    assert_eq!(
        checkpoint(&first)
            .await?
            .context("saved reaction checkpoint")?
            .sequence,
        2
    );
    first.core.start_query(QUERY).await?;
    wait_sequence(&first, 3).await?;

    first.core.stop_reaction(REACTION).await?;
    append_member(&first, "while-reaction-stopped").await?;
    let query = first
        .core
        .query_manager()
        .get_query_instance(QUERY)
        .await
        .map_err(anyhow::Error::msg)?;
    tokio::time::timeout(DEADLINE, async {
        while query.fetch_outbox(0).await?.latest_sequence != 4 {
            tokio::task::yield_now().await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    assert_eq!(
        checkpoint(&first)
            .await?
            .context("saved reaction checkpoint")?
            .sequence,
        3
    );
    first.core.start_reaction(REACTION).await?;
    wait_sequence(&first, 4).await?;
    drop(query);

    first.core.stop().await?;
    append_member(&first, "while-instance-stopped").await?;
    first.core.start().await?;
    wait_sequence(&first, 5).await?;
    first.core.shutdown().await?;
    assert!(
        first.core.start().await.is_err(),
        "shutdown is terminal; restart requires reconstruction"
    );
    drop(first);

    let restored = configured_pipeline(directory.path(), &server.endpoint, transport, true).await?;
    wait_sequence(&restored, 5).await?;
    append_member(&restored, "after-reconstruction").await?;
    wait_sequence(&restored, 6).await?;
    restored.core.shutdown().await?;
    let effects = server.recorder.effects()?;
    assert_eq!(&effects[..3], original.as_slice());
    let expected = [
        "while-source-stopped",
        "while-query-stopped",
        "while-reaction-stopped",
        "while-instance-stopped",
        "after-reconstruction",
    ];
    assert_eq!(
        effects.len(),
        8,
        "restarts must not duplicate already checkpointed effects"
    );
    for (offset, name) in expected.iter().enumerate() {
        assert_eq!(effects[offset + 3].sequence, offset as u64 + 2);
        assert_eq!(effects[offset + 3].row, json!({"name": name}));
    }
    server.close().await?;
    Ok(())
}
