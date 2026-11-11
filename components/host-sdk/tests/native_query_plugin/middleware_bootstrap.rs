// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_host_sdk::computation::{NativeOutputSnapshotsResource, OutputSelector};
use std::net::TcpListener;

async fn transformed_bootstrap() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("source.sqlite");
    create_database(&database)?;
    let core = DrasiLib::builder().with_id(INSTANCE).build().await?;
    core.start().await?;
    let catalogue = core.output_catalogue()?;
    let scope = Scope {
        instance_id: INSTANCE.into(),
        graph_id: catalogue.graph_id().to_string(),
        generation: 1,
    };
    let bootstrap_plugin = computation::load(bootstrap_library())?;
    let bootstrap = bootstrap_plugin
        .bootstrap_factories()
        .iter()
        .find(|factory| {
            factory.metadata().implementation.name.as_ref() == "drasi.sqlite/file-bootstrap"
        })
        .context("missing file bootstrap")?
        .create_query_provider(
            ComponentId::try_new("mapper")?,
            provider_configuration(&database),
            scope.clone(),
            None,
        )?;
    let middleware_plugin = computation::load(library().with_file_name(format!(
        "{}drasi_computation_transformer_middleware{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    )))?;
    let middleware = middleware_plugin
        .factories()
        .iter()
        .find(|factory| {
            factory.metadata().implementation.name.as_ref() == "drasi.standard/middleware"
        })
        .context("missing real middleware factory")?;
    let mapper = middleware.create_component_with_bootstrap(ComponentId::try_new("mapper")?, json!({
        "stream":"mapper/out", "bootstrap":true,
        "middleware":[{"name":"rename","kind":"relabel","config":{"labelMappings":{"items":"ReadyItem"}}}],
        "pipeline":["rename"]
    }), scope.clone(), Arc::new(NativeBootstrapResource(bootstrap.clone())))?;
    assert_ne!(
        mapper.descriptor().semantic_kind(),
        Some(ComponentSemanticKind::Query)
    );
    let mapper = core
        .add_computation_component(
            ComponentAddition::new(ConstructedComponent::transformer(Box::new(mapper)))
                .auto_start(false)
                .bind_stream(PortId::try_new("out")?, StreamId::try_new("mapper/out")?),
        )
        .await?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    let source = core
        .add_computation_component(
            sqlite_live::source(&database, port)?
                .auto_start(false)
                .bind_stream(PortId::try_new("out")?, StreamId::try_new("sqlite/out")?),
        )
        .await?;
    let query_plugin = computation::load(library())?;
    let query = query_plugin
        .factories()
        .iter()
        .find(|factory| {
            factory.metadata().implementation.name.as_ref() == "drasi.experimental/volatile-query"
        })
        .context("missing volatile query")?
        .create_component_with_output_snapshots(
            ComponentId::try_new("downstream")?,
            json!({"query":"MATCH (n:ReadyItem) RETURN sum(n.amount) AS total", "volatile":true,
                "bootstrap":[{"component":"mapper","port":"out"}]}),
            scope,
            Arc::new(NativeOutputSnapshotsResource::new(
                [OutputSelector::new(
                    mapper.id().clone(),
                    Some(PortId::try_new("out")?),
                )],
                Duration::from_secs(5),
                1,
            )?),
            catalogue.clone(),
            vec![InputBinding {
                input_port: PortId::try_new("in")?,
                output: OutputEndpoint::new(mapper.id().clone(), PortId::try_new("out")?),
                stream: StreamId::try_new("mapper/out")?,
            }],
        )?;
    let query = core
        .add_computation_component(
            ComponentAddition::new(ConstructedComponent::transformer(Box::new(query)))
                .auto_start(false)
                .bind_stream(
                    PortId::try_new("out")?,
                    StreamId::try_new("downstream/out")?,
                ),
        )
        .await?;
    let received = Arc::new(Mutex::new(Vec::new()));
    let changed = Arc::new(Notify::new());
    let sink = core
        .add_computation_component(
            ComponentAddition::new(ConstructedComponent::sink(Box::new(Sink {
                descriptor: ComponentDescriptor::try_new(
                    ComponentId::try_new("results")?,
                    vec![super::super::super::port("in", PortDirection::Input)?],
                )?,
                received: received.clone(),
                notification: changed.clone(),
            })))
            .auto_start(false),
        )
        .await?;
    connect(
        &core,
        vec![
            EdgeDefinition::new(
                Endpoint::new(source.id().clone(), PortId::try_new("out")?),
                Endpoint::new(mapper.id().clone(), PortId::try_new("in")?),
            ),
            EdgeDefinition::new(
                Endpoint::new(mapper.id().clone(), PortId::try_new("out")?),
                Endpoint::new(query.id().clone(), PortId::try_new("in")?),
            ),
            EdgeDefinition::new(
                Endpoint::new(query.id().clone(), PortId::try_new("out")?),
                Endpoint::new(sink.id().clone(), PortId::try_new("in")?),
            ),
        ],
    )
    .await?;
    let snapshot = || async {
        Ok::<_, anyhow::Error>(
            catalogue
                .resolve(&OutputEndpoint::new(
                    query.id().clone(),
                    PortId::try_new("out")?,
                ))?
                .snapshot()
                .await?,
        )
    };
    let output = |index: usize| {
        let received = &received;
        let changed = &changed;
        async move {
            loop {
                let notified = changed.notified();
                if let Some(output) = received
                    .lock()
                    .map_err(|_| anyhow::anyhow!("middleware result capture poisoned"))?
                    .get(index)
                    .cloned()
                {
                    return Ok::<_, anyhow::Error>(output);
                }
                notified.await;
            }
        }
    };
    let exercise = async {
        source.start().await?;
        sink.start().await?;
        mapper.start().await?;
        bootstrap.freeze_for_retirement()?.resume();
        query.start().await?;
        assert_snapshot_total(&snapshot().await?, 30.0)?;
        assert!(
            received.lock().expect("result capture poisoned").is_empty(),
            "initialization emitted intermediate results"
        );
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()?;
        let url = format!("http://127.0.0.1:{port}/api/tables/items");
        client
            .put(format!("{url}/1"))
            .json(&json!({"amount":12}))
            .send()
            .await?
            .error_for_status()?;
        output(0).await?;
        assert_snapshot_total(&snapshot().await?, 32.0)?;
        client
            .delete(format!("{url}/2"))
            .send()
            .await?
            .error_for_status()?;
        output(1).await?;
        assert_snapshot_total(&snapshot().await?, 12.0)?;
        let mapped = catalogue
            .resolve(&OutputEndpoint::new(
                mapper.id().clone(),
                PortId::try_new("out")?,
            ))?
            .snapshot()
            .await?;
        assert_eq!(mapped.records().len(), 1);
        assert_eq!(mapped.cursor().sequence(), 2);
        query.stop().await?;
        mapper.stop().await?;
        mapper.start().await?;
        query.start().await?;
        assert_snapshot_total(&snapshot().await?, 12.0)?;
        assert_eq!(received.lock().expect("result capture poisoned").len(), 2);
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let stopped = core.shutdown().await;
    exercise?;
    stopped?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn real_non_query_middleware_bootstrap_reaches_downstream_current_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(30), transformed_bootstrap()).await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_non_query_middleware_bootstrap_reaches_downstream_multi_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(30), transformed_bootstrap()).await?
}
