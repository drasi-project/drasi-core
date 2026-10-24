// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_computation_sink_sse::SSE_SINK;
use std::{collections::BTreeMap, num::NonZeroUsize};

async fn roundtrip(statically_linked: bool) -> Result<()> {
    let port = free_port()?;
    let config = json!({
        "queryStreams": {"q": "q/out"},
        "host": "127.0.0.1", "port": port,
        "ssePath": "/events", "heartbeatIntervalMs": 25,
    });
    let mut sink = if statically_linked {
        let plugin = NativePlugin::from_definition(drasi_computation_sink_sse::plugin()?)?;
        plugin.factories()[0].create_component(ComponentId::try_new("ui")?, config)?
    } else {
        component(SSE_SINK, "ui", config)?
    };
    assert_eq!(sink.completion(), SinkCompletion::Accepted);
    assert!(sink.handle(result_input(1)?).await.is_err());
    sink.start().await?;
    let original = sink.configuration()?;
    let client = reqwest::Client::new();
    assert_eq!(
        client
            .get(format!("http://127.0.0.1:{port}/snapshot/q"))
            .send()
            .await?
            .status(),
        StatusCode::SERVICE_UNAVAILABLE,
    );
    assert_eq!(
        client
            .get(format!("http://127.0.0.1:{port}/missing"))
            .send()
            .await?
            .status(),
        StatusCode::NOT_FOUND
    );
    let response = client
        .request(
            reqwest::Method::OPTIONS,
            format!("http://127.0.0.1:{port}/events"),
        )
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let mut response = client
        .get(format!("http://127.0.0.1:{port}/events"))
        .send()
        .await?
        .error_for_status()?;
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    assert_eq!(response.headers()["access-control-allow-origin"], "*");
    sink.handle(result_input(42)?).await?;
    let mut frames = Vec::new();
    tokio::time::timeout(DEADLINE, async {
        let mut pending = String::new();
        loop {
            let bytes = response.chunk().await?.context("SSE stream ended")?;
            pending.push_str(std::str::from_utf8(&bytes)?);
            while let Some(end) = pending.find("\n\n") {
                let frame: String = pending.drain(..end + 2).collect();
                for data in frame.lines().filter_map(|line| line.strip_prefix("data:")) {
                    frames.push(serde_json::from_str::<Value>(data.trim())?);
                }
            }
            if frames.iter().any(|frame| frame["queryId"] == "q")
                && frames.iter().any(|frame| frame["type"] == "heartbeat")
            {
                return Ok::<_, anyhow::Error>(());
            }
        }
    })
    .await??;
    let expected = QueryChangeCodec::to_legacy_result(&result_input(42)?.envelope)?;
    assert_eq!(
        frames
            .iter()
            .find(|frame| frame["queryId"] == "q")
            .context("query event")?["results"],
        serde_json::to_value(expected.results)?
    );
    assert_eq!(sink.configuration()?, original);
    sink.stop().await?;
    tokio::time::timeout(DEADLINE, async {
        while response.chunk().await?.is_some() {}
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    assert!(sink.handle(result_input(43)?).await.is_err());
    sink.start().await?;
    sink.handle(result_input(44)?).await?;
    sink.stop().await?;
    let _released = TcpListener::bind(("127.0.0.1", port)).await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_sse_current_thread() -> Result<()> {
    for statically_linked in [false, true] {
        roundtrip(statically_linked).await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_sse_multi_thread() -> Result<()> {
    for statically_linked in [false, true] {
        roundtrip(statically_linked).await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_sse_dynamic_routes_are_bounded_and_recreated_only_after_cleanup() -> Result<()> {
    for linked in [false, true] {
        let port = free_port()?;
        let config = json!({
            "queryStreams": {"q": "q/out"}, "host": "127.0.0.1", "port": port,
            "heartbeatIntervalMs": 20, "maxPaths": 2,
            "defaultTemplate": {
                "added": {"template": "{\"value\":{{after.value}}}", "path": "values/{{after.value}}"}
            },
        });
        let mut sink = if linked {
            NativePlugin::from_definition(drasi_computation_sink_sse::plugin()?)?.factories()[0]
                .create_component(ComponentId::try_new("ui")?, config)?
        } else {
            component(SSE_SINK, "ui", config)?
        };
        sink.start().await?;
        let client = reqwest::Client::new();
        let path = format!("http://127.0.0.1:{port}/events/values/42");
        assert_eq!(
            client.get(&path).send().await?.status(),
            StatusCode::NOT_FOUND
        );
        sink.handle(result_input(1)?).await?;
        let mut events = EventReader {
            response: client.get(&path).send().await?.error_for_status()?,
            pending: String::new(),
        };
        sink.handle(result_input(2)?).await?;
        assert_eq!(events.next().await?, json!({"value": 42}));
        assert!(sink.handle(result_inputs(3, &[42, 43])?).await.is_err());
        assert!(
            tokio::time::timeout(Duration::from_millis(50), events.next())
                .await
                .is_err(),
            "path exhaustion must fail before broadcasting an existing route's partial batch",
        );
        assert_eq!(
            client
                .get(format!("http://127.0.0.1:{port}/events/values/43"))
                .send()
                .await?
                .status(),
            StatusCode::NOT_FOUND,
        );
        sink.stop().await?;
        tokio::time::timeout(DEADLINE, async {
            while events.response.chunk().await?.is_some() {}
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        sink.start().await?;
        assert_eq!(
            client.get(&path).send().await?.status(),
            StatusCode::NOT_FOUND
        );
        sink.handle(result_input(4)?).await?;
        assert_eq!(client.get(&path).send().await?.status(), StatusCode::OK);
        sink.stop().await?;
        let _released = TcpListener::bind(("127.0.0.1", port)).await?;
    }
    Ok(())
}

#[test]
fn native_sse_snapshot_binding_rejects_a_foreign_graph_before_construction() -> Result<()> {
    let snapshots = Arc::new(
        drasi_host_sdk::computation::NativeQuerySnapshotsResource::new(
            QueryResultsCatalog::new("foreign")?,
            ["q".into()],
            Duration::from_secs(1),
            1,
        )?,
    );
    let plugin = NativePlugin::from_definition(drasi_computation_sink_sse::plugin()?)?;
    assert!(plugin.factories()[0]
        .create_component_with_query_snapshots(
            ComponentId::try_new("ui")?,
            json!({"queryStreams": {"q": "q/out"}, "host": "127.0.0.1", "port": 0}),
            drasi_computation_plugin_sdk::Scope {
                instance_id: "owner".into(),
                graph_id: "owner".into(),
                generation: 1,
            },
            snapshots,
        )
        .is_err());
    Ok(())
}

struct EventReader {
    response: reqwest::Response,
    pending: String,
}
impl EventReader {
    async fn next(&mut self) -> Result<Value> {
        tokio::time::timeout(DEADLINE, async {
            loop {
                while let Some(end) = self.pending.find("\n\n") {
                    let frame: String = self.pending.drain(..end + 2).collect();
                    let payload = frame
                        .lines()
                        .filter_map(|line| line.strip_prefix("data:"))
                        .map(str::trim_start)
                        .collect::<Vec<_>>()
                        .join("\n");
                    if payload.is_empty() {
                        continue;
                    }
                    let value: Value = serde_json::from_str(&payload)?;
                    if value["type"] != "heartbeat" {
                        return Ok(value);
                    }
                }
                let bytes = self.response.chunk().await?.context("SSE stream ended")?;
                self.pending.push_str(std::str::from_utf8(&bytes)?);
            }
        })
        .await?
    }
}

async fn instance_snapshots_and_routes(statically_linked: bool) -> Result<()> {
    const QUERY: &str = "scope.q";
    let core = drasi_lib::DrasiLib::builder()
        .with_id(format!("sse-host-{statically_linked}"))
        .build()
        .await?;
    let result = async {
        core.start().await?;
        let graph_id = core
            .computation_control()?
            .desired_snapshot()
            .id
            .to_string();
        let source_port = free_port()?;
        let sse_port = free_port()?;
        let source_plugin = if statically_linked {
            NativePlugin::from_definition(drasi_computation_source_http::plugin()?)?
        } else {
            plugin(HTTP_SOURCE)?
        };
        let sse_plugin = if statically_linked {
            NativePlugin::from_definition(drasi_computation_sink_sse::plugin()?)?
        } else {
            plugin(SSE_SINK)?
        };
        let source = source_plugin.factories()[0].clone();
        let sink = sse_plugin.factories()[0].clone();
        let indexes_id = ResourceId::try_new("indexes")?;
        let catalog_id = ResourceId::try_new("catalog")?;
        let snapshots_id = ResourceId::try_new("snapshots")?;
        let catalog = QueryResultsCatalog::new(graph_id.clone())?;
        let snapshots = Arc::new(
            drasi_host_sdk::computation::NativeQuerySnapshotsResource::new(
                catalog.clone(),
                [QUERY.into()],
                Duration::from_millis(250),
                4,
            )?,
        );
        let query = ContinuousQueryDefinition {
            graph_id: graph_id.clone(),
            id: ComponentId::try_new(QUERY)?,
            query: "MATCH (r:Room) RETURN r.value AS value".into(),
            language: ComputationQueryLanguage::Cypher,
            output_stream: stream("scope.q/out"),
            outbox_capacity: NonZeroUsize::new(32).expect("nonzero outbox capacity"),
        };
        let query_spec = ComponentSpecification {
            descriptor: query.descriptor(),
            role: ComponentRole::Query,
            completion: None,
            implementation: ImplementationIdentity::try_new("drasi/continuous-query", "1")?,
            configuration_version: 1,
            configuration: BTreeMap::from([
                (
                    "query".into(),
                    ConfigurationValue::Literal(query.query.clone().into()),
                ),
                (
                    "stream".into(),
                    ConfigurationValue::Literal("scope.q/out".into()),
                ),
            ]),
            dependencies: BTreeMap::from([
                ("indexes".into(), vec![indexes_id.clone()]),
                ("catalog".into(), vec![catalog_id.clone()]),
            ]),
        };
        let mut sink_spec = sink.specification(
            ComponentId::try_new("ui")?,
            json!({
                "queryStreams": {QUERY: "scope.q/out"},
                "host": "127.0.0.1", "port": sse_port, "heartbeatIntervalMs": 25,
                "routes": {"q": {
                    "added": {"template": "", "path": "items"},
                    "updated": {"template": "{{json after}}", "path": "items"},
                    "deleted": {"template": "{\"deleted\":{{json before}}}", "path": "items"}
                }}
            }),
        )?;
        sink_spec
            .dependencies
            .insert("query_snapshots".into(), vec![snapshots_id.clone()]);
        let endpoint = |component: &str, port: &str| -> Result<Endpoint> {
            Ok(Endpoint::new(
                ComponentId::try_new(component)?,
                PortId::try_new(port)?,
            ))
        };
        let mut builder = ComputationGraph::builder(graph_id.as_str());
        for (id, role, handle) in [
            (
                indexes_id,
                ResourceRole::IndexBackend,
                ResourceHandle::new(
                    ResourceRole::IndexBackend,
                    Arc::new(QueryIndexProviderResource(Arc::new(
                        InMemoryComputationProvider,
                    ))),
                ),
            ),
            (
                catalog_id,
                ResourceRole::QueryCatalog,
                ResourceHandle::new(ResourceRole::QueryCatalog, Arc::new(catalog)),
            ),
            (
                snapshots_id,
                ResourceRole::QueryCatalog,
                ResourceHandle::new(ResourceRole::QueryCatalog, snapshots),
            ),
        ] {
            builder = builder
                .declare_resource(ResourceSpecification {
                    id: id.clone(),
                    role,
                    ownership: ResourceOwnership::Borrowed,
                    binding: "sse-fixture".into(),
                })?
                .provide_resource(id, handle)?;
        }
        let batch = builder
            .component(
                source.specification(
                    ComponentId::try_new("facilities-db")?,
                    json!({
                        "sourceId": "facilities-db", "stream": "facilities-db",
                        "host": "127.0.0.1", "port": source_port,
                    }),
                )?,
                source,
            )
            .component(query_spec, Arc::new(ContinuousQueryFactory::default()))
            .component(sink_spec, sink)
            .connect(
                EdgeDefinition::new(endpoint("facilities-db", "out")?, endpoint(QUERY, "in")?),
                Box::new(BoundedPipeConfig { capacity: 16 }),
            )
            .connect(
                EdgeDefinition::new(endpoint(QUERY, "out")?, endpoint("ui", "in")?),
                Box::new(BoundedPipeConfig { capacity: 16 }),
            )
            .bind_stream(endpoint("facilities-db", "out")?, stream("facilities-db"))
            .bind_stream(endpoint(QUERY, "out")?, stream("scope.q/out"))
            .build_components()?;
        let report = core.add_components(batch).await?;
        assert!(report.committed, "{report:?}");
        let client = reqwest::Client::new();
        let snapshots = format!("http://127.0.0.1:{sse_port}/snapshot/{QUERY}");
        assert_eq!(
            client
                .get(format!("http://127.0.0.1:{sse_port}/snapshot/foreign"))
                .send()
                .await?
                .status(),
            StatusCode::NOT_FOUND,
        );
        let mut events = EventReader {
            response: client
                .get(format!("http://127.0.0.1:{sse_port}/events/items"))
                .send()
                .await?
                .error_for_status()?,
            pending: String::new(),
        };
        let initial = client.get(&snapshots).send().await?.error_for_status()?;
        assert_eq!(initial.json::<Value>().await?, json!([]));
        let ingress = format!("http://127.0.0.1:{source_port}/sources/facilities-db/events");
        client
            .post(&ingress)
            .json(&http_event(1))
            .send()
            .await?
            .error_for_status()?;
        let added = events.next().await?;
        assert_eq!(added["queryId"], QUERY);
        assert_eq!(added["result"]["data"], json!({"value": 1}));
        let snapshot = client.get(&snapshots).send().await?.error_for_status()?;
        let sequence: u64 = snapshot.headers()["x-drasi-query-sequence"]
            .to_str()?
            .parse()?;
        assert!(sequence > 0);
        assert_eq!(snapshot.json::<Value>().await?, json!([{"value": 1}]));
        let mut update = http_event(2);
        update["operation"] = json!("update");
        update["timestamp"] = json!(1234568890000u64);
        update["element"]["id"] = json!("row-1");
        client
            .post(&ingress)
            .json(&update)
            .send()
            .await?
            .error_for_status()?;
        assert_eq!(events.next().await?, json!({"value": 2}));
        assert_eq!(
            client
                .get(&snapshots)
                .send()
                .await?
                .error_for_status()?
                .json::<Value>()
                .await?,
            json!([{"value": 2}]),
        );
        client
            .post(&ingress)
            .json(&json!({
                "operation": "delete", "timestamp": 1234569890000u64, "id": "row-1",
            }))
            .send()
            .await?
            .error_for_status()?;
        assert_eq!(events.next().await?, json!({"deleted": {"value": 2}}));
        assert_eq!(
            client
                .get(&snapshots)
                .send()
                .await?
                .error_for_status()?
                .json::<Value>()
                .await?,
            json!([]),
        );
        drop(events);
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let shutdown = core.shutdown().await;
    result?;
    shutdown?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_sse_snapshots_and_templates_share_the_instance_query_on_current_thread(
) -> Result<()> {
    for statically_linked in [false, true] {
        instance_snapshots_and_routes(statically_linked).await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_sse_snapshots_and_templates_share_the_instance_query_on_multi_thread() -> Result<()>
{
    for statically_linked in [false, true] {
        instance_snapshots_and_routes(statically_linked).await?;
    }
    Ok(())
}
