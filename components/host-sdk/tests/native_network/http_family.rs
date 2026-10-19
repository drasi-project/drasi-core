// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use axum::{
    body::Bytes,
    extract::Query,
    http::{Method, Uri},
    routing::get,
};
use drasi_computation_plugin_sdk::Scope;
use futures_util::{StreamExt, TryStreamExt};

pub(super) fn family_plugin(kind: &str, linked: bool) -> Result<Arc<NativePlugin>> {
    if linked {
        return NativePlugin::from_definition(match kind {
            HTTP_SOURCE => drasi_computation_source_http::plugin()?,
            GRPC_SOURCE => drasi_computation_source_grpc::plugin()?,
            HTTP_SINK => drasi_computation_sink_http::plugin()?,
            GRPC_SINK => drasi_computation_sink_grpc::plugin()?,
            drasi_computation_bootstrap_http::HTTP_BOOTSTRAP => {
                drasi_computation_bootstrap_http::plugin()?
            }
            _ => anyhow::bail!("unknown test family"),
        });
    }
    if kind != drasi_computation_bootstrap_http::HTTP_BOOTSTRAP {
        return plugin(kind);
    }
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace().join("target"));
    let target = if target.is_absolute() {
        target
    } else {
        workspace().join(target)
    };
    let directory = std::env::var_os("DRASI_NATIVE_PLUGIN_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| target.join("debug"));
    computation::load(directory.join(format!(
        "{}drasi_computation_bootstrap_http{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    )))
}

pub(super) fn family_component(
    kind: &str,
    name: &str,
    config: Value,
    linked: bool,
) -> Result<NativeComponentProxy> {
    family_plugin(kind, linked)?.factories()[0]
        .create_component(ComponentId::try_new(name)?, config)
}

#[tokio::test(flavor = "current_thread")]
async fn native_webhooks_reuse_auth_mapping_timestamps_content_types_and_cors() -> Result<()> {
    const BODY: &str = r#"{"id":"r1","value":21,"time":1700000000000}"#;
    std::env::set_var("NATIVE_HTTP_FAMILY_HMAC_TEST", "native-http-test-only");
    std::env::set_var("NATIVE_HTTP_FAMILY_BEARER_TEST", "native-http-test-token");
    for linked in [true, false] {
        let port = free_port()?;
        let config = json!({
            "stream":"hooks/out","sourceId":"hooks","host":"127.0.0.1","port":port,
            "ingressCapacity":4,"adaptiveEnabled":true,"adaptiveMinBatchSize":2,"adaptiveMaxBatchSize":2,
            "webhooks":{"errorBehavior":"reject","cors":{"allowOrigins":["https://example.test"]},
                "routes":[{
                    "path":"/hooks/:tenant","methods":["POST"],
                    "auth":{"signature":{"type":"hmac-sha256","secretEnv":"NATIVE_HTTP_FAMILY_HMAC_TEST",
                        "header":"x-signature","prefix":"sha256="},
                        "bearer":{"tokenEnv":"NATIVE_HTTP_FAMILY_BEARER_TEST"}},
                    "mappings":[
                        {"operation":"insert","elementType":"node",
                         "effectiveFrom":{"value":"{{payload.time}}","format":"unix_millis"},
                         "template":{"id":"{{payload.id}}","labels":["Room"],
                            "properties":{"value":"{{payload.value}}","tenant":"{{route.tenant}}","region":"{{query.region}}"}}},
                        {"operation":"insert","elementType":"relation",
                         "template":{"id":"{{route.tenant}}-{{payload.id}}","labels":["OWNS"],
                            "from":"{{route.tenant}}","to":"{{payload.id}}"}}],
                },{
                    "path":"/format/:kind","mappings":[{"operation":"insert","elementType":"node",
                        "template":{"id":"{{route.kind}}","labels":["Payload"],"properties":{"data":"{{payload}}"}}}]
                }]
            }
        });
        let mut source = family_component(HTTP_SOURCE, "hooks", config, linked)?;
        source.start().await?;
        let client = reqwest::Client::new();
        let url = format!("http://127.0.0.1:{port}/hooks/acme?region=west%20coast");
        assert_eq!(
            client.post(&url).body(BODY).send().await?.status(),
            StatusCode::UNAUTHORIZED
        );
        let response = client
            .post(&url)
            .header("content-type", "application/json")
            .header("authorization", "Bearer native-http-test-token")
            .header(
                "x-signature",
                "sha256=d0302b6518bdbfe1561a712ebf1b46031223ed010b7f1d243cc51b72882ec3d9",
            )
            .body(BODY)
            .send()
            .await?
            .error_for_status()?
            .json::<Value>()
            .await?;
        assert_eq!(response["events_processed"], 2);
        let first = GraphChangeCodec::decode_changes(&next(&mut source).await?)?;
        let [SourceChange::Insert {
            element:
                Element::Node {
                    metadata,
                    properties,
                },
        }] = first.as_slice()
        else {
            anyhow::bail!("expected mapped node");
        };
        assert_eq!(metadata.reference.source_id.as_ref(), "hooks");
        assert_eq!(metadata.reference.element_id.as_ref(), "r1");
        assert_eq!(metadata.effective_from, 1_700_000_000_000);
        assert_eq!(
            properties.get("value"),
            Some(&drasi_core::models::ElementValue::Integer(21))
        );
        assert_eq!(
            properties.get("region"),
            Some(&drasi_core::models::ElementValue::String(
                "west coast".into()
            ))
        );
        let relation = GraphChangeCodec::decode_changes(&next(&mut source).await?)?;
        assert!(
            matches!(&relation[0], SourceChange::Insert { element: Element::Relation { in_node, out_node, .. }}
            if in_node.element_id.as_ref() == "r1" && out_node.element_id.as_ref() == "acme")
        );
        for (kind, content_type, body) in [
            ("json", "application/json", r#"{"value":7}"#),
            ("yaml", "application/yaml", "value: 7"),
            (
                "xml",
                "application/xml",
                "<payload><value>7</value></payload>",
            ),
            ("text", "text/plain", "hello"),
        ] {
            client
                .post(format!("http://127.0.0.1:{port}/format/{kind}"))
                .header("content-type", content_type)
                .body(body)
                .send()
                .await?
                .error_for_status()?;
            let mapped = GraphChangeCodec::decode_changes(&next(&mut source).await?)?;
            assert!(
                matches!(&mapped[0], SourceChange::Insert { element: Element::Node { metadata, .. }}
                if metadata.reference.element_id.as_ref() == kind)
            );
        }
        let cors = client
            .request(reqwest::Method::OPTIONS, &url)
            .header("origin", "https://example.test")
            .header("access-control-request-method", "POST")
            .send()
            .await?;
        assert_eq!(
            cors.headers()["access-control-allow-origin"],
            "https://example.test"
        );
        source.stop().await?;
        drop(TcpListener::bind(("127.0.0.1", port)).await?);
    }
    std::env::remove_var("NATIVE_HTTP_FAMILY_HMAC_TEST");
    std::env::remove_var("NATIVE_HTTP_FAMILY_BEARER_TEST");
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_http_adaptive_prefetch_keeps_the_first_event_when_next_is_cancelled() -> Result<()>
{
    for linked in [true, false] {
        let port = free_port()?;
        let mut source = family_component(
            HTTP_SOURCE,
            "adaptive",
            json!({
                "stream":"facilities-db/out","host":"127.0.0.1","port":port,"ingressCapacity":3,
                "adaptiveEnabled":true,"adaptiveMinBatchSize":3,"adaptiveMaxBatchSize":3,
                "adaptiveMinWaitMs":1000,"adaptiveMaxWaitMs":1000
            }),
            linked,
        )?;
        source.start().await?;
        let client = reqwest::Client::new();
        let url = format!("http://127.0.0.1:{port}/sources/facilities-db/events");
        client
            .post(&url)
            .json(&http_event(1))
            .send()
            .await?
            .error_for_status()?;
        assert!(
            tokio::time::timeout(Duration::from_millis(20), source.next())
                .await
                .is_err()
        );
        for value in 2..=3 {
            client
                .post(&url)
                .json(&http_event(value))
                .send()
                .await?
                .error_for_status()?;
        }
        for value in 1..=3 {
            let envelope = next(&mut source).await?;
            assert_eq!(source_value(&envelope)?, value);
            assert_eq!(envelope.system().sequence(), value as u64);
        }
        source.stop().await?;
    }
    Ok(())
}

#[derive(Default)]
struct RestState {
    requests: Mutex<Vec<(Method, String, HeaderMap, Value)>>,
    token_requests: AtomicUsize,
    row_requests: AtomicUsize,
    truncate_first: AtomicUsize,
    arrived: Notify,
}
struct RestServer {
    address: SocketAddr,
    state: Arc<RestState>,
    cancel: CancellationToken,
    task: JoinHandle<Result<()>>,
}
impl RestServer {
    async fn stop(self) -> Result<()> {
        self.cancel.cancel();
        self.task.await??;
        Ok(())
    }
}
async fn rest() -> Result<RestServer> {
    async fn token(State(state): State<Arc<RestState>>) -> Json<Value> {
        state.token_requests.fetch_add(1, Ordering::SeqCst);
        Json(
            json!({"access_token":"native-bootstrap-test-token","expires_in":3600,"token_type":"Bearer"}),
        )
    }
    async fn rows(
        State(state): State<Arc<RestState>>,
        Query(params): Query<HashMap<String, String>>,
        headers: HeaderMap,
    ) -> axum::response::Response {
        use axum::response::IntoResponse;
        state.row_requests.fetch_add(1, Ordering::SeqCst);
        if headers.get("authorization").and_then(|v| v.to_str().ok())
            != Some("Bearer native-bootstrap-test-token")
        {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        if state.truncate_first.swap(0, Ordering::SeqCst) > 0 {
            return (
                StatusCode::OK,
                [("content-type", "application/json")],
                r#"{"items":["#,
            )
                .into_response();
        }
        let offset = params
            .get("offset")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);
        let items = [json!({"id":"r1","value":10}), json!({"id":"r2","value":20})];
        Json(json!({"items":items.get(offset).into_iter().collect::<Vec<_>>(),"total":2}))
            .into_response()
    }
    async fn capture(
        State(state): State<Arc<RestState>>,
        method: Method,
        uri: Uri,
        headers: HeaderMap,
        body: Bytes,
    ) -> StatusCode {
        let body =
            serde_json::from_slice(&body).unwrap_or_else(|_| json!(String::from_utf8_lossy(&body)));
        state
            .requests
            .lock()
            .unwrap()
            .push((method, uri.path().to_owned(), headers, body));
        state.arrived.notify_one();
        StatusCode::OK
    }
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let state = Arc::new(RestState::default());
    let cancel = CancellationToken::new();
    let shutdown = cancel.clone();
    let router = Router::new()
        .route("/token", post(token))
        .route("/rows", get(rows))
        .fallback(capture)
        .with_state(state.clone());
    let task = tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(shutdown.cancelled_owned())
            .await?;
        Ok(())
    });
    Ok(RestServer {
        address,
        state,
        cancel,
        task,
    })
}

fn snapshot_config(address: SocketAddr) -> Value {
    json!({
        "source":"source","stream":"source/out","outputCapacity":1,
        "maxDurationMs":5000,"settings":{"maxRetries":1,"retryDelayMs":1,"maxPages":4,
            "endpoints":[{
                "url":format!("http://{address}/rows"),
                "auth":{"type":"oauth2-client-credentials","token_url":format!("http://{address}/token"),
                    "client_id_env":"NATIVE_HTTP_BOOTSTRAP_CLIENT_TEST","client_secret_env":"NATIVE_HTTP_BOOTSTRAP_SECRET_TEST"},
                "pagination":{"type":"offset-limit","page_size":1,"total_path":"$.total"},
                "response":{"itemsPath":"$.items","mappings":[{"elementType":"node","template":{
                    "id":"{{item.id}}","labels":["Room"],"properties":{"value":"{{item.value}}"}
                }}]}
            }]
        }
    })
}

#[tokio::test(flavor = "current_thread")]
async fn native_http_templates_and_coalesced_bodies_keep_the_legacy_wire_contract() -> Result<()> {
    let server = rest().await?;
    for linked in [true, false] {
        let mut sink = family_component(
            HTTP_SINK,
            "templated",
            json!({
                "queryId":"q","baseUrl":format!("http://{}",server.address),
                "outputTemplates":{"routes":{"q":{"added":{"method":"PUT","url":"/row/{{after.value}}",
                    "headers":{"x-rendered-query":"{{query_id}}"},
                    "template":"{\"value\":{{after.value}},\"operation\":\"{{operation}}\",\"metadata\":{{json metadata}}}"}}}}
            }),
            linked,
        )?;
        sink.start().await?;
        sink.handle(result_input(1)?).await?;
        sink.stop().await?;
        let request = server
            .state
            .requests
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .clone();
        assert_eq!(request.0, Method::PUT);
        assert_eq!(request.1, "/row/42");
        assert_eq!(request.2["x-rendered-query"], "q");
        assert_eq!(request.3["value"], 42);
        assert_eq!(request.3["operation"], "ADD");
        assert_eq!(request.3["metadata"]["source_id"], "facilities-db");
        let mut batch = family_component(
            HTTP_SINK,
            "batch",
            json!({
                "queryStreams":{"q":"q/out"},"baseUrl":format!("http://{}",server.address),
                "adaptive":{"adaptiveMinBatchSize":2,"adaptiveMaxBatchSize":2,"adaptiveBatchTimeoutMs":50},
                "batchEndpoint":"/batch"
            }),
            linked,
        )?;
        batch.start().await?;
        batch
            .handle_batch(vec![result_input(2)?, result_input(3)?])
            .await?;
        batch.stop().await?;
        let request = server
            .state
            .requests
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .clone();
        assert_eq!(request.1, "/batch");
        let items = request.3["batch"].as_array().context("batch body")?;
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["sequenceId"], 2);
        assert_eq!(items[1]["sequenceId"], 3);
    }
    server.stop().await
}

#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(native_http_bootstrap)]
async fn native_http_bootstrap_oauth_pagination_retry_and_live_update_share_one_drasi_instance(
) -> Result<()> {
    std::env::set_var("NATIVE_HTTP_BOOTSTRAP_CLIENT_TEST", "test-client");
    std::env::set_var("NATIVE_HTTP_BOOTSTRAP_SECRET_TEST", "test-only-secret");
    let server = rest().await?;
    for linked in [true, false] {
        server.state.truncate_first.store(1, Ordering::SeqCst);
        let native = family_plugin(drasi_computation_bootstrap_http::HTTP_BOOTSTRAP, linked)?;
        assert!(native.factories().is_empty());
        let core = drasi_lib::DrasiLib::builder()
            .with_id("http-family")
            .build()
            .await?;
        let port = free_port()?;
        let result = async {
            let source = family_component(
                HTTP_SOURCE,
                "source",
                json!({
                    "sourceId":"source","stream":"source/out","host":"127.0.0.1","port":port
                }),
                linked,
            )?;
            let output = Endpoint::new(ComponentId::try_new("source")?, PortId::try_new("out")?);
            core.add_components(
                ComponentBatch::builder()
                    .source(Box::new(source))
                    .bind_stream(output.clone(), stream("source/out"))
                    .build()?,
            )
            .await?;
            let snapshot = native.bootstrap_factories()[0].create_provider(
                ComponentId::try_new("q")?,
                snapshot_config(server.address),
                Some(Scope {
                    instance_id: "http-family".into(),
                    graph_id: core
                        .computation_control()?
                        .desired_snapshot()
                        .id
                        .to_string(),
                    generation: 1,
                }),
                None,
            )?;
            let pipeline = core
                .computation_pipeline()?
                .native_source(output)?
                .query_bootstrap("q", snapshot)?
                .query(
                    drasi_lib::Query::cypher("q")
                        .query("MATCH (r:Room) RETURN elementId(r) AS id,r.value AS value")
                        .from_source("source")
                        .enable_bootstrap(true)
                        .build(),
                );
            core.add_components(pipeline.build()?).await?;
            let sink = family_component(
                HTTP_SINK,
                "sink",
                json!({
                    "queryId":"q","url":format!("http://{}/reaction",server.address)
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
                        Box::new(BoundedPipeConfig { capacity: 2 }),
                    )
                    .build()?,
            )
            .await?;
            core.start().await?;
            tokio::time::timeout(DEADLINE, core.computation_component("q")?.wait_started())
                .await??;
            let mut results = core.subscribe_query_results("q").await?;
            assert_eq!(results.snapshot.len(), 2);
            let mut initial: Vec<_> = results.snapshot.to_vec().into_iter().map(|row| row["value"].as_i64().expect("snapshot value")).collect();
            initial.sort();
            assert_eq!(initial, [10, 20]);
            reqwest::Client::new()
                .post(format!("http://127.0.0.1:{port}/sources/source/events"))
                .json(
                    &json!({"operation":"update","timestamp":1700000000000000000u64,"element":{
                        "type":"node","id":"r1","labels":["Room"],"properties":{"value":99}
                    }}),
                )
                .send()
                .await?
                .error_for_status()?;
            tokio::time::timeout(DEADLINE, async {
                loop {
                    let delivered = server.state.arrived.notified();
                    let rows = core.get_query_results("q").await?;
                    let effect = server.state.requests.lock().unwrap().iter()
                        .any(|(_,path,_,body)| path == "/reaction" && body["operation"] == "UPDATE"
                            && body["before"]["value"] == 10 && body["after"]["value"] == 99);
                    if rows.len() == 2 && rows.iter().any(|row| row["value"] == 99) && effect {
                        break;
                    }
                    tokio::select! {
                        _ = delivered => {},
                        result = results.stream.next() => { result.context("query results closed")??; },
                    }
                }
                Result::<()>::Ok(())
            })
            .await??;
            assert_eq!(
                core.computation_component("source")?.observed()?.lifecycle,
                ComponentLifecycle::Running
            );
            assert!(core.computation_component("sink")?.observed()?.failure.is_none());
            Ok::<_, anyhow::Error>(())
        }
        .await;
        let cleanup = core.shutdown().await;
        result?;
        cleanup?;
    }
    assert!(
        server.state.row_requests.load(Ordering::SeqCst) >= 6,
        "truncated page must retry without dropping it"
    );
    assert_eq!(
        server.state.token_requests.load(Ordering::SeqCst),
        2,
        "OAuth token is cached for each snapshot"
    );
    server.stop().await?;
    std::env::remove_var("NATIVE_HTTP_BOOTSTRAP_CLIENT_TEST");
    std::env::remove_var("NATIVE_HTTP_BOOTSTRAP_SECRET_TEST");
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(native_http_bootstrap)]
async fn native_http_bootstrap_bounds_and_cancelled_streams_never_report_partial_success(
) -> Result<()> {
    std::env::set_var("NATIVE_HTTP_BOOTSTRAP_CLIENT_TEST", "test-client");
    std::env::set_var("NATIVE_HTTP_BOOTSTRAP_SECRET_TEST", "test-only-secret");
    let server = rest().await?;
    for linked in [true, false] {
        let native = family_plugin(drasi_computation_bootstrap_http::HTTP_BOOTSTRAP, linked)?;
        for option in ["maxResponseBytes", "maxPages"] {
            let mut config = snapshot_config(server.address);
            if option == "maxPages" {
                config["settings"]["maxPages"] = json!(1);
            } else {
                config[option] = json!(8);
            }
            let provider = native.bootstrap_factories()[0].create_provider(
                ComponentId::try_new("q")?,
                config,
                Some(Scope {
                    instance_id: "limits".into(),
                    graph_id: "limits".into(),
                    generation: 1,
                }),
                None,
            )?;
            provider.prepare().await?;
            assert!(provider
                .snapshot()
                .await?
                .changes
                .try_collect::<Vec<_>>()
                .await
                .is_err());
            provider.stop().await?;
        }
        let provider = native.bootstrap_factories()[0].create_provider(
            ComponentId::try_new("q")?,
            snapshot_config(server.address),
            Some(Scope {
                instance_id: "cancel".into(),
                graph_id: "cancel".into(),
                generation: 1,
            }),
            None,
        )?;
        provider.prepare().await?;
        let mut old = provider.snapshot().await?.changes;
        old.next().await.context("first snapshot row")??;
        provider.stop().await?;
        provider.prepare().await?;
        let fresh = provider.snapshot().await?;
        assert!(old.next().await.context("stale stream error")?.is_err());
        drop(old);
        assert_eq!(fresh.changes.try_collect::<Vec<_>>().await?.len(), 2);
        provider.complete_snapshot().await?;
        provider.stop().await?;
        let mut config = snapshot_config(server.address);
        config["nodeLabels"] = json!(["Room"]);
        config["relationLabels"] = json!(["Linked"]);
        config["settings"]["endpoints"][0]["response"]["mappings"]
            .as_array_mut()
            .context("mapping array")?
            .push(json!({
                "elementType":"relation","operation":"delete",
                "template":{"id":"{{item.id}}-link","labels":["Linked"]}
            }));
        let provider = native.bootstrap_factories()[0].create_provider(
            ComponentId::try_new("q")?,
            config,
            Some(Scope {
                instance_id: "labels".into(),
                graph_id: "labels".into(),
                generation: 1,
            }),
            None,
        )?;
        provider.prepare().await?;
        let events = provider
            .snapshot()
            .await?
            .changes
            .try_collect::<Vec<_>>()
            .await?;
        assert_eq!(
            events.len(),
            4,
            "relation deletes must use relation label filters, not node filters"
        );
        let mut deletes = 0;
        for event in events {
            for change in GraphChangeCodec::decode_changes(&event)? {
                if let SourceChange::Delete { metadata } = change {
                    assert_eq!(metadata.labels[0].as_ref(), "Linked");
                    deletes += 1;
                }
            }
        }
        assert_eq!(deletes, 2);
        provider.complete_snapshot().await?;
        provider.stop().await?;
    }
    server.stop().await?;
    std::env::remove_var("NATIVE_HTTP_BOOTSTRAP_CLIENT_TEST");
    std::env::remove_var("NATIVE_HTTP_BOOTSTRAP_SECRET_TEST");
    Ok(())
}
