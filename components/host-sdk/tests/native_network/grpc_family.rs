// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::http_family::family_component;
use super::*;
use futures_util::FutureExt;

#[derive(Default)]
struct GrpcState {
    requests: Mutex<
        Vec<(
            tonic::metadata::MetadataMap,
            reaction::ProcessResultsRequest,
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
impl reaction::reaction_service_server::ReactionService for Recorder {
    async fn process_results(
        &self,
        request: tonic::Request<reaction::ProcessResultsRequest>,
    ) -> std::result::Result<tonic::Response<reaction::ProcessResultsResponse>, tonic::Status> {
        let count = request
            .get_ref()
            .results
            .as_ref()
            .map_or(0, |results| results.results.len());
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
        reaction::reaction_service_server::ReactionServiceServer::new(Recorder(state.clone()));
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

fn input(query: &str, sequence: u64, routes: &[&str]) -> Result<InputEnvelope> {
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
            &ComponentId::try_new(query)?,
            SystemMetadata::new(stream(&format!("{query}/out")), sequence),
            &results,
            QueryOutputMetadata {
                query_id: query.into(),
                source_id: None,
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
            "stream-counts",
            json!({
                "stream":"facilities-db/out","host":"127.0.0.1","port":port,"ingressCapacity":4,
            }),
            linked,
        )?;
        source.start().await?;
        let mut client = source::source_service_client::SourceServiceClient::connect(format!(
            "http://127.0.0.1:{port}"
        ))
        .await?;
        let mut invalid = proto_event(3);
        invalid.source_id = "wrong-source".into();
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
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_grpc_batches_preserve_templates_metadata_boundaries_and_typed_rows() -> Result<()> {
    let receiver = server().await?;
    for linked in [true, false] {
        let mut sink = family_component(
            GRPC_SINK,
            "routed",
            json!({
                "queryStreams":{"a":"a/out","b":"b/out"},"endpoint":format!("grpc://{}",receiver.address),
                "batching":{"mode":"fixed","batchSize":3,"batchFlushTimeoutMs":30},
                "outputTemplates":{"defaultTemplate":{"added":{
                    "template":"{\"query\":\"{{query_id}}\",\"sequence\":{{sequence_id}},\"signature\":{{row_signature}}}",
                    "metadata":{"x-route":"{{after.route}}"}
                }}}
            }),
            linked,
        )?;
        sink.start().await?;
        let before = receiver.state.requests.lock().unwrap().len();
        sink.handle_batch(vec![
            input("a", 1, &["same", "same"])?,
            input("b", 1, &["same"])?,
        ])
        .await?;
        sink.handle(input("a", 2, &["left", "right"])?).await?;
        sink.stop().await?;
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
                .map(|(_, request)| request.results.as_ref().unwrap().results.len())
                .collect::<Vec<_>>(),
            [2, 1, 1, 1]
        );
        for (headers, request) in calls {
            let results = request.results.as_ref().unwrap();
            assert_eq!(
                headers
                    .get("x-query-sequence")
                    .context("query header")?
                    .to_str()?,
                results.query_id.as_str()
            );
            assert_eq!(
                headers.get("x-route").context("route header")?.to_str()?,
                request.metadata["x-route"].as_str()
            );
            for item in &results.results {
                assert!(
                    item.after.as_ref().unwrap().fields.contains_key("value"),
                    "templating must not rewrite the typed row"
                );
                let payload = item.payload.as_ref().unwrap();
                assert!(payload.fields.contains_key("signature"));
                assert_eq!(item.item_type, reaction::QueryResultItemType::Add as i32);
                assert!(item.row_signature >= 10);
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
            "retry-batch",
            json!({
                "queryId":"q","endpoint":format!("grpc://{}",receiver.address),
                "batchSize":3,"batchFlushTimeoutMs":25,"maxRetries":1,
            }),
            linked,
        )?;
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
            assert_eq!(requests[start].1.results.as_ref().unwrap().results.len(), 3);
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
    }
    receiver.stop().await
}

#[tokio::test(flavor = "current_thread")]
async fn native_grpc_adaptive_and_request_byte_limits_split_without_losing_items() -> Result<()> {
    let receiver = server().await?;
    for linked in [true, false] {
        let mut sink = family_component(
            GRPC_SINK,
            "adaptive",
            json!({
                "queryId":"q","endpoint":format!("grpc://{}",receiver.address),
                "batching":{"mode":"adaptive","adaptiveMinBatchSize":10,"adaptiveMaxBatchSize":10,
                    "adaptiveWindowSize":10,"adaptiveBatchTimeoutMs":40},
                "maxRequestBytes":1800,"maxRetries":0,
            }),
            linked,
        )?;
        sink.start().await?;
        assert_eq!(sink.batching()?.unwrap().max_operations, 10);
        let before = receiver.state.requests.lock().unwrap().len();
        sink.handle(input("q", 1, &["route"; 10])?).await?;
        sink.stop().await?;
        let requests = receiver.state.requests.lock().unwrap();
        let calls = &requests[before..];
        assert!(
            calls.len() > 1,
            "encoded-byte limit must split an otherwise compatible batch"
        );
        let signatures: Vec<_> = calls
            .iter()
            .flat_map(|(_, request)| {
                request
                    .results
                    .as_ref()
                    .unwrap()
                    .results
                    .iter()
                    .map(|item| item.row_signature)
            })
            .collect();
        assert_eq!(signatures, (10..20).collect::<Vec<_>>());
    }
    receiver.stop().await
}

#[tokio::test(flavor = "current_thread")]
async fn native_grpc_source_query_and_batched_sink_execute_in_one_drasi_instance() -> Result<()> {
    for linked in [true, false] {
        let receiver = server().await?;
        let core = drasi_lib::DrasiLib::builder()
            .with_id("grpc-family")
            .build()
            .await?;
        let port = free_port()?;
        let result = async {
            let source = family_component(
                GRPC_SOURCE,
                "facilities-db",
                json!({
                    "stream":"facilities-db/out","host":"127.0.0.1","port":port,
                }),
                linked,
            )?;
            let output = Endpoint::new(
                ComponentId::try_new("facilities-db")?,
                PortId::try_new("out")?,
            );
            core.add_components(
                ComponentBatch::builder()
                    .source(Box::new(source))
                    .bind_stream(output.clone(), stream("facilities-db/out"))
                    .build()?,
            )
            .await?;
            let pipeline = core.computation_pipeline()?.native_source(output)?.query(
                drasi_lib::Query::cypher("q")
                    .query("MATCH (r:Room) RETURN r.value AS value")
                    .from_source("facilities-db")
                    .enable_bootstrap(false)
                    .build(),
            );
            core.add_components(pipeline.build()?).await?;
            let sink = family_component(
                GRPC_SINK,
                "sink",
                json!({
                    "queryId":"q","endpoint":format!("grpc://{}",receiver.address),"batchSize":3,
                    "batchFlushTimeoutMs":50,"maxRetries":0,
                }),
                linked,
            )?;
            core.add_components(
                ComponentBatch::builder()
                    .sink(Box::new(sink))
                    .connect(
                        EdgeDefinition::new(
                            Endpoint::new(ComponentId::try_new("q")?, PortId::try_new("out")?),
                            Endpoint::new(ComponentId::try_new("sink")?, PortId::try_new("in")?),
                        ),
                        Box::new(BoundedPipeConfig { capacity: 8 }),
                    )
                    .build()?,
            )
            .await?;
            core.start().await?;
            tokio::time::timeout(DEADLINE, core.computation_component("sink")?.wait_started())
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
                        .map(|(_, request)| request.results.as_ref().unwrap().results.len())
                        .sum();
                    if count == 7 {
                        return Result::<()>::Ok(());
                    }
                    notified.await;
                }
            })
            .await??;
            assert_eq!(core.get_query_results("q").await?.len(), 7);
            assert!(receiver
                .state
                .requests
                .lock()
                .unwrap()
                .iter()
                .any(|(_, request)| request.results.as_ref().unwrap().results.len() > 1));
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
