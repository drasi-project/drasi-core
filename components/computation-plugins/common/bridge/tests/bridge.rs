// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

#[path = "bridge/codec.rs"]
mod codec;
#[cfg(unix)]
#[path = "bridge/integration.rs"]
mod integration;
#[cfg(unix)]
#[path = "bridge/tls.rs"]
mod tls;

use std::{
    collections::BTreeMap,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::Result;
use bytes::Bytes;
use drasi_core::{
    computation::{ComputationIndexProvider, ComputationIndexes},
    interface::FailureMode,
    models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
    },
};
use drasi_index_rocksdb::{
    computation::RocksDbComputationProvider, RocksDbMemoryBudget, RocksIndexOptions,
};
use drasi_lib::computation::v1::*;

use drasi_computation_bridge_common::*;
use drasi_host_sdk::computation::{NativeFactory, NativePlugin};

/// The bridge failure inside a sink or lifecycle error, which also carries the
/// graph's delivery-failure or lifecycle classification.
trait BridgeCause {
    fn bridge_cause(&self) -> Option<&BridgeError>;
}

impl BridgeCause for anyhow::Error {
    fn bridge_cause(&self) -> Option<&BridgeError> {
        self.chain().find_map(|cause| cause.downcast_ref())
    }
}

impl BridgeCause for drasi_computation_plugin_sdk::lifecycle::LifecycleError {
    fn bridge_cause(&self) -> Option<&BridgeError> {
        self.source_error().bridge_cause()
    }
}
use drasi_computation_sink_bridge::BridgeSink;
use drasi_computation_source_bridge::BridgeSource;

/// How a graph reaches the bridge plugins: linked into this test binary, or
/// through the library staged by `make build-test-plugins`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Link {
    Static,
    Loaded,
}

impl Link {
    const ALL: [Link; 2] = [Link::Static, Link::Loaded];

    fn plugin(self, role: ComponentRole) -> Result<Arc<NativePlugin>> {
        let source = role == ComponentRole::Source;
        match self {
            Link::Static => NativePlugin::from_definition(if source {
                drasi_computation_source_bridge::plugin()?
            } else {
                drasi_computation_sink_bridge::plugin()?
            }),
            Link::Loaded => {
                let package = if source {
                    drasi_computation_source_bridge::PLUGIN_ID
                } else {
                    drasi_computation_sink_bridge::PLUGIN_ID
                };
                static LOADED: std::sync::OnceLock<
                    std::sync::Mutex<BTreeMap<&'static str, Arc<NativePlugin>>>,
                > = std::sync::OnceLock::new();
                let mut loaded = LOADED
                    .get_or_init(Default::default)
                    .lock()
                    .map_err(|_| anyhow::anyhow!("loaded bridge plugin cache poisoned"))?;
                if let Some(plugin) = loaded.get(package) {
                    return Ok(plugin.clone());
                }
                let path = library_directory().join(format!(
                    "{}{}{}",
                    std::env::consts::DLL_PREFIX,
                    package.replace('-', "_"),
                    std::env::consts::DLL_SUFFIX,
                ));
                anyhow::ensure!(
                    path.is_file(),
                    "missing {}; first run make build-test-plugins",
                    path.display()
                );
                let plugin = drasi_host_sdk::computation::load(&path)?;
                loaded.insert(package, plugin.clone());
                Ok(plugin)
            }
        }
    }

    fn factory(self, role: ComponentRole) -> Result<Arc<NativeFactory>> {
        let plugin = self.plugin(role)?;
        let factory = plugin.factories()[0].clone();
        let expected = if role == ComponentRole::Source {
            SOURCE_FACTORY
        } else {
            SINK_FACTORY
        };
        anyhow::ensure!(
            plugin.factories().len() == 1
                && factory.metadata().implementation.name.as_ref() == expected,
            "{self:?} bridge plugin must export exactly {expected}"
        );
        Ok(factory)
    }

    /// One endpoint's construction recipe and the factory that builds it.
    fn component(
        self,
        config: &BridgeConfig,
        role: ComponentRole,
    ) -> Result<(ComponentSpecification, Arc<dyn ComponentFactory>)> {
        let factory = self.factory(role)?;
        let id = if role == ComponentRole::Source {
            config.receiver_endpoint.clone()
        } else {
            config.sender_endpoint.clone()
        };
        let specification = factory.specification(id, serde_json::to_value(config)?)?;
        Ok((specification, factory))
    }
}

/// Honours `CARGO_TARGET_DIR` and `DRASI_NATIVE_PLUGIN_DIR` as
/// `build-test-plugins` does.
fn library_directory() -> PathBuf {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(4)
        .expect("workspace")
        .to_owned();
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace.join("target"));
    let target = if target.is_absolute() {
        target
    } else {
        workspace.join(target)
    };
    std::env::var_os("DRASI_NATIVE_PLUGIN_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| target.join("debug/plugins"))
}

const GRAPH: &str = "__drasi_lib_runtime__";
const DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);
const SOURCE_ID: &str = "5c3e49cc-32f8-5b75-888d-30b75b572221";
const OTHER_SOURCE_ID: &str = "9a7efebd-4820-5bea-9eb3-17e12e60b60a";
const QUERY_ID: &str = "adda9b25-a196-524a-91e2-aa6dbefa64cf";
const BRIDGE_OUT_ID: &str = "f8c0597d-9354-50fc-a539-cb3112cc80ba";
const BRIDGE_IN_ID: &str = "4ea31e22-2f20-57c8-8999-97cd2abc9ed0";
const INPUT_ID: &str = "bfd083e9-2185-528c-a8c4-a8aac2825309";
const CAPTURE_ID: &str = "e1a8a858-9db4-5691-801c-34e7928690fe";

fn fixture_component_id(component: &str) -> &str {
    match component {
        "bridge-out" => BRIDGE_OUT_ID,
        "bridge-in" => BRIDGE_IN_ID,
        "input" => INPUT_ID,
        "capture" => CAPTURE_ID,
        _ => component,
    }
}

struct Directory(PathBuf);

impl Directory {
    fn new() -> Result<Self> {
        let path =
            PathBuf::from(".bridge-test-data").join(uuid::Uuid::new_v4().simple().to_string());
        std::fs::create_dir_all(&path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The exact namespace of the `plant/query` output, as the runtime derives it.
fn query_dataset() -> String {
    drasi_computation_contracts::output_identity::dataset_namespace(
        "left",
        GRAPH,
        &ComponentId::try_new(QUERY_ID).expect("component ID"),
        &PortId::try_new("out").expect("port ID"),
    )
    .expect("output dataset namespace")
}

fn query_projector() -> ResultNodeProjector {
    ResultNodeProjector::try_new(&query_dataset(), None).expect("query output projector")
}

fn namespace() -> Namespace {
    Namespace {
        version: 1,
        source_prefixes: vec![SOURCE_ID.into(), query_dataset()],
    }
}

fn configuration(
    directory: &Directory,
    source: bool,
    profile: Profile,
    schema: PortableDelivery,
) -> BridgeConfig {
    BridgeConfig {
        link_id: "link".into(),
        sender_instance: "left".into(),
        receiver_instance: "right".into(),
        sender_endpoint: drasi_core_types::ComponentId::try_new(
            "f8c0597d-9354-50fc-a539-cb3112cc80ba",
        )
        .expect("fixture UUID"),
        receiver_endpoint: drasi_core_types::ComponentId::try_new(
            "4ea31e22-2f20-57c8-8999-97cd2abc9ed0",
        )
        .expect("fixture UUID"),
        stream: StreamId::try_new(if source { "receiver/in" } else { "sender/out" })
            .expect("fixture stream"),
        peer_stream: StreamId::try_new(if source { "sender/out" } else { "receiver/in" })
            .expect("fixture stream"),
        delivery: schema,
        profile,
        failure_scope: FailureMode::ProcessRestart,
        on_permanent_failure: Default::default(),
        namespace: namespace(),
        transport: Transport::Unix {
            data_path: directory.path().join("data.sock").to_string_lossy().into(),
            control_path: directory
                .path()
                .join("control.sock")
                .to_string_lossy()
                .into(),
        },
        limits: Limits {
            retry_attempts: 1,
            queue_capacity: 1,
            timeout_ms: 2_000,
            ..Limits::default()
        },
        tls: None,
        mapping_directory: (!source && profile == Profile::Durable)
            .then(|| directory.path().join("sender").to_string_lossy().into()),
    }
}

fn graph_input(sequence: u64, value: i64) -> Result<ChangeEnvelope> {
    let mut properties = ElementPropertyMap::new();
    properties.insert("number", ElementValue::Integer(value));
    properties.insert(
        "nested",
        ElementValue::List(vec![
            ElementValue::String(Arc::from("λ")),
            ElementValue::Object(ElementPropertyMap::from(serde_json::json!({"flag":true}))),
        ]),
    );
    let event = GraphChangeCodec::encode_change(
        SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new(SOURCE_ID, "item"),
                    labels: Arc::from([Arc::from("Item")]),
                    effective_from: 1_700_000_000_000,
                },
                properties,
            },
        },
        StreamId::try_new("sender/out")?,
        sequence,
        Some(chrono::DateTime::from_timestamp(1_700_000_000, 123_456_789).expect("timestamp")),
    )?;
    let mut envelope = ChangeEnvelope::new(
        event.id().clone(),
        event.changes().clone(),
        event
            .system()
            .as_ref()
            .clone()
            .with_source_position(Bytes::from_static(b"commit/42")),
    );
    for value in [3, 8] {
        envelope.append_annotation(ContextEntry::try_new(
            ComponentId::try_new("ac7d3d4f-bac0-52cc-add7-caa0dba38431")?,
            "application/duplicate",
            ContextValue::Unsigned(value),
        )?)?;
    }
    Ok(envelope)
}

fn query_input(sequence: u64) -> Result<ChangeEnvelope> {
    use drasi_core::evaluation::{
        context::QueryPartEvaluationContext, variable_value::VariableValue,
    };
    let values = BTreeMap::from([
        ("number".into(), VariableValue::Integer(42.into())),
        (
            "nested".into(),
            VariableValue::List(vec![
                VariableValue::String("query value".into()),
                VariableValue::Null,
            ]),
        ),
        (
            "when".into(),
            VariableValue::Date(chrono::NaiveDate::from_ymd_opt(2026, 10, 12).expect("date")),
        ),
    ]);
    query_results(
        sequence,
        &query_projector(),
        &[QueryPartEvaluationContext::Adding {
            after: values,
            row_signature: 42,
        }],
    )
}

/// A current query output envelope for an explicit output scope.
fn query_results(
    sequence: u64,
    projector: &ResultNodeProjector,
    results: &[drasi_core::evaluation::context::QueryPartEvaluationContext],
) -> Result<ChangeEnvelope> {
    Ok(QueryChangeCodec::encode_evaluation(
        None,
        &ComponentId::try_new(QUERY_ID)?,
        SystemMetadata::new(StreamId::try_new("sender/out")?, sequence),
        projector,
        results,
        QueryOutputMetadata {
            query_id: drasi_core_types::ComponentId::try_new(
                "00000000-0000-0000-0000-0000000000b8",
            )
            .expect("fixture UUID"),
            input_origin: Some(drasi_computation_contracts::QueryInputOrigin::Source(
                drasi_core_types::ComponentId::try_new("5c3e49cc-32f8-5b75-888d-30b75b572221")
                    .expect("fixture UUID"),
            )),
            timestamp: chrono::DateTime::from_timestamp(1_700_000_000, 123_456_789)
                .expect("timestamp"),
            metadata: Default::default(),
            profiling: None,
        },
    )?
    .expect("query changes"))
}

fn transaction_input(sequence: u64) -> Result<ChangeEnvelope> {
    let limits = SourceTransactionLimits {
        max_changes: NonZeroUsize::new(10).expect("nonzero change limit"),
        max_bytes: NonZeroUsize::new(65_536).expect("nonzero byte limit"),
        max_duration_ms: std::num::NonZeroU64::new(10_000).expect("nonzero duration limit"),
    };
    let mut transaction = SourceTransactionBuilder::new(
        &drasi_core_types::ComponentId::try_new("5c3e49cc-32f8-5b75-888d-30b75b572221")
            .expect("fixture UUID"),
        limits,
    )?;
    for value in [1, 2] {
        for change in GraphChangeCodec::decode_changes(&graph_input(sequence, value)?)? {
            transaction.push(change)?;
        }
    }
    Ok(transaction
        .commit(
            Bytes::from_static(b"transaction-1"),
            Bytes::from_static(b"commit/42"),
        )?
        .into_envelope(
            StreamId::try_new("sender/out")?,
            sequence,
            chrono::DateTime::from_timestamp(1_700_000_000, 123_456_789).expect("timestamp"),
        )?)
}

fn provider(path: &Path) -> Arc<dyn ComputationIndexProvider> {
    Arc::new(RocksDbComputationProvider::new(
        path,
        RocksIndexOptions::new(
            false,
            false,
            RocksDbMemoryBudget::from_total_budget_bytes(32 << 20).expect("memory budget"),
        ),
    ))
}

fn envelope_codec() -> Result<EnvelopeCodec> {
    let mut codec = EnvelopeCodec::new(NonZeroUsize::new(8 << 20).expect("nonzero codec capacity"));
    codec.register_schema(GraphChangeCodec::schema())?;
    Ok(codec)
}

fn definition(stream: &str, subscriber: &str, capacity: usize) -> Result<QosChannelDefinition> {
    Ok(QosChannelDefinition {
        stream: StreamId::try_new(stream)?,
        capacity: NonZeroUsize::new(capacity).expect("nonzero channel capacity"),
        durable: true,
        retention: RetentionPolicy::Backpressure,
        subscribers: BTreeMap::from([(subscriber.into(), SubscriptionStart::Earliest)]),
    })
}

async fn channel(
    path: &Path,
    stream: &str,
    instance: &str,
    component: &str,
    subscriber: &str,
    capacity: usize,
) -> Result<Arc<QosChannel>> {
    let component = ComponentId::try_new(fixture_component_id(component))?;
    let indexes = provider(path)
        .create_indexes("bridge-test-ingress", &component)
        .await?;
    Ok(QosChannel::persistent_with_recovery(
        definition(stream, subscriber, capacity)?,
        indexes,
        envelope_codec()?,
        "bridge-test-ingress",
        QosRecoveryOptions::Admission(AdmissionOptions {
            construction_scope: instance.into(),
            graph_id: GRAPH.into(),
            component_id: component,
            failure_scope: FailureMode::ProcessRestart,
            max_producers: NonZeroUsize::MIN,
            receipts_per_producer: NonZeroUsize::MIN,
        }),
    )
    .await?)
}

fn endpoint(component: &str, port: &str) -> Endpoint {
    Endpoint::new(
        ComponentId::try_new(fixture_component_id(component)).expect("component ID"),
        PortId::try_new(port).expect("port ID"),
    )
}

fn declaration(id: &str, role: ResourceRole) -> ResourceSpecification {
    ResourceSpecification {
        id: ResourceId::try_new(id).expect("resource ID"),
        role,
        ownership: ResourceOwnership::Borrowed,
        binding: id.into(),
    }
}

fn input(envelope: ChangeEnvelope) -> InputEnvelope {
    InputEnvelope {
        execution_context: Default::default(),
        port: PortId::try_new("in").expect("input port ID"),
        envelope,
    }
}

/// Seals the receiver from the sender's side, as a composition finishing the
/// link does, and checks that the cut is the sender's last resolved receipt.
/// The control connection closes on return.
async fn finish(
    sink: &BridgeSink,
    config: &BridgeConfig,
    tls: Option<Arc<TlsResource>>,
) -> Result<IngressCut> {
    let mut endpoint =
        BridgeEndpointControl::connect(config.clone(), ComponentRole::Sink, tls).await?;
    let cut = endpoint.seal(DEADLINE).await?;
    match sink.last_receipt() {
        Some(receipt) => {
            assert_eq!(cut.session.as_ref(), Some(&receipt.admission.session));
            assert_eq!(cut.client_sequence, receipt.admission.sequence);
            assert_eq!(cut.position, receipt.admission.position);
            assert_eq!(cut.contract, receipt.contract);
            assert_eq!(cut.boundary, receipt.boundary);
        }
        None => assert_eq!((cut.client_sequence, cut.position), (0, 0)),
    }
    Ok(cut)
}

/// The receiving host's observing control lane.
async fn observe(
    config: &BridgeConfig,
    tls: Option<Arc<TlsResource>>,
) -> Result<BridgeEndpointControl> {
    Ok(BridgeEndpointControl::connect(config.clone(), ComponentRole::Source, tls).await?)
}

pub(crate) mod lifecycle_ctx {
    use drasi_computation_plugin_sdk::lifecycle::{
        CancellationToken, DisposeContext, DisposeReason, OperationScope, PauseContext,
        ResumeContext, StartContext, StateIntent, StopContext, StopMode,
    };

    pub(crate) fn scope() -> OperationScope {
        OperationScope::new("test", "component", 1, None, CancellationToken::new())
    }
    pub(crate) fn start() -> StartContext {
        StartContext::new(scope(), None)
    }
    pub(crate) fn pause() -> PauseContext {
        PauseContext::new(scope())
    }
    pub(crate) fn resume() -> ResumeContext {
        ResumeContext::new(scope())
    }
    pub(crate) fn stop() -> StopContext {
        StopContext::new(scope(), StopMode::Drain)
    }
    pub(crate) fn dispose() -> DisposeContext {
        DisposeContext::new(scope(), DisposeReason::Shutdown, StateIntent::Preserve, 1)
            .expect("valid dispose context")
    }
    pub(crate) fn erase() -> DisposeContext {
        DisposeContext::new(scope(), DisposeReason::Removal, StateIntent::Erase, 1)
            .expect("valid dispose context")
    }
}
