// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_computation_plugin_sdk::Scope;
use drasi_host_sdk::computation::{NativeBootstrapProxy, NativeBootstrapResource};
use std::path::Path;

const INSTANCE: &str = "native-provider-query";

#[path = "middleware_bootstrap.rs"]
mod middleware;
#[path = "sqlite_live.rs"]
mod sqlite_live;

pub(crate) fn bootstrap_library() -> PathBuf {
    library().with_file_name(format!(
        "{}drasi_computation_bootstrap_sqlite{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    ))
}

pub(crate) fn create_database(path: &Path) -> Result<()> {
    let connection = rusqlite::Connection::open(path)?;
    connection.execute_batch(
        "CREATE TABLE items (id INTEGER PRIMARY KEY, amount INTEGER NOT NULL);
         INSERT INTO items VALUES (1, 10), (2, 20);",
    )?;
    Ok(())
}

pub(crate) fn provider_configuration(path: &Path) -> serde_json::Value {
    json!({
        "source": "sqlite",
        "stream": "sqlite/out",
        "settings": {
            "path": path,
            "tables": ["items"],
            "max_tables": 4,
            "max_bytes": 65536,
            "max_duration_ms": 5000,
            "output_capacity": 1,
            "shutdown_timeout_ms": 5000
        }
    })
}

struct IdleLiveInput(ComponentDescriptor);

#[async_trait]
impl ComputationComponent for IdleLiveInput {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.0
    }
    async fn start(&mut self) -> Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSource for IdleLiveInput {
    async fn next(&mut self) -> Result<Option<OutputEnvelope>> {
        std::future::pending().await
    }
}

struct ProviderFixture {
    core: DrasiLib,
    factory: Arc<NativeFactory>,
    provider: Arc<NativeBootstrapProxy>,
    scope: Scope,
    query: ComponentHandle,
    live: ComponentHandle,
    capture: ComponentHandle,
    received: Arc<Mutex<Vec<ChangeEnvelope>>>,
    notification: Arc<Notify>,
}

impl ProviderFixture {
    async fn new(path: &Path, text: &str) -> Result<Self> {
        Self::with_live_input(path, text, None).await
    }

    async fn with_live_input(
        path: &Path,
        text: &str,
        live: Option<ComponentAddition>,
    ) -> Result<Self> {
        let query_plugin = computation::load(library())?;
        let factory = query_plugin
            .factories()
            .iter()
            .find(|factory| {
                factory.metadata().implementation.name.as_ref()
                    == "drasi.experimental/provider-query"
            })
            .context("provider query factory missing")?
            .clone();
        let bootstrap_plugin = computation::load(bootstrap_library())?;
        assert!(bootstrap_plugin.factories().is_empty());
        let bootstrap_factory = bootstrap_plugin
            .bootstrap_factories()
            .iter()
            .find(|factory| {
                factory.metadata().implementation.name.as_ref() == "drasi.sqlite/file-bootstrap"
            })
            .context("existing native SQLite file-bootstrap factory missing")?;
        let core = DrasiLib::builder().with_id(INSTANCE).build().await?;
        core.start().await?;
        let scope = Scope {
            instance_id: INSTANCE.into(),
            graph_id: core.output_catalogue()?.graph_id().to_string(),
            generation: 1,
        };
        let id = ComponentId::try_new("query")?;
        let provider = bootstrap_factory.create_query_provider(
            id.clone(),
            provider_configuration(path),
            scope.clone(),
            None,
        )?;
        let resource = ResourceId::try_new("file-bootstrap")?;
        let mut specification =
            factory.specification(id, json!({"query": text, "volatile": true}))?;
        assert!(factory.validate(&specification).is_err());
        specification
            .dependencies
            .insert("bootstrap".into(), vec![resource.clone()]);
        let query = core
            .add_computation_component(
                ComponentAddition::from_specification(specification, factory.clone())
                    .auto_start(false)
                    .bind_stream(PortId::try_new("out")?, StreamId::try_new("query/out")?)
                    .with_resource(
                        ResourceSpecification {
                            id: resource,
                            role: ResourceRole::Bootstrap,
                            ownership: ResourceOwnership::Graph,
                            binding: "bootstrap".into(),
                        },
                        ResourceHandle::new(
                            ResourceRole::Bootstrap,
                            Arc::new(NativeBootstrapResource(provider.clone())),
                        )
                        .with_cleanup(provider.clone()),
                    ),
            )
            .await?;
        let live = match live {
            Some(live) => live,
            None => ComponentAddition::new(ConstructedComponent::source(Box::new(IdleLiveInput(
                ComponentDescriptor::try_new(
                    ComponentId::try_new("sqlite")?,
                    vec![port("out", PortDirection::Output)?],
                )?,
            )))),
        };
        let live = core
            .add_computation_component(
                live.auto_start(false)
                    .bind_stream(PortId::try_new("out")?, StreamId::try_new("sqlite/out")?),
            )
            .await?;
        let received = Arc::new(Mutex::new(Vec::new()));
        let notification = Arc::new(Notify::new());
        let capture = core
            .add_computation_component(
                ComponentAddition::new(ConstructedComponent::sink(Box::new(Sink {
                    descriptor: ComponentDescriptor::try_new(
                        ComponentId::try_new("results")?,
                        vec![port("in", PortDirection::Input)?],
                    )?,
                    received: received.clone(),
                    notification: notification.clone(),
                })))
                .auto_start(false),
            )
            .await?;
        connect(
            &core,
            vec![
                EdgeDefinition::new(
                    Endpoint::new(live.id().clone(), PortId::try_new("out")?),
                    Endpoint::new(query.id().clone(), PortId::try_new("in")?),
                ),
                EdgeDefinition::new(
                    Endpoint::new(query.id().clone(), PortId::try_new("out")?),
                    Endpoint::new(capture.id().clone(), PortId::try_new("in")?),
                ),
            ],
        )
        .await?;
        tokio::time::timeout(Duration::from_secs(10), query.wait_created()).await??;
        Ok(Self {
            core,
            factory,
            provider,
            scope,
            query,
            live,
            capture,
            received,
            notification,
        })
    }

    async fn start(&self) -> Result<()> {
        self.capture.start().await?;
        self.live.start().await?;
        tokio::time::timeout(Duration::from_secs(10), self.query.start()).await??;
        Ok(())
    }

    async fn snapshot(&self) -> Result<OutputSnapshot> {
        Ok(self
            .core
            .output_catalogue()?
            .resolve(&OutputEndpoint::new(
                self.query.id().clone(),
                PortId::try_new("out")?,
            ))?
            .snapshot()
            .await?)
    }

    fn assert_provider_stopped(&self) -> Result<()> {
        self.provider.freeze_for_retirement()?.resume();
        Ok(())
    }

    async fn output(&self, count: usize) -> Result<ChangeEnvelope> {
        loop {
            let changed = self.notification.notified();
            if let Some(output) = self
                .received
                .lock()
                .map_err(|_| anyhow::anyhow!("provider result capture poisoned"))?
                .get(count - 1)
                .cloned()
            {
                assert_eq!(output.system().sequence(), count as u64);
                return Ok(output);
            }
            changed.await;
        }
    }
}

async fn real_file_bootstrap() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("items.db");
    create_database(&path)?;
    let fixture =
        ProviderFixture::new(&path, "MATCH (n:items) RETURN sum(n.amount) AS total").await?;
    let exercise = async {
        assert!(fixture.snapshot().await.is_err());
        fixture.start().await?;
        let initial = fixture.snapshot().await?;
        assert_snapshot_total(&initial, 30.0)?;
        assert_eq!(initial.cursor().sequence(), 0);
        assert!(!initial.cursor().producer().persistent());
        fixture.assert_provider_stopped()?;
        assert!(fixture
            .received
            .lock()
            .map_err(|_| anyhow::anyhow!("provider result capture poisoned"))?
            .is_empty());
        fixture.query.stop().await?;
        assert!(fixture.snapshot().await.is_err());
        {
            let connection = rusqlite::Connection::open(&path)?;
            connection.execute("UPDATE items SET amount = amount + 10", [])?;
        }
        fixture.query.start().await?;
        let restarted = fixture.snapshot().await?;
        assert_snapshot_total(&restarted, 50.0)?;
        assert_eq!(restarted.cursor().sequence(), 0);
        assert_ne!(restarted.cursor().epoch(), initial.cursor().epoch());
        fixture.assert_provider_stopped()?;
        Ok::<(), anyhow::Error>(())
    };
    let result = tokio::time::timeout(Duration::from_secs(15), exercise).await;
    fixture.core.shutdown().await?;
    result??;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn existing_sqlite_provider_initializes_loaded_query_in_graph_current_thread() -> Result<()> {
    real_file_bootstrap().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn existing_sqlite_provider_initializes_loaded_query_in_graph_multi_thread() -> Result<()> {
    real_file_bootstrap().await
}

#[tokio::test(flavor = "current_thread")]
async fn missing_sqlite_file_is_an_error_and_cleanup_allows_retry() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("missing.db");
    let fixture =
        ProviderFixture::new(&path, "MATCH (n:items) RETURN sum(n.amount) AS total").await?;
    let exercise = async {
        assert!(fixture.start().await.is_err());
        assert!(!path.exists());
        assert!(fixture.snapshot().await.is_err());
        fixture.assert_provider_stopped()?;
        fixture.query.stop().await?;
        create_database(&path)?;
        fixture.query.start().await?;
        assert_snapshot_total(&fixture.snapshot().await?, 30.0)?;
        fixture.assert_provider_stopped()?;
        Ok::<(), anyhow::Error>(())
    };
    let result = tokio::time::timeout(Duration::from_secs(15), exercise).await;
    fixture.core.shutdown().await?;
    result??;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn failed_consumer_cancels_and_stops_the_real_sqlite_stream() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("invalid-value.db");
    create_database(&path)?;
    {
        let mut connection = rusqlite::Connection::open(&path)?;
        let transaction = connection.transaction()?;
        transaction.execute("UPDATE items SET amount = 'not-a-number' WHERE id = 2", [])?;
        for id in 3..=128 {
            transaction.execute("INSERT INTO items VALUES (?1, 20)", [id])?;
        }
        transaction.commit()?;
    }
    let fixture =
        ProviderFixture::new(&path, "MATCH (n:items) RETURN abs(n.amount) AS value").await?;
    let exercise = async {
        assert!(fixture.start().await.is_err());
        assert!(fixture.snapshot().await.is_err());
        fixture.assert_provider_stopped()?;
        fixture.query.stop().await?;
        {
            let connection = rusqlite::Connection::open(&path)?;
            connection.execute("UPDATE items SET amount = 20 WHERE id = 2", [])?;
        }
        fixture.query.start().await?;
        assert_eq!(fixture.snapshot().await?.records().len(), 128);
        fixture.assert_provider_stopped()?;
        Ok::<(), anyhow::Error>(())
    };
    let result = tokio::time::timeout(Duration::from_secs(15), exercise).await;
    fixture.core.shutdown().await?;
    result??;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn provider_scope_and_required_binding_are_checked() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("not-opened.db");
    let fixture = ProviderFixture::new(&path, "MATCH (n:items) RETURN n.amount AS value").await?;
    assert!(!path.exists());
    let configuration = json!({"query":"MATCH (n:items) RETURN n.amount AS value","volatile":true});
    let mut unbound = fixture
        .factory
        .create_component(ComponentId::try_new("unbound")?, configuration.clone())?;
    assert!(unbound.start().await.is_err());
    assert!(fixture
        .factory
        .create_component_with_bootstrap(
            ComponentId::try_new("another-query")?,
            configuration,
            fixture.scope.clone(),
            Arc::new(NativeBootstrapResource(fixture.provider.clone())),
        )
        .is_err());
    assert!(!path.exists());
    unbound.stop().await?;
    fixture.core.shutdown().await?;
    Ok(())
}

struct UnsupportedHandover {
    mode: u8,
    stopped: AtomicBool,
}

impl UnsupportedHandover {
    fn watermark() -> BootstrapWatermark {
        BootstrapWatermark {
            stream: StreamId::try_new("source/out").expect("constant stream"),
            source_id: Some("source".into()),
            sequence: 17,
            position: None,
            producer: None,
        }
    }
}

#[async_trait]
impl ComputationBootstrapProvider for UnsupportedHandover {
    async fn snapshot(&self) -> Result<ComputationBootstrapSnapshot> {
        Ok(ComputationBootstrapSnapshot {
            changes: Box::pin(futures_util::stream::empty()),
            watermarks: if self.mode == 0 {
                vec![Self::watermark()]
            } else {
                Vec::new()
            },
        })
    }
    async fn complete_snapshot(&self) -> Result<Vec<BootstrapWatermark>> {
        Ok(if self.mode == 1 {
            vec![Self::watermark()]
        } else {
            Vec::new()
        })
    }
    fn completion_state(&self) -> Result<Option<bytes::Bytes>> {
        Ok((self.mode == 2).then(|| bytes::Bytes::from_static(b"requires-persistence")))
    }
    async fn stop(&self) -> Result<()> {
        self.stopped.store(true, Ordering::Release);
        Ok(())
    }
}

#[tokio::test(flavor = "current_thread")]
async fn quiescent_consumer_rejects_handover_metadata_instead_of_discarding_it() -> Result<()> {
    let plugin = computation::load(library())?;
    let factory = plugin
        .factories()
        .iter()
        .find(|factory| {
            factory.metadata().implementation.name.as_ref() == "drasi.experimental/provider-query"
        })
        .context("provider query factory missing")?;
    for mode in 0..3 {
        let provider = Arc::new(UnsupportedHandover {
            mode,
            stopped: AtomicBool::new(false),
        });
        let mut query = factory.create_component_with_bootstrap(
            ComponentId::try_new("query")?,
            json!({"query":"MATCH (n:items) RETURN n.amount AS value","volatile":true}),
            Scope {
                instance_id: INSTANCE.into(),
                graph_id: INSTANCE.into(),
                generation: 1,
            },
            Arc::new(NativeBootstrapResource(provider.clone())),
        )?;
        assert!(query.start().await.is_err(), "handover mode {mode}");
        assert!(provider.stopped.load(Ordering::Acquire));
        assert!(snapshot_reader(&query)?.snapshot().await.is_err());
        query.stop().await?;
    }
    Ok(())
}
