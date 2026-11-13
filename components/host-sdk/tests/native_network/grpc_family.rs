// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::http_family::family_component;
use super::*;
use futures_util::FutureExt;

const A_QUERY_ID: &str = "00000000-0000-0000-0000-000000000601";
const B_QUERY_ID: &str = "00000000-0000-0000-0000-000000000602";

#[derive(Default)]
struct GrpcState {
    requests: Mutex<
        Vec<(
            tonic::metadata::MetadataMap,
            reaction::ProcessChangesRequest,
        )>,
    >,
    failures: AtomicUsize,
    partial: AtomicUsize,
    gated: AtomicUsize,
    entered: Notify,
    release: Notify,
    cancel: CancellationToken,
}
struct Recorder(Arc<GrpcState>);
#[tonic::async_trait]
impl reaction::change_service_server::ChangeService for Recorder {
    async fn process_changes(
        &self,
        request: tonic::Request<reaction::ProcessChangesRequest>,
    ) -> std::result::Result<tonic::Response<reaction::ProcessResultsResponse>, tonic::Status> {
        let count = request
            .get_ref()
            .changes
            .as_ref()
            .map_or(0, |batch| batch.changes.len());
        self.0
            .requests
            .lock()
            .expect("gRPC capture lock")
            .push((request.metadata().clone(), request.into_inner()));
        self.0.entered.notify_one();
        if self.0.gated.load(Ordering::SeqCst) > 0 {
            tokio::select! {
                _ = self.0.release.notified() => {},
                _ = self.0.cancel.cancelled() => return Err(tonic::Status::cancelled("receiver stopped")),
            }
        }
        if self
            .0
            .failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |value| {
                value.checked_sub(1)
            })
            .is_ok()
        {
            return Err(tonic::Status::unavailable("temporary receiver failure"));
        }
        Ok(tonic::Response::new(reaction::ProcessResultsResponse {
            success: true,
            message: String::new(),
            error: String::new(),
            items_processed: if self.0.partial.load(Ordering::SeqCst) > 0 {
                count.saturating_sub(1) as u32
            } else {
                count as u32
            },
        }))
    }
}
struct Server {
    address: SocketAddr,
    state: Arc<GrpcState>,
    task: JoinHandle<Result<()>>,
}
impl Server {
    async fn stop(self) -> Result<()> {
        self.state.cancel.cancel();
        tokio::time::timeout(DEADLINE, self.task).await???;
        Ok(())
    }
}
async fn server() -> Result<Server> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let state = Arc::new(GrpcState::default());
    let service =
        reaction::change_service_server::ChangeServiceServer::new(Recorder(state.clone()));
    let cancel = state.cancel.clone();
    let task = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(service)
            .serve_with_incoming_shutdown(
                tokio_stream::wrappers::TcpListenerStream::new(listener),
                cancel.cancelled_owned(),
            )
            .await?;
        Ok(())
    });
    Ok(Server {
        address,
        state,
        task,
    })
}

fn input(
    query: &str,
    component: &ComponentId,
    sequence: u64,
    routes: &[&str],
) -> Result<InputEnvelope> {
    let results: Vec<_> = routes
        .iter()
        .enumerate()
        .map(|(index, route)| QueryPartEvaluationContext::Adding {
            after: [
                (
                    "value".into(),
                    VariableValue::Integer((index as i64 + 1).into()),
                ),
                ("route".into(), VariableValue::String((*route).into())),
            ]
            .into(),
            row_signature: index as u64 + 10,
        })
        .collect();
    Ok(InputEnvelope {
        port: PortId::try_new("in")?,
        envelope: QueryChangeCodec::encode_evaluation(
            None,
            component,
            SystemMetadata::new(stream(&format!("{query}/out")), sequence),
            &ResultNodeProjector::try_new(&format!("test/native-grpc-family/{query}/out"), None)?,
            &results,
            QueryOutputMetadata {
                query_id: component.clone(),
                input_origin: None,
                timestamp: chrono::Utc::now(),
                metadata: HashMap::from([("tenant".into(), json!("test"))]),
                profiling: None,
            },
        )?
        .context("query output")?,
    })
}

#[tokio::test(flavor = "current_thread")]
async fn native_grpc_stream_totals_preserve_the_consumer_contract_and_failed_prefix() -> Result<()>
{
    for linked in [true, false] {
        let port = free_port()?;
        let mut source = family_component(
            GRPC_SOURCE,
            NETWORK_SOURCE_ID,
            json!({
                "stream":"facilities-db/out","host":"127.0.0.1","port":port,"ingressCapacity":4,
            }),
            linked,
        )?;
        source.initialize().await?;
        source.start().await?;
        let mut client = source::source_service_client::SourceServiceClient::connect(format!(
            "http://127.0.0.1:{port}"
        ))
        .await?;
        let mut invalid = proto_event(3);
        invalid.source_id = "00000000-0000-0000-0000-000000000607".into();
        let mut replies = client
            .stream_events(tokio_stream::iter([
                proto_event(1),
                proto_event(2),
                invalid,
            ]))
            .await?
            .into_inner();
        for count in 1..=2 {
            let response = replies.message().await?.context("progress response")?;
            assert!(response.success);
            assert_eq!(response.events_processed, count);
        }
        let failed = replies.message().await?.context("failed response")?;
        assert!(!failed.success);
        assert_eq!(failed.events_processed, 2);
        assert!(!failed.error.is_empty());
        assert!(replies.message().await?.is_none());
        for value in 1..=2 {
            assert_eq!(source_value(&next(&mut source).await?)?, value);
        }
        let mut replies = client
            .stream_events(tokio_stream::iter((3..=5).map(proto_event)))
            .await?
            .into_inner();
        for count in 1..=3 {
            let response = replies.message().await?.context("progress response")?;
            assert!(response.success);
            assert_eq!(
                response.events_processed, count,
                "totals reset for each stream"
            );
        }
        assert!(replies.message().await?.is_none());
        for value in 3..=5 {
            assert_eq!(source_value(&next(&mut source).await?)?, value);
        }
        source.stop().await?;
        source.dispose().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_grpc_batches_preserve_templates_metadata_boundaries_and_typed_rows() -> Result<()> {
    let receiver = server().await?;
    for linked in [true, false] {
        let mut sink = family_component(
            GRPC_SINK,
            NETWORK_SINK_ID,
            json!({
                "outputs":{"a":{"component":A_QUERY_ID,"port":"out"},"b":{"component":B_QUERY_ID,"port":"out"}},"endpoint":format!("grpc://{}",receiver.address),
                "batching":{"mode":"fixed","batchSize":3,"batchFlushTimeoutMs":30},
                "outputTemplates":{"defaultTemplate":{"added":{
                    "template":"{\"alias\":\"{{output.alias}}\",\"port\":\"{{output.port}}\",\"sequence\":{{sequence}},\"key\":\"{{identity.id}}\"}",
                    "metadata":{"x-route":"{{after.route}}"}
                }}}
            }),
            linked,
        )?;
        sink.initialize().await?;
        sink.start().await?;
        let before = receiver.state.requests.lock().unwrap().len();
        sink.handle_batch(vec![
            input(
                "a",
                &ComponentId::try_new(A_QUERY_ID)?,
                1,
                &["same", "same"],
            )?,
            input("b", &ComponentId::try_new(B_QUERY_ID)?, 1, &["same"])?,
        ])
        .await?;
        sink.handle(input(
            "a",
            &ComponentId::try_new(A_QUERY_ID)?,
            2,
            &["left", "right"],
        )?)
        .await?;
        sink.stop().await?;
        sink.dispose().await?;
        let requests = receiver.state.requests.lock().unwrap();
        let calls = &requests[before..];
        assert_eq!(
            calls.len(),
            4,
            "query or rendered metadata changes must split requests"
        );
        assert_eq!(
            calls
                .iter()
                .map(|(_, request)| request.changes.as_ref().unwrap().changes.len())
                .collect::<Vec<_>>(),
            [2, 1, 1, 1]
        );
        for (headers, request) in calls {
            let batch = request.changes.as_ref().unwrap();
            let output = batch.output.as_ref().context("output identity")?;
            assert_eq!(
                headers
                    .get("x-drasi-component")
                    .context("component header")?
                    .to_str()?,
                output.component
            );
            assert_eq!(
                headers
                    .get("x-drasi-port")
                    .context("port header")?
                    .to_str()?,
                output.port
            );
            assert_eq!(output.port, "out");
            assert_eq!(
                output.component,
                match output.alias.as_str() {
                    "a" => A_QUERY_ID,
                    "b" => B_QUERY_ID,
                    other => anyhow::bail!("unexpected output alias {other}"),
                }
            );
            assert_eq!(
                headers.get("x-route").context("route header")?.to_str()?,
                request.metadata["x-route"].as_str()
            );
            for item in &batch.changes {
                assert!(
                    item.after.as_ref().unwrap().fields.contains_key("value"),
                    "templating must not rewrite the typed row"
                );
                let payload = item.payload.as_ref().unwrap();
                assert!(payload.fields.contains_key("key"));
                assert_eq!(item.operation, reaction::ChangeOperation::Add as i32);
                assert!(item.identity.as_ref().unwrap().id.len() == 16);
            }
        }
    }
    receiver.stop().await
}

#[tokio::test(flavor = "current_thread")]
async fn native_grpc_retries_the_same_batch_and_requires_exact_acknowledgement() -> Result<()> {
    let receiver = server().await?;
    for linked in [true, false] {
        let mut sink = family_component(
            GRPC_SINK,
            NETWORK_SINK_ID,
            json!({
                "outputs":{"q":{"component":RESULT_QUERY_ID,"port":"out"}},"endpoint":format!("grpc://{}",receiver.address),
                "batchSize":3,"batchFlushTimeoutMs":25,"maxRetries":1,
            }),
            linked,
        )?;
        sink.initialize().await?;
        sink.start().await?;
        receiver.state.failures.store(1, Ordering::SeqCst);
        let start = receiver.state.requests.lock().unwrap().len();
        sink.handle_batch(vec![result_input(1)?, result_input(2)?, result_input(3)?])
            .await?;
        {
            let requests = receiver.state.requests.lock().unwrap();
            assert_eq!(requests.len() - start, 2);
            assert_eq!(
                requests[start].1,
                requests[start + 1].1,
                "retry identity/body must be unchanged"
            );
            assert_eq!(requests[start].1.changes.as_ref().unwrap().changes.len(), 3);
        }
        receiver.state.partial.store(1, Ordering::SeqCst);
        let failure = sink
            .handle_batch(vec![result_input(4)?, result_input(5)?])
            .await
            .unwrap_err();
        assert!(format!("{failure:#}").contains("incomplete"));
        receiver.state.partial.store(0, Ordering::SeqCst);
        receiver.state.gated.store(1, Ordering::SeqCst);
        {
            let starting = receiver.state.requests.lock().unwrap().len();
            let call = sink.handle_batch(vec![result_input(6)?, result_input(7)?]);
            tokio::pin!(call);
            loop {
                tokio::select! {
                    result = &mut call => panic!("batch completed before remote acknowledgement: {result:?}"),
                    _ = receiver.state.entered.notified() => {},
                }
                if receiver.state.requests.lock().unwrap().len() > starting {
                    break;
                }
            }
            assert!(call.as_mut().now_or_never().is_none());
            receiver.state.gated.store(0, Ordering::SeqCst);
            receiver.state.release.notify_one();
            call.await?;
        }
        sink.stop().await?;
        sink.dispose().await?;
    }
    receiver.stop().await
}

#[tokio::test(flavor = "current_thread")]
async fn native_grpc_failure_policy_defines_graph_retry_ownership() -> Result<()> {
    let receiver = server().await?;
    for linked in [true, false] {
        let configuration = |failure_policy: &str| {
            json!({
                "outputs":{"q":{"component":RESULT_QUERY_ID,"port":"out"}},
                "endpoint":format!("grpc://{}",receiver.address),
                "batchSize":1,
                "batchFlushTimeoutMs":25,
                "maxRetries":0,
                "failurePolicy":failure_policy,
            })
        };

        let mut strict =
            family_component(GRPC_SINK, NETWORK_SINK_ID, configuration("strict"), linked)?;
        strict.initialize().await?;
        strict.start().await?;
        receiver.state.failures.store(1, Ordering::SeqCst);
        let start = receiver.state.requests.lock().unwrap().len();
        assert!(strict.handle(result_input(1)?).await.is_err());
        strict.handle(result_input(1)?).await?;
        {
            let requests = receiver.state.requests.lock().unwrap();
            assert_eq!(requests.len() - start, 2);
            assert_eq!(
                requests[start].1,
                requests[start + 1].1,
                "the graph-owned retry must preserve the exact failed envelope"
            );
        }
        strict.stop().await?;
        strict.dispose().await?;

        let mut skip = family_component(
            GRPC_SINK,
            "00000000-0000-0000-0000-000000000609",
            configuration("skip"),
            linked,
        )?;
        skip.initialize().await?;
        skip.start().await?;
        receiver.state.failures.store(1, Ordering::SeqCst);
        let start = receiver.state.requests.lock().unwrap().len();
        skip.handle(result_input(2)?).await?;
        assert_eq!(
            receiver.state.requests.lock().unwrap().len() - start,
            1,
            "skip deliberately acknowledges the failed envelope without retry"
        );
        skip.stop().await?;
        skip.dispose().await?;
    }
    receiver.stop().await
}

#[tokio::test(flavor = "current_thread")]
async fn native_grpc_adaptive_and_request_byte_limits_split_without_losing_items() -> Result<()> {
    let receiver = server().await?;
    for linked in [true, false] {
        let mut sink = family_component(
            GRPC_SINK,
            NETWORK_SINK_ID,
            json!({
                "outputs":{"q":{"component":RESULT_QUERY_ID,"port":"out"}},"endpoint":format!("grpc://{}",receiver.address),
                "batching":{"mode":"adaptive","adaptiveMinBatchSize":10,"adaptiveMaxBatchSize":10,
                    "adaptiveWindowSize":10,"adaptiveBatchTimeoutMs":40},
                "maxRequestBytes":1800,"maxRetries":0,
            }),
            linked,
        )?;
        sink.initialize().await?;
        sink.start().await?;
        assert_eq!(sink.batching()?.unwrap().max_operations, 10);
        let before = receiver.state.requests.lock().unwrap().len();
        sink.handle(input(
            "q",
            &ComponentId::try_new(RESULT_QUERY_ID)?,
            1,
            &["route"; 10],
        )?)
        .await?;
        sink.stop().await?;
        sink.dispose().await?;
        let requests = receiver.state.requests.lock().unwrap();
        let calls = &requests[before..];
        assert!(
            calls.len() > 1,
            "encoded-byte limit must split an otherwise compatible batch"
        );
        let elements: Vec<_> = calls
            .iter()
            .flat_map(|(_, request)| {
                request
                    .changes
                    .as_ref()
                    .unwrap()
                    .changes
                    .iter()
                    .map(|item| item.identity.as_ref().unwrap().id.clone())
            })
            .collect();
        assert_eq!(
            elements,
            (10..20)
                .map(|row| format!("{row:016x}"))
                .collect::<Vec<_>>()
        );
    }
    receiver.stop().await
}

#[tokio::test(flavor = "current_thread")]
async fn native_grpc_source_query_and_batched_sink_execute_in_one_drasi_instance() -> Result<()> {
    for linked in [true, false] {
        let receiver = server().await?;
        let core = drasi_lib::DrasiLib::builder()
            .with_query_factory(
                drasi_lib::computation::v1::FactoryRegistry::standard()
                    .get(
                        &drasi_lib::computation::v1::ImplementationIdentity::try_new(
                            "drasi/continuous-query",
                            "1",
                        )
                        .expect("reference query identity"),
                    )
                    .expect("reference query factory")
                    .clone(),
            )
            .with_id("grpc-family")
            .with_allow_external_component_ids(true)
            .build()
            .await?;
        let port = free_port()?;
        let result = async {
            let source = family_component(
                GRPC_SOURCE,
                NETWORK_SOURCE_ID,
                json!({
                    "stream":"facilities-db/out","host":"127.0.0.1","port":port,
                }),
                linked,
            )?;
            let source_id = source.descriptor().id().clone();
            let query_id = ComponentId::try_new(RESULT_QUERY_ID)?;
            let output = Endpoint::new(
                source_id.clone(),
                PortId::try_new("out")?,
            );
            core.add_components(
                ComponentBatch::builder()
                    .source(drasi_lib::computation::v1::ComponentName::try_new("facilities-db").expect("fixture component name"), Box::new(source))
                    .bind_stream(output.clone(), stream("facilities-db/out"))
                    .build()?,
            )
            .await?;
            let pipeline = core.computation_pipeline()?.native_source(output)?.query(
                drasi_lib::Query::cypher(drasi_core_types::ComponentName::try_new("q").expect("fixture component name"))
                    .query("MATCH (r:Room) RETURN r.value AS value")
                    .from_input_by_id(source_id)
                    .enable_bootstrap(false)
                    .build()?
                    .with_assigned_id(query_id.clone()),
            );
            core.add_components(pipeline.build()?).await?;
            let query_port = PortId::try_new("out")?;
            let query_stream = core.computation_control()?.desired_snapshot().nodes.iter()
                .find(|node| node.descriptor.id() == &query_id)
                .and_then(|node| node.output_streams.get(&query_port))
                .cloned()
                .context("actual query output stream")?;
            let sink = http_family::family_plugin(GRPC_SINK, linked)?.factories()[0].create_component_with_inputs(
                ComponentId::try_new(NETWORK_SINK_ID)?,
                json!({
                    "outputs":{"q":{"component":RESULT_QUERY_ID,"port":"out"}},"endpoint":format!("grpc://{}",receiver.address),"batchSize":3,
                    "batchFlushTimeoutMs":50,"maxRetries":0,
                }),
                vec![InputBinding {
                    input_port: PortId::try_new("in")?,
                    output: OutputEndpoint::new(query_id.clone(), query_port),
                    stream: query_stream,
                }],
            )?;
            let sink_id = sink.descriptor().id().clone();
            core.add_components(
                ComponentBatch::builder()
                    .sink(drasi_lib::computation::v1::ComponentName::try_new("sink").expect("fixture component name"), Box::new(sink))
                    .connect(
                        EdgeDefinition::new(
                            Endpoint::new(query_id, PortId::try_new("out")?),
                            Endpoint::new(sink_id, PortId::try_new("in")?),
                        ),
                        Box::new(BoundedPipeConfig { capacity: 8 }),
                    )
                    .build()?,
            )
            .await?;
            core.start().await?;
            tokio::time::timeout(DEADLINE, core.component_by_name(&drasi_core_types::ComponentName::try_new("sink").expect("fixture component name"))?.wait_started())
                .await??;
            let mut client = source::source_service_client::SourceServiceClient::connect(format!(
                "http://127.0.0.1:{port}"
            ))
            .await?;
            for value in 1..=7 {
                let response = client
                    .submit_event(source::SubmitEventRequest {
                        event: Some(proto_event(value)),
                    })
                    .await?
                    .into_inner();
                assert!(response.success);
                assert!(response.error.is_empty());
            }
            tokio::time::timeout(DEADLINE, async {
                loop {
                    let notified = receiver.state.entered.notified();
                    let count: usize = receiver
                        .state
                        .requests
                        .lock()
                        .unwrap()
                        .iter()
                        .map(|(_, request)| request.changes.as_ref().unwrap().changes.len())
                        .sum();
                    if count == 7 {
                        return Result::<()>::Ok(());
                    }
                    notified.await;
                }
            })
            .await??;
            assert_eq!(core.get_query_results_by_name(&drasi_core_types::ComponentName::try_new("q").expect("fixture component name")).await?.len(), 7);
            assert!(receiver
                .state
                .requests
                .lock()
                .unwrap()
                .iter()
                .any(|(_, request)| request.changes.as_ref().unwrap().changes.len() > 1));
            Ok::<_, anyhow::Error>(())
        }
        .await;
        let cleanup = core.shutdown().await;
        result?;
        cleanup?;
        receiver.stop().await?;
    }
    Ok(())
}
