// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_computation_contracts::GraphChangeBatch;
use drasi_computation_sink_sse::SSE_SINK;
use drasi_host_sdk::computation::{NativeOutputSnapshotsResource, OutputSelector};
use std::{collections::BTreeMap, num::NonZeroUsize};

async fn roundtrip(statically_linked: bool) -> Result<()> {
    let port = free_port()?;
    let config = json!({
        "outputs":{"q":{"component":"q","port":"out"}},
        "host": "127.0.0.1", "port": port,
        "ssePath": "/events", "heartbeatIntervalMs": 25,
    });
    let mut sink = if statically_linked {
        let plugin = NativePlugin::from_definition(drasi_computation_sink_sse::plugin()?)?;
        crate::sink_inputs::create(&plugin.factories()[0], ComponentId::try_new("ui")?, config)?
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
        StatusCode::NOT_IMPLEMENTED,
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
            if frames
                .iter()
                .any(|frame| frame["output"]["component"] == "q")
                && frames.iter().any(|frame| frame["type"] == "heartbeat")
            {
                return Ok::<_, anyhow::Error>(());
            }
        }
    })
    .await??;
    let expected = GraphChangeBatch::from_envelope(&result_input(42)?.envelope)?
        .changes()
        .iter()
        .map(|change| change.to_json())
        .collect::<Result<Vec<_>, _>>()?;
    let frame = frames
        .iter()
        .find(|frame| frame["output"]["component"] == "q")
        .context("output frame")?;
    assert_eq!(frame["changes"], json!(expected));
    assert_eq!(
        frame["output"],
        json!({"alias": "q", "component": "q", "port": "out"})
    );
    assert!(frame.get("producer").is_none() && frame.get("timestamp").is_none());
    assert!(
        frame["publishedAt"].as_i64().is_some(),
        "sink publication time"
    );
    assert_eq!(frame["sequence"], 42);
    assert_eq!(frame["changes"][0]["operation"], "ADD");
    assert_eq!(frame["changes"][0]["after"], json!({"value": 42}));
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
            "outputs":{"q":{"component":"q","port":"out"}}, "host": "127.0.0.1", "port": port,
            "heartbeatIntervalMs": 20, "maxPaths": 2,
            "defaultTemplate": {
                "added": {"template": "{\"value\":{{after.value}}}", "path": "values/{{after.value}}"}
            },
        });
        let mut sink = if linked {
            crate::sink_inputs::create(
                &NativePlugin::from_definition(drasi_computation_sink_sse::plugin()?)?.factories()
                    [0],
                ComponentId::try_new("ui")?,
                config,
            )?
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
fn native_sse_output_snapshot_policy_rejects_invalid_limits_and_duplicates() -> Result<()> {
    let output = |component: &str, port: Option<&str>| -> Result<OutputSelector> {
        Ok(OutputSelector::new(
            ComponentId::try_new(component)?,
            port.map(PortId::try_new).transpose()?,
        ))
    };
    let second_port = [output("q", Some("out"))?, output("q", Some("errors"))?];
    assert!(NativeOutputSnapshotsResource::new(second_port, Duration::from_secs(1), 1).is_ok());
    for (outputs, timeout, readers) in [
        (vec![], Duration::from_secs(1), 1),
        (
            vec![output("q", None)?, output("q", None)?],
            Duration::from_secs(1),
            1,
        ),
        (vec![output("q", None)?], Duration::ZERO, 1),
        (vec![output("q", None)?], Duration::from_secs(1), 0),
        (vec![output("q", None)?], Duration::from_secs(1), 1025),
    ] {
        assert!(NativeOutputSnapshotsResource::new(outputs, timeout, readers).is_err());
    }
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
        let snapshots = Arc::new(NativeOutputSnapshotsResource::new(
            [OutputSelector::new(
                ComponentId::try_new(QUERY)?,
                Some(PortId::try_new("out")?),
            )],
            Duration::from_millis(250),
            4,
        )?);
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
                "outputs": {QUERY: {"component": QUERY, "port": "out"}},
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
            .insert("output_snapshots".into(), vec![snapshots_id.clone()]);
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
                ResourceRole::OutputCatalogue,
                ResourceHandle::new(ResourceRole::OutputCatalogue, snapshots),
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
        assert_eq!(
            initial.headers()["x-drasi-output-epoch"].len(),
            36,
            "epoch UUID"
        );
        assert!(initial.headers().get("x-drasi-query-sequence").is_none());
        assert_eq!(initial.json::<Value>().await?, json!([]));
        let ingress = format!("http://127.0.0.1:{source_port}/sources/facilities-db/events");
        client
            .post(&ingress)
            .json(&http_event(1))
            .send()
            .await?
            .error_for_status()?;
        let added = events.next().await?;
        assert_eq!(added["output"]["component"], QUERY);
        assert_eq!(added["output"]["port"], "out");
        assert_eq!(added["change"]["operation"], "ADD");
        assert_eq!(added["change"]["after"], json!({"value": 1}));
        let snapshot = client.get(&snapshots).send().await?.error_for_status()?;
        let position: u64 = snapshot.headers()["x-drasi-output-position"]
            .to_str()?
            .parse()?;
        assert!(position > 0);
        let rows = snapshot.json::<Value>().await?;
        let changes = rows.as_array().context("snapshot array")?;
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0]["operation"], "ADD");
        assert_eq!(changes[0]["after"], json!({"value": 1}));
        assert!(changes[0]["identity"]["id"].as_str().is_some());
        for stale in ["row_signature", "rowSignature", "queryId"] {
            assert!(changes[0].get(stale).is_none(), "{stale}");
        }
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
        let updated = client
            .get(&snapshots)
            .send()
            .await?
            .error_for_status()?
            .json::<Value>()
            .await?;
        let updated = updated.as_array().context("snapshot array")?;
        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0]["after"], json!({"value": 2}));
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

async fn open_events(client: &reqwest::Client, port: u16) -> Result<EventReader> {
    tokio::time::timeout(DEADLINE, async {
        loop {
            if let Ok(response) = client
                .get(format!("http://127.0.0.1:{port}/events"))
                .send()
                .await
            {
                if response.status().is_success() {
                    return EventReader {
                        response,
                        pending: String::new(),
                    };
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .context("SSE listener did not accept a subscriber")
}

async fn post_event(
    client: &reqwest::Client,
    source_port: u16,
    source: &str,
    value: i64,
) -> Result<()> {
    tokio::time::timeout(DEADLINE, async {
        loop {
            let sent = client
                .post(format!(
                    "http://127.0.0.1:{source_port}/sources/{source}/events"
                ))
                .json(&http_event(value))
                .send()
                .await;
            if sent.is_ok_and(|response| response.status().is_success()) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .context("source did not accept the event")
}

/// One real loaded SSE sink is constructed from the host-declared incoming edge,
/// then a managed graph rebind removes that edge and declares another producer.
/// The sink must be reconstructed (a new generation) with the new mapping, while
/// an untouched sink on the old producer keeps its instance.
async fn declared_inputs_follow_a_live_rebind(statically_linked: bool) -> Result<()> {
    let core = drasi_lib::DrasiLib::builder()
        .with_id(format!("sse-rebind-{statically_linked}"))
        .build()
        .await?;
    let result = async {
        core.start().await?;
        let graph_id = core
            .computation_control()?
            .desired_snapshot()
            .id
            .to_string();
        let (facilities_port, rooms_port, ui_port, mirror_port) =
            (free_port()?, free_port()?, free_port()?, free_port()?);
        let source_plugin = if statically_linked {
            NativePlugin::from_definition(drasi_computation_source_http::plugin()?)?
        } else {
            plugin(HTTP_SOURCE)?
        };
        let sink_plugin = if statically_linked {
            NativePlugin::from_definition(drasi_computation_sink_sse::plugin()?)?
        } else {
            plugin(SSE_SINK)?
        };
        let source = source_plugin.factories()[0].clone();
        let sink = sink_plugin.factories()[0].clone();
        let id = |name: &str| ComponentId::try_new(name);
        let endpoint = |component: &str, port: &str| -> Result<Endpoint> {
            Ok(Endpoint::new(id(component)?, PortId::try_new(port)?))
        };
        let source_spec = |name: &str, port: u16| {
            source.specification(
                id(name)?,
                json!({"sourceId": name, "stream": name, "host": "127.0.0.1", "port": port}),
            )
        };
        let sink_spec = |name: &str, port: u16| {
            sink.specification(
                id(name)?,
                json!({"host": "127.0.0.1", "port": port, "heartbeatIntervalMs": 25}),
            )
        };
        let batch = ComputationGraph::builder(graph_id.as_str())
            .component(
                source_spec("facilities-db", facilities_port)?,
                source.clone(),
            )
            .component(sink_spec("ui", ui_port)?, sink.clone())
            .component(sink_spec("mirror", mirror_port)?, sink.clone())
            .connect(
                EdgeDefinition::new(endpoint("facilities-db", "out")?, endpoint("ui", "in")?),
                Box::new(BoundedPipeConfig { capacity: 16 }),
            )
            .connect(
                EdgeDefinition::new(endpoint("facilities-db", "out")?, endpoint("mirror", "in")?),
                Box::new(BoundedPipeConfig { capacity: 16 }),
            )
            .bind_stream(endpoint("facilities-db", "out")?, stream("facilities-db"))
            .build_components()?;
        let report = core.add_components(batch).await?;
        assert!(report.committed, "{report:?}");
        let client = reqwest::Client::new();

        // The sink was built from the declared edge, not from configuration.
        let mut ui = open_events(&client, ui_port).await?;
        let mut mirror = open_events(&client, mirror_port).await?;
        post_event(&client, facilities_port, "facilities-db", 1).await?;
        for events in [&mut ui, &mut mirror] {
            let frame = events.next().await?;
            assert_eq!(frame["output"]["alias"], "facilities-db");
            assert_eq!(frame["output"]["component"], "facilities-db");
            assert_eq!(frame["output"]["port"], "out");
            assert_eq!(frame["changes"][0]["operation"], "ADD");
        }
        drop(ui);

        // Rebind: ui now takes a different producer's output instead.
        let control = core.computation_control()?;
        let snapshot = control.desired_snapshot();
        let generation = |name: &str| -> Result<ComponentGeneration> {
            Ok(control.observed().components[&id(name)?].generation)
        };
        let (ui_generation, mirror_generation) = (generation("ui")?, generation("mirror")?);
        let rooms = source_spec("rooms-db", rooms_port)?;
        let rooms_definition = DesiredComponent {
            descriptor: rooms.descriptor.clone(),
            role: ComponentRole::Source,
            completion: rooms.completion,
            streams: BTreeMap::from([(PortId::try_new("out")?, stream("rooms-db"))]),
            lifecycle: snapshot.lifecycle_policies[&id("facilities-db")?].clone(),
            input_merge: InputMergePolicy::Arrival,
            construction: ComponentConstruction::Factory(rooms),
        };
        let preview = control
            .preview(
                snapshot.revision,
                vec![
                    DesiredMutation::Unbind {
                        edge: EdgeDefinition::new(
                            endpoint("facilities-db", "out")?,
                            endpoint("ui", "in")?,
                        ),
                        policy: RemovalPolicy::Reject,
                    },
                    DesiredMutation::PutComponent(rooms_definition),
                    DesiredMutation::Bind(DesiredRelationship {
                        definition: EdgeDefinition::new(
                            endpoint("rooms-db", "out")?,
                            endpoint("ui", "in")?,
                        ),
                        policy: RelationshipPolicy::default(),
                        pipe: DesiredPipe::Bounded { capacity: 16 },
                    }),
                ],
            )
            .await?;
        assert!(preview.replaced().contains(&id("ui")?));
        assert!(!preview.replaced().contains(&id("mirror")?));
        let report = control
            .reconcile(preview, TopologyBindings::default())
            .await?;
        assert!(report.failures.is_empty() && report.committed, "{report:?}");
        assert_ne!(generation("ui")?, ui_generation, "ui was not reconstructed");
        assert_eq!(generation("mirror")?, mirror_generation);

        // The old mapping is gone and the new one is live: only the new
        // producer's data reaches ui, labelled with its real component and port.
        let mut ui = open_events(&client, ui_port).await?;
        post_event(&client, facilities_port, "facilities-db", 2).await?;
        let frame = mirror.next().await?;
        assert_eq!(frame["output"]["component"], "facilities-db");
        post_event(&client, rooms_port, "rooms-db", 3).await?;
        let frame = ui.next().await?;
        assert_eq!(frame["output"]["alias"], "rooms-db");
        assert_eq!(frame["output"]["component"], "rooms-db");
        assert_eq!(frame["output"]["port"], "out");
        assert_eq!(frame["changes"][0]["operation"], "ADD");
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let shutdown = core.shutdown().await;
    result?;
    shutdown?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_sse_declared_inputs_follow_a_live_rebind_on_current_thread() -> Result<()> {
    for statically_linked in [false, true] {
        declared_inputs_follow_a_live_rebind(statically_linked).await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_sse_declared_inputs_follow_a_live_rebind_on_multi_thread() -> Result<()> {
    for statically_linked in [false, true] {
        declared_inputs_follow_a_live_rebind(statically_linked).await?;
    }
    Ok(())
}
