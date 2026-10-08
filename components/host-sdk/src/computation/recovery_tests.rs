// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use anyhow::{Context, Result};
use drasi_computation_plugin_sdk::{self as sdk, Factory};
use drasi_core::interface::{FailureMode, StorageDurability};
use drasi_index_rocksdb::{
    computation::RocksDbComputationProvider, RocksDbMemoryBudget, RocksIndexOptions,
};
use futures_util::poll;
use std::{
    num::{NonZeroU64, NonZeroUsize},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::Duration,
};

const GRAPH: &str = "native-recovery";
const TIMEOUT: Duration = Duration::from_secs(10);

fn id(value: &str) -> ComponentId {
    ComponentId::try_new(value).unwrap()
}
fn stream(value: &str) -> StreamId {
    StreamId::try_new(value).unwrap()
}
fn size(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).unwrap()
}
fn owner() -> Arc<QuerySourceProgress> {
    Arc::new(QuerySourceProgress::new(GRAPH, id("query")).unwrap())
}
fn limits() -> SourceTransactionLimits {
    SourceTransactionLimits {
        max_changes: size(64),
        max_bytes: size(1 << 20),
        max_duration_ms: NonZeroU64::new(5000).unwrap(),
    }
}
fn request(factory: &NativeFactory, configuration: serde_json::Value) -> CreateRequest {
    CreateRequest {
        id: id("source"),
        implementation: factory.metadata.implementation.clone(),
        configuration_version: factory.metadata.configuration_version,
        configuration,
        scope: Some(Scope {
            instance_id: GRAPH.into(),
            graph_id: GRAPH.into(),
            generation: 1,
        }),
    }
}

pub(crate) fn sqlite_plugin() -> Result<Arc<super::super::NativePlugin>> {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let path = if let Some(path) = std::env::var_os("DRASI_NATIVE_RECOVERY_PLUGIN") {
        path.into()
    } else {
        let target = match std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from) {
            Some(path) if path.is_absolute() => path,
            Some(path) => workspace.join(path),
            None => workspace.join("target"),
        };
        target.join("debug/examples").join(format!(
            "{}native_recovery{}",
            std::env::consts::DLL_PREFIX,
            std::env::consts::DLL_SUFFIX
        ))
    };
    anyhow::ensure!(
        path.is_file(),
        "missing fixture {}; first run CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=3 cargo build -p drasi-host-sdk --example native_recovery",
        path.display()
    );
    super::super::load(path)
}
fn sqlite_configuration(path: &Path) -> serde_json::Value {
    serde_json::json!({
        "stream": "changes",
        "settings": {
            "path": path.join("source.db"), "tables": [], "output": "transactions",
            "replay": {"max_transactions":8, "max_bytes":8 << 20},
            "transactions":limits(), "max_sql_bytes":65536,
            "command_capacity":4, "output_capacity":2, "shutdown_timeout_ms":5000,
        },
    })
}
async fn query(
    path: &Path,
    progress: Arc<QuerySourceProgress>,
) -> Result<ContinuousQueryTransformer> {
    ContinuousQueryTransformer::new_configured(
        ContinuousQueryDefinition {
            graph_id: GRAPH.into(),
            id: id("query"),
            query: "MATCH (n:items) RETURN n.id AS id, n.value AS value".into(),
            language: ComputationQueryLanguage::Cypher,
            output_stream: stream("results"),
            outbox_capacity: size(8),
        },
        Arc::new(RocksDbComputationProvider::new(
            path.join("query"),
            RocksIndexOptions::new(
                false,
                false,
                RocksDbMemoryBudget::from_total_budget_bytes(32 << 20)?,
            ),
        )),
        QueryOptions {
            recovery: QueryRecoveryPolicy::Strict,
            ..Default::default()
        },
        QueryExecutionSettings {
            source_transactions: Some(limits()),
            ..Default::default()
        },
        None,
    )
    .await?
    .with_source_progress(progress)
}
fn command(kind: &str, payload: serde_json::Value) -> Result<Vec<u8>> {
    wire::encode(&sdk::ControlMessage {
        from: id("query"),
        generation: 1,
        direction: sdk::ControlDirection::Upstream,
        notification: sdk::ControlNotification::Custom {
            kind: kind.into(),
            payload,
        },
    })
}
async fn sql(source: &NativeComponentProxy, text: &str) -> Result<()> {
    source
        .inner
        .unit(
            abi::operation::CONTROL,
            &command("fixture.sql", text.into())?,
        )
        .await
}
async fn next(source: &mut NativeComponentProxy) -> Result<OutputEnvelope> {
    tokio::time::timeout(TIMEOUT, source.next())
        .await??
        .context("SQLite source ended")
}

#[tokio::test(flavor = "current_thread")]
async fn native_recovery_sqlite_replays_through_a_separate_binary_and_real_query_commit(
) -> Result<()> {
    let plugin = sqlite_plugin()?;
    let factory = &plugin.factories()[0];
    let directory = tempfile::tempdir()?;
    let progress = owner();
    let mut consumer = query(directory.path(), progress.clone()).await?;
    consumer.start().await?;
    let mut source = factory.construct(
        request(factory, sqlite_configuration(directory.path())),
        None,
        Some(progress.clone()),
    )?;
    assert!(Arc::ptr_eq(&source.recovery_progress().unwrap(), &progress));
    assert!(source.admission().is_none());
    source.start().await?;
    sql(
        &source,
        "CREATE TABLE items(id INTEGER PRIMARY KEY, value TEXT)",
    )
    .await?;
    sql(
        &source,
        "INSERT INTO items VALUES(1,'first'); UPDATE items SET value='final';
        INSERT INTO items VALUES(2,'temporary'); DELETE FROM items WHERE id=2;",
    )
    .await?;
    let original = next(&mut source).await?;
    assert_eq!(
        SourceTransactionCodec::decode(&original.envelope, limits())?
            .into_changes()
            .len(),
        4
    );
    assert!(progress.snapshot().checkpoints.is_empty());
    source.stop().await?;
    let retained = source.inner.clone();
    drop(source);
    let error = retained
        .unit(
            abi::operation::CONTROL,
            &command("fixture.progress", serde_json::Value::Null)?,
        )
        .await
        .expect_err("old callbacks must be revoked");
    assert_eq!(
        error
            .downcast_ref::<sdk::transport::Failure>()
            .unwrap()
            .code,
        abi::status::CLOSED
    );
    drop(retained);
    consumer.stop().await?;
    drop(consumer);
    drop(progress);

    let progress = owner();
    let mut consumer = query(directory.path(), progress.clone()).await?;
    consumer.start().await?;
    let mut source = factory.construct(
        request(factory, sqlite_configuration(directory.path())),
        None,
        Some(progress.clone()),
    )?;
    source.start().await?;
    let replay = next(&mut source).await?;
    assert_eq!(
        original.envelope.system().source_position(),
        replay.envelope.system().source_position()
    );
    let duplicate = replay.envelope.clone();
    let output = consumer
        .transform(InputEnvelope {
            port: PortId::try_new("in")?,
            envelope: replay.envelope,
        })
        .await?;
    consumer.delivery_completed(&output).await?;
    assert_eq!(
        output
            .iter()
            .map(|output| output.envelope.changes().operations().len())
            .sum::<usize>(),
        1
    );
    let rows = consumer
        .results()
        .snapshot()?
        .rows
        .values()
        .map(|row| {
            Ok(QueryChangeCodec::row_values_to_json(
                &QueryChangeCodec::decode_row(row)?.values,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    assert_eq!(rows, [serde_json::json!({"id":1, "value":"final"})]);
    assert!(consumer
        .transform(InputEnvelope {
            port: PortId::try_new("in")?,
            envelope: duplicate
        })
        .await?
        .is_empty());
    source
        .inner
        .unit(
            abi::operation::CONTROL,
            &command("fixture.flush", serde_json::Value::Null)?,
        )
        .await?;
    source.stop().await?;
    drop(source);
    let journal: (u64, u64, u64) = rusqlite::Connection::open(directory.path().join("source.db"))?
        .query_row(
            "SELECT head, retired, bytes FROM __drasi_native_state_v1 WHERE singleton=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
    assert_eq!(journal, (1, 1, 0));
    consumer.stop().await?;
    drop(consumer);
    drop(progress);

    let progress = owner();
    let mut consumer = query(directory.path(), progress.clone()).await?;
    consumer.start().await?;
    let mut source = factory.construct(
        request(factory, sqlite_configuration(directory.path())),
        None,
        Some(progress),
    )?;
    source.start().await?;
    assert!(
        tokio::time::timeout(Duration::from_millis(50), source.next())
            .await
            .is_err()
    );
    sql(
        &source,
        "UPDATE items SET value='after-recovery' WHERE id=1",
    )
    .await?;
    let output = consumer
        .transform(InputEnvelope {
            port: PortId::try_new("in")?,
            envelope: next(&mut source).await?.envelope,
        })
        .await?;
    consumer.delivery_completed(&output).await?;
    assert!(!output.is_empty());
    source.stop().await?;
    consumer.stop().await?;
    Ok(())
}

struct Sink(ComponentDescriptor);
#[async_trait]
impl ComputationComponent for Sink {
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
impl EnvelopeSink for Sink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, _: InputEnvelope) -> Result<()> {
        Ok(())
    }
}
fn sink(name: &str, schema: Arc<Schema>) -> Result<Sink> {
    Ok(Sink(ComponentDescriptor::try_new(
        id(name),
        vec![PortDescriptor::new(
            PortId::try_new("in")?,
            PortDirection::Input,
            schema.descriptor().clone(),
            PipeRequirements::default(),
        )],
    )?))
}
fn endpoint(component: &str, port: &str) -> Endpoint {
    Endpoint::new(id(component), PortId::try_new(port).unwrap())
}

#[tokio::test(flavor = "current_thread")]
async fn native_recovery_graph_requires_the_actual_host_owner_not_matching_names() -> Result<()> {
    let plugin = sqlite_plugin()?;
    let factory = &plugin.factories()[0];
    for substitute in [false, true] {
        let directory = tempfile::tempdir()?;
        let progress = owner();
        let bound = if substitute {
            owner()
        } else {
            progress.clone()
        };
        let source = factory.construct(
            request(factory, sqlite_configuration(directory.path())),
            None,
            Some(bound),
        )?;
        let graph = ComputationGraph::builder(GRAPH)
            .source(Box::new(source))
            .query(Box::new(query(directory.path(), progress).await?))
            .sink(Box::new(sink("sink", QueryChangeCodec::schema())?))
            .bind_stream(endpoint("source", "out"), stream("changes"))
            .bind_stream(endpoint("query", "out"), stream("results"))
            .connect(
                EdgeDefinition::new(endpoint("source", "out"), endpoint("query", "in")),
                Box::new(BoundedPipeConfig { capacity: 1 }),
            )
            .connect(
                EdgeDefinition::new(endpoint("query", "out"), endpoint("sink", "in")),
                Box::new(BoundedPipeConfig { capacity: 1 }),
            )
            .build()?;
        let report = graph.recovery_report(&RecoveryRequirement {
            consumer: id("query"),
            scope: RecoveryScope::Failure(FailureMode::ProcessRestart),
            guarantees: [RecoveryGuarantee::Replay].into(),
        })?;
        assert_eq!(report.satisfied(), !substitute, "{report:?}");
        assert_eq!(
            report
                .issues
                .iter()
                .any(|issue| issue.reason == RecoveryIncompatibility::MismatchedProgressResource),
            substitute
        );
        assert!(!directory.path().join("source.db").exists());
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_recovery_cancelled_start_retains_no_sqlite_worker_and_rejects_absent_service(
) -> Result<()> {
    let plugin = sqlite_plugin()?;
    let factory = &plugin.factories()[0];
    let directory = tempfile::tempdir()?;
    assert!(factory
        .create_component(id("source"), sqlite_configuration(directory.path()))
        .is_err());
    let mut source = factory.construct(
        request(factory, sqlite_configuration(directory.path())),
        None,
        Some(owner()),
    )?;
    let mut starting = Box::pin(source.start());
    assert!(poll!(starting.as_mut()).is_pending());
    drop(starting);
    source.stop().await?;
    assert!(!directory.path().join("source.db").exists());
    Ok(())
}

fn captured() -> &'static Mutex<Option<sdk::NativeSourceProgress>> {
    static CAPTURE: OnceLock<Mutex<Option<sdk::NativeSourceProgress>>> = OnceLock::new();
    CAPTURE.get_or_init(|| Mutex::new(None))
}
struct ProbeFactory;
impl Factory for ProbeFactory {
    fn metadata(&self) -> sdk::FactoryMetadata {
        sdk::FactoryMetadata {
            implementation: ImplementationIdentity::try_new("fixture/progress", "1").unwrap(),
            role: ComponentRole::Source,
            configuration_version: 1,
            configuration: sdk::ConfigSchema {
                fields: [(
                    "mode".into(),
                    sdk::ConfigField {
                        value_type: sdk::ConfigType::String,
                        required: true,
                        secret: false,
                    },
                )]
                .into(),
                allow_additional: false,
            },
            ports: vec![PortDescriptor::new(
                PortId::try_new("out").unwrap(),
                PortDirection::Output,
                GraphChangeCodec::schema().descriptor().clone(),
                PipeRequirements::default(),
            )],
            completion: None,
            capabilities: sdk::Capabilities::default(),
        }
    }
    fn supports_source_admission(&self) -> bool {
        true
    }
    fn supports_source_progress(&self) -> bool {
        true
    }
    fn create(
        &self,
        request: &sdk::CreateRequest,
        _: sdk::ControlSender,
    ) -> Result<sdk::CreatedComponent> {
        Ok(sdk::Component::Source(Box::new(Probe {
            descriptor: self.metadata().descriptor(request.id.clone())?,
            reader: None,
            mode: request.configuration["mode"].as_str().unwrap().into(),
            started: false,
        }))
        .into())
    }
    fn create_with_progress(
        &self,
        request: &sdk::CreateRequest,
        _: sdk::ControlSender,
        progress: sdk::NativeSourceProgress,
    ) -> Result<sdk::CreatedComponent> {
        let mode = request.configuration["mode"].as_str().unwrap().to_owned();
        let reader = match mode.as_str() {
            "ignored" => None,
            "substituted" => Some(SourceProgressReader::from(owner())),
            _ => Some(progress.reader()),
        };
        *captured().lock().unwrap() = Some(progress);
        Ok(sdk::Component::Source(Box::new(Probe {
            descriptor: self.metadata().descriptor(request.id.clone())?,
            reader,
            mode,
            started: false,
        }))
        .into())
    }
}
struct Probe {
    descriptor: ComponentDescriptor,
    reader: Option<SourceProgressReader>,
    mode: String,
    started: bool,
}
#[async_trait]
impl ComputationComponent for Probe {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn configuration(&self) -> Result<serde_json::Value> {
        Ok(serde_json::json!({"mode":self.mode}))
    }
    fn recovery_contract(&self) -> ComponentRecovery {
        if self.reader.is_none()
            || self.mode == "retentionless"
            || (self.mode == "mutate" && self.started)
        {
            ComponentRecovery::default()
        } else {
            ComponentRecovery::admitted(StorageDurability::LOCAL_POWER_LOSS).replay_until(id(
                if self.mode == "wrong-consumer" {
                    "another"
                } else {
                    "query"
                },
            ))
        }
    }
    async fn start(&mut self) -> Result<()> {
        self.started = true;
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
        self.started = false;
        Ok(())
    }
}
#[async_trait]
impl EnvelopeSource for Probe {
    fn recovery_reader(&self) -> Option<SourceProgressReader> {
        self.reader.clone()
    }
    async fn next(&mut self) -> Result<Option<OutputEnvelope>> {
        Ok(None)
    }
}
fn export() -> &'static sdk::ExportedPlugin {
    static EXPORT: OnceLock<sdk::ExportedPlugin> = OnceLock::new();
    EXPORT.get_or_init(|| {
        sdk::ExportedPlugin::new(
            sdk::PluginDefinition::new(
                "fixture/progress",
                "1",
                vec![Arc::new(ProbeFactory)],
                vec![GraphChangeCodec::schema()],
            )
            .unwrap(),
        )
        .unwrap()
    })
}
unsafe extern "C" fn metadata() -> *const abi::Metadata {
    export().metadata()
}
unsafe extern "C" fn entry(out: *mut abi::PluginHandle) -> abi::Status {
    sdk::transport::status_boundary(|| unsafe { export().entry(out) })
}
unsafe extern "C" fn recovery() -> *const abi::recovery::PluginRecoveryV1 {
    export().recovery()
}
unsafe extern "C" fn services() -> *const abi::services::PluginServicesV1 {
    export().services()
}
fn probe_plugin(recovery_enabled: bool) -> Result<Arc<super::super::NativePlugin>> {
    unsafe {
        super::super::NativePlugin::from_entry_points_with_recovery(
            metadata,
            entry,
            Some(services),
            recovery_enabled.then_some(recovery),
        )
    }
}

#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(native_progress)]
async fn native_recovery_negotiation_rejects_forged_pairing_and_activation_changes() -> Result<()> {
    let old = probe_plugin(false)?;
    let new = probe_plugin(true)?;
    assert_eq!(old.metadata(), new.metadata());
    assert!(!old.factories()[0]
        .descriptor
        .dependencies
        .contains_key("source_progress"));
    let factory = &new.factories()[0];
    assert_eq!(
        factory.descriptor.dependencies["source_progress"].minimum,
        0
    );
    assert_eq!(
        factory.descriptor.dependencies["source_progress"].maximum,
        Some(1)
    );
    let mut specification =
        factory.specification(id("source"), serde_json::json!({"mode":"valid"}))?;
    specification.dependencies.insert(
        "source_progress".into(),
        vec![ResourceId::try_new("owner")?],
    );
    assert!(old.factories()[0].validate(&specification).is_err());
    factory.validate(&specification)?;
    specification
        .dependencies
        .insert("admission".into(), vec![ResourceId::try_new("journal")?]);
    assert!(factory.validate(&specification).is_err());
    for mode in ["ignored", "substituted", "wrong-consumer"] {
        assert!(
            factory
                .construct(
                    request(factory, serde_json::json!({"mode":mode})),
                    None,
                    Some(owner())
                )
                .is_err(),
            "{mode}"
        );
        assert!(captured()
            .lock()
            .unwrap()
            .take()
            .unwrap()
            .reader()
            .snapshot()
            .is_err());
    }
    let mut request = request(factory, serde_json::json!({"mode":"valid"}));
    request.scope.as_mut().unwrap().graph_id = "wrong".into();
    assert!(factory.construct(request, None, Some(owner())).is_err());
    for mode in ["valid", "retentionless", "mutate"] {
        let mut source = factory.construct(
            self::request(factory, serde_json::json!({"mode":mode})),
            None,
            Some(owner()),
        )?;
        let captured = captured().lock().unwrap().take().unwrap();
        assert!(captured.reader().local_owner().is_none());
        assert!(captured.reader().snapshot().is_ok());
        assert_eq!(
            source.recovery_contract().replay_retention().is_some(),
            mode != "retentionless"
        );
        assert_eq!(source.start().await.is_err(), mode == "mutate");
        source.stop().await?;
        drop(source);
        assert!(captured.reader().snapshot().is_err());
    }
    let mut fast =
        old.factories()[0].create_component(id("source"), serde_json::json!({"mode":"valid"}))?;
    assert!(fast.recovery_progress().is_none());
    fast.start().await?;
    assert!(fast.next().await?.is_none());
    fast.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(native_progress)]
async fn native_recovery_graph_factory_resolves_typed_resources_and_revokes_after_shutdown(
) -> Result<()> {
    let plugin = probe_plugin(true)?;
    for graph_id in [GRAPH, "wrong-graph"] {
        let factory = plugin.factories()[0].clone();
        let mut specification =
            factory.specification(id("source"), serde_json::json!({"mode":"retentionless"}))?;
        let resource = ResourceId::try_new("progress")?;
        specification
            .dependencies
            .insert("source_progress".into(), vec![resource.clone()]);
        let progress = owner();
        let mut graph = ComputationGraph::builder(graph_id)
            .declare_resource(ResourceSpecification {
                id: resource.clone(),
                role: ResourceRole::Checkpoint,
                ownership: ResourceOwnership::Borrowed,
                binding: "progress".into(),
            })?
            .provide_resource(
                resource,
                ResourceHandle::new(
                    ResourceRole::Checkpoint,
                    Arc::new(QuerySourceProgressResource(progress.clone())),
                ),
            )?
            .component(specification, factory)
            .sink(Box::new(sink("query", GraphChangeCodec::schema())?))
            .bind_stream(endpoint("source", "out"), stream("changes"))
            .connect(
                EdgeDefinition::new(endpoint("source", "out"), endpoint("query", "in")),
                Box::new(BoundedPipeConfig { capacity: 1 }),
            )
            .build()?;
        let result = tokio::time::timeout(TIMEOUT, graph.start()?).await?;
        let captured = captured().lock().unwrap().take();
        if graph_id == GRAPH {
            result?;
            let captured = captured.context("factory did not receive the progress service")?;
            drop(graph);
            assert!(captured.reader().snapshot().is_err());
        } else {
            assert!(result.is_err());
            assert!(
                captured.is_none(),
                "foreign constructor must not receive wrong-scope progress"
            );
        }
    }
    Ok(())
}

#[test]
fn native_recovery_rejects_malformed_extension_before_plugin_entry() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    static CASE: AtomicUsize = AtomicUsize::new(0);
    static ENTERED: AtomicBool = AtomicBool::new(false);
    unsafe extern "C" fn must_not_enter(_: *mut abi::PluginHandle) -> abi::Status {
        ENTERED.store(true, Ordering::Relaxed);
        sdk::transport::status_boundary(|| Err(sdk::transport::Failure::failed("unexpected entry")))
    }
    unsafe extern "C" fn malformed() -> *const abi::recovery::PluginRecoveryV1 {
        static TABLES: OnceLock<[abi::recovery::PluginRecoveryV1; 4]> = OnceLock::new();
        let tables = TABLES.get_or_init(|| {
            let valid = unsafe { *recovery() };
            let mut tables = [valid; 4];
            tables[0].version += 1;
            tables[1].reserved = 1;
            tables[2].inspect = None;
            tables[3].header.size = 0;
            tables
        });
        tables
            .get(CASE.load(Ordering::Relaxed))
            .map_or(std::ptr::null(), |table| table)
    }
    for index in 0..5 {
        CASE.store(index, Ordering::Relaxed);
        assert!(unsafe {
            super::super::NativePlugin::from_entry_points_with_recovery(
                metadata,
                must_not_enter,
                None,
                Some(malformed),
            )
        }
        .is_err());
        assert!(!ENTERED.load(Ordering::Relaxed));
    }
}
