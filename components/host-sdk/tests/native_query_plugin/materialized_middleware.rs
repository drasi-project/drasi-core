// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_computation_plugin_sdk::Scope;
use drasi_core::computation::InMemoryComputationProvider;
use drasi_lib::DrasiLib;
use tokio::sync::mpsc;

struct Feed(ComponentDescriptor, mpsc::Receiver<InputEnvelope>);

#[async_trait]
impl ComputationComponent for Feed {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.0
    }
    fn description(&self) -> ComponentDescription {
        ComponentDescription::undeclared()
    }
    drasi_lib::passive_component_lifecycle!();
}

#[async_trait]
impl EnvelopeSource for Feed {
    async fn next(&mut self) -> Result<Option<OutputEnvelope>> {
        let port = PortId::try_new("out")?;
        Ok(self.1.recv().await.map(|input| OutputEnvelope {
            port,
            envelope: input.envelope,
            execution_context: input.execution_context,
        }))
    }
}

async fn assert_rows(
    catalogue: &OutputCatalogue,
    output: &ComponentHandle,
    sequence: u64,
    label: &str,
    expected: &[i64],
) -> Result<OutputCursor> {
    let endpoint = OutputEndpoint::new(output.id().clone(), PortId::try_new("out")?);
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let snapshot = catalogue.resolve(&endpoint)?.snapshot().await?;
            if snapshot.cursor().sequence() < sequence {
                tokio::time::sleep(Duration::from_millis(10)).await;
                continue;
            }
            assert_eq!(snapshot.cursor().sequence(), sequence, "{endpoint:?}");
            let mut values = Vec::new();
            for record in snapshot.records().values() {
                let row = QueryChangeCodec::decode_row(record)?;
                assert_eq!(row.label, label);
                values.push(row.values["value"].as_i64().context("integer result")?);
            }
            values.sort_unstable();
            assert_eq!(values, expected, "{endpoint:?}");
            return Ok(snapshot.cursor().clone());
        }
    })
    .await
    .with_context(|| format!("waiting for {endpoint:?} at {sequence}"))?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn loaded_query_middleware_query_hands_off_materialized_rows_and_live_changes() -> Result<()>
{
    middleware_chain(true, false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn loaded_stateless_middleware_projects_pinned_reads_and_hands_off_its_own_cut() -> Result<()>
{
    middleware_chain(false, false).await
}

#[tokio::test(flavor = "current_thread")]
async fn unconnected_loaded_query_starts_then_initializes_on_its_first_input() -> Result<()> {
    middleware_chain(false, true).await
}

async fn mapped_rows(mut read: CurrentStateRead, expected: &[i64]) -> Result<()> {
    let mut values = Vec::new();
    while let Some(batch) = read.next_batch().await? {
        for operation in batch.changes().operations() {
            let ChangeOperation::Added { after, .. } = operation else {
                anyhow::bail!("projected state must consist of additions");
            };
            let row = QueryChangeCodec::decode_row(after)?;
            assert_eq!(row.label, "Mapped");
            values.push(row.values["value"].as_i64().context("integer result")?);
        }
    }
    values.sort_unstable();
    assert_eq!(values, expected);
    assert!(read.into_follow().is_err());
    Ok(())
}

async fn middleware_chain(materialized: bool, late_connection: bool) -> Result<()> {
    let core = DrasiLib::builder()
        .with_id("bounded-middleware")
        .with_allow_external_component_ids(true)
        .build()
        .await?;
    core.start().await?;
    let catalogue = core.output_catalogue()?;
    let scope = Scope {
        instance_id: "bounded-middleware".into(),
        graph_id: catalogue.graph_id().to_string(),
        generation: 1,
    };
    let query_library = computation::load(library())?;
    let factory = query_library
        .factories()
        .iter()
        .find(|f| f.metadata().implementation.name.as_ref() == "drasi.query/indexed")
        .context("indexed query factory")?;
    let query = |id, name_: &str, label, bootstrap| -> Result<ComponentAddition> {
        let stream = format!("{name_}/out");
        let component = factory.create_component_with_processing(
            ComponentId::try_new(id)?,
            json!({"query":format!("MATCH (n:{label}) RETURN n.value AS value"),
                "stream":stream, "volatile":true, "bootstrap":bootstrap}),
            scope.clone(),
            Arc::new(InMemoryComputationProvider),
        )?;
        Ok(ComponentAddition::new(
            name(name_)?,
            ConstructedComponent::transformer(Box::new(component)),
        )
        .auto_start(false)
        .bind_stream(PortId::try_new("out")?, StreamId::try_new(stream)?))
    };
    let upstream = core
        .add_computation_component(query(
            "1b255488-31b6-4f6f-b4da-86d027967a01",
            "upstream",
            "Item",
            false,
        )?)
        .await?;
    let downstream = core
        .add_computation_component(query(
            "1b255488-31b6-4f6f-b4da-86d027967a02",
            "downstream",
            "Mapped",
            true,
        )?)
        .await?;
    let mapper_library = computation::load(library().with_file_name(format!(
        "{}drasi_computation_transformer_middleware{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    )))?;
    // Materialized state is the stateful middleware, the same transformer
    // runtime as the queries around it; stateless middleware projects reads.
    let kind = if materialized {
        "drasi.standard/stateful-middleware"
    } else {
        "drasi.standard/middleware"
    };
    let mapper_factory = mapper_library
        .factories()
        .iter()
        .find(|f| f.metadata().implementation.name.as_ref() == kind)
        .context("native middleware factory")?;
    let mapper_id = ComponentId::try_new("1b255488-31b6-4f6f-b4da-86d027967a03")?;
    let mut mapper_configuration = json!({"stream":"mapper/out", "middleware":[
        {"name":"rename","kind":"relabel","config":{"labelMappings":{"Result":"Mapped"}}}
    ], "pipeline":["rename"]});
    let mapper = if materialized {
        mapper_configuration["bootstrap"] = json!(true);
        mapper_configuration["volatile"] = json!(true);
        ComponentAddition::new(
            name("mapper")?,
            ConstructedComponent::transformer(Box::new(
                mapper_factory.create_component_with_processing(
                    mapper_id,
                    mapper_configuration,
                    scope.clone(),
                    Arc::new(InMemoryComputationProvider),
                )?,
            )),
        )
    } else {
        ComponentAddition::from_specification(
            name("mapper")?,
            mapper_factory.specification(mapper_id, mapper_configuration)?,
            mapper_factory.clone(),
        )
    };
    let mapper = core
        .add_computation_component(
            mapper
                .auto_start(false)
                .bind_stream(PortId::try_new("out")?, StreamId::try_new("mapper/out")?),
        )
        .await?;
    let (send, receive) = mpsc::channel(8);
    let feed = core
        .add_computation_component(
            ComponentAddition::new(
                name("feed")?,
                ConstructedComponent::source(Box::new(Feed(
                    ComponentDescriptor::try_new(
                        ComponentId::try_new("1b255488-31b6-4f6f-b4da-86d027967a04")?,
                        vec![port("out", PortDirection::Output)?],
                    )?,
                    receive,
                ))),
            )
            .auto_start(false)
            .bind_stream(PortId::try_new("out")?, StreamId::try_new("source/out")?),
        )
        .await?;
    let edge = |from: &ComponentHandle, to: &ComponentHandle| -> Result<EdgeDefinition> {
        Ok(EdgeDefinition::new(
            Endpoint::new(from.id().clone(), PortId::try_new("out")?),
            Endpoint::new(to.id().clone(), PortId::try_new("in")?),
        ))
    };
    // Only data connections. Both state lanes must be established by the binder.
    let mut connections = vec![edge(&feed, &upstream)?, edge(&upstream, &mapper)?];
    if !late_connection {
        connections.push(edge(&mapper, &downstream)?);
    }
    bootstrap::connect_with_capacity(&core, connections, 8).await?;
    let run = async {
        if late_connection {
            downstream
                .start()
                .await
                .context("start with no configured connections")?;
            assert_rows(&catalogue, &downstream, 0, "Result", &[]).await?;
        }
        upstream.start().await?;
        feed.start().await?;
        let insert = |id, v, seq| {
            input(
                SourceChange::Insert {
                    element: item_with_id(id, v, seq),
                },
                seq,
            )
        };
        let replace = |id, v, seq| {
            input(
                SourceChange::Replace {
                    element: item_with_id(id, v, seq),
                },
                seq,
            )
        };
        let delete = |id, seq| {
            input(
                SourceChange::Delete {
                    metadata: item_with_id(id, 0, seq).get_metadata().clone(),
                },
                seq,
            )
        };
        for change in [
            insert("a", 1, 1)?,
            insert("b", 2, 2)?,
            replace("a", 3, 3)?,
            delete("b", 4)?,
        ] {
            send.send(change).await?;
        }
        assert_rows(&catalogue, &upstream, 4, "Result", &[3]).await?;
        mapper.start().await?;
        let endpoint = OutputEndpoint::new(mapper.id().clone(), PortId::try_new("out")?);
        let stalled = if materialized {
            let baseline = assert_rows(&catalogue, &mapper, 0, "Mapped", &[3]).await?;
            assert!(!baseline.producer().persistent());
            None
        } else {
            let handle = catalogue.resolve(&endpoint)?;
            assert!(!handle.provides_snapshot() && !handle.provides_replay());
            let stream = handle.current_state(Default::default()).await?;
            let cut = stream.cursor().context("independently translated cut")?;
            assert_eq!(cut.producer().component_id(), mapper.id());
            assert_eq!(cut.sequence(), 4);
            assert!(!cut.producer().persistent());
            let file = handle
                .current_state(CurrentStateOptions {
                    transfer: StateTransfer::TemporaryFile {
                        max_file_bytes: 65536.try_into()?,
                    },
                    ..Default::default()
                })
                .await?;
            assert_eq!(file.cursor(), Some(cut));
            Some((stream, file))
        };
        send.send(insert("c", 4, 5)?).await?;
        if materialized {
            assert_rows(&catalogue, &mapper, 1, "Mapped", &[3, 4]).await?;
        } else {
            assert_rows(&catalogue, &upstream, 5, "Result", &[3, 4]).await?;
        }
        if late_connection {
            bootstrap::connect_with_capacity(&core, vec![edge(&mapper, &downstream)?], 8).await?;
            downstream.wait_started().await?;
        } else {
            downstream.start().await?;
        }
        assert_rows(&catalogue, &downstream, 0, "Result", &[3, 4]).await?;
        send.send(replace("a", 9, 6)?).await?;
        assert_rows(&catalogue, &downstream, 1, "Result", &[4, 9]).await?;
        send.send(delete("c", 7)?).await?;
        assert_rows(&catalogue, &downstream, 2, "Result", &[9]).await?;
        if let Some((stream, file)) = stalled {
            mapped_rows(stream, &[3]).await?;
            mapped_rows(file, &[3]).await?;
            mapped_rows(
                catalogue
                    .resolve(&endpoint)?
                    .current_state(Default::default())
                    .await?,
                &[9],
            )
            .await?;
        } else {
            assert_rows(&catalogue, &mapper, 3, "Mapped", &[9]).await?;
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    drop(send);
    let stopped = core.shutdown().await;
    run?;
    stopped?;
    Ok(())
}
