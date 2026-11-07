// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::Context as _;
use async_trait::async_trait;
use drasi_computation_plugin_abi as abi;
use drasi_computation_plugin_sdk::{
    metadata::FactoryMetadata,
    transport::{checked_table, take_reply, take_status, Failure, OperationFuture, ReceivedBytes},
    wire::{self, InstanceMetadata},
};
use drasi_lib::computation::v1::*;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

use super::{
    control::{self, ControlBinding},
    loader::PluginOwner,
};

mod updates;

pub(super) fn native_failure_disposition(error: &Failure) -> FailureDisposition {
    if error.code == abi::status::FAILED && error.retryable {
        FailureDisposition::Retryable
    } else {
        FailureDisposition::Terminal
    }
}

fn operation_failure(error: Failure) -> anyhow::Error {
    ComponentOperationError::new(
        native_failure_disposition(&error),
        native_operation_cause(error),
    )
    .into()
}

pub(super) fn native_operation_cause(error: Failure) -> anyhow::Error {
    if error.code == abi::status::PANICKED {
        ComponentPanic::new(error).into()
    } else {
        error.into()
    }
}

#[cfg(test)]
mod failure_tests {
    use super::*;

    #[test]
    fn native_terminal_status_cannot_be_made_retryable_by_its_flag() {
        for (code, retryable, expected) in [
            (abi::status::FAILED, true, FailureDisposition::Retryable),
            (abi::status::FAILED, false, FailureDisposition::Terminal),
            (abi::status::PANICKED, true, FailureDisposition::Terminal),
            (abi::status::PROTOCOL, true, FailureDisposition::Terminal),
            (abi::status::CANCELLED, true, FailureDisposition::Terminal),
            (abi::status::CLOSED, true, FailureDisposition::Terminal),
            (u32::MAX, true, FailureDisposition::Terminal),
        ] {
            let error = operation_failure(Failure {
                code,
                message: "retained native cause".into(),
                retryable,
            })
            .context("host operation");
            assert_eq!(
                error
                    .downcast_ref::<ComponentOperationError>()
                    .unwrap()
                    .disposition,
                expected
            );
            let source = error
                .chain()
                .find_map(|source| source.downcast_ref::<Failure>())
                .unwrap();
            assert_eq!(source.code, code);
            assert_eq!(source.message, "retained native cause");
            assert_eq!(
                error.chain().any(|cause| cause.is::<ComponentPanic>()),
                code == abi::status::PANICKED,
                "only native PANICKED marks the generation as damaged"
            );
        }
    }
}

pub(super) struct RemoteComponent {
    raw: abi::ComponentHandle,
    _plugin: Arc<PluginOwner>,
    pub(super) control: Mutex<ControlBinding>,
    stopping: AtomicBool,
    retiring: AtomicBool,
}
unsafe impl Send for RemoteComponent {}
unsafe impl Sync for RemoteComponent {}
impl RemoteComponent {
    pub(super) fn state(&self) -> *mut std::ffi::c_void {
        self.raw.state
    }
    unsafe fn new(raw: abi::ComponentHandle, plugin: Arc<PluginOwner>) -> anyhow::Result<Self> {
        let table = unsafe { checked_table(raw.vtable)? };
        anyhow::ensure!(
            !raw.state.is_null()
                && table.release.is_some()
                && table.inspect.is_some()
                && table.configuration.is_some()
                && table.bind_control.is_some()
                && table.begin.is_some(),
            "incomplete native component table"
        );
        Ok(Self {
            raw,
            _plugin: plugin,
            control: Mutex::new(ControlBinding::default()),
            stopping: AtomicBool::new(false),
            retiring: AtomicBool::new(false),
        })
    }
    fn table(&self) -> &abi::ComponentVTable {
        unsafe { &*self.raw.vtable }
    }
    fn inspect(&self) -> anyhow::Result<InstanceMetadata> {
        let bytes = unsafe {
            take_reply(self.table().inspect.expect("validated")(self.raw.state))
                .map_err(operation_failure)?
        };
        Ok(serde_json::from_slice(&bytes)?)
    }
    fn configuration(&self) -> anyhow::Result<serde_json::Value> {
        let bytes = unsafe {
            take_reply(self.table().configuration.expect("validated")(
                self.raw.state,
            ))
            .map_err(operation_failure)?
        };
        Ok(serde_json::from_slice(&bytes)?)
    }
    pub(super) fn bind(&self, control: &abi::Control) -> std::result::Result<(), Failure> {
        unsafe {
            take_status(self.table().bind_control.expect("validated")(
                self.raw.state,
                control,
            ))
        }
    }
    pub(super) fn begin(
        &self,
        code: u32,
        input: &[u8],
        transaction: Option<&abi::Transaction>,
    ) -> anyhow::Result<OperationFuture> {
        if self.retiring.load(Ordering::Acquire)
            && !matches!(
                code,
                abi::operation::DELETE_OWNED_RESOURCES | abi::operation::DISPOSE
            )
            || code != abi::operation::STOP && self.stopping.load(Ordering::Acquire)
        {
            return Err(operation_failure(Failure::closed()));
        }
        anyhow::ensure!(
            input.len() <= abi::MAX_MESSAGE_BYTES,
            "native operation input exceeds size limit"
        );
        let mut operation = abi::OperationHandle::null();
        unsafe {
            take_status(self.table().begin.expect("validated")(
                self.raw.state,
                code,
                abi::BorrowedBytes::new(input),
                transaction.map_or(std::ptr::null(), |value| value),
                &mut operation,
            ))
            .map_err(operation_failure)?;
            OperationFuture::new(operation).map_err(operation_failure)
        }
    }
    pub(super) async fn call(&self, code: u32, input: &[u8]) -> anyhow::Result<ReceivedBytes> {
        self.begin(code, input, None)?
            .await
            .map_err(operation_failure)
    }
    pub(super) async fn unit(&self, code: u32, input: &[u8]) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.call(code, input).await?.is_empty(),
            "native unit operation returned unexpected data"
        );
        Ok(())
    }
    fn revoke_control(&self) {
        self.control
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .revoke();
    }
}
impl Drop for RemoteComponent {
    fn drop(&mut self) {
        self.revoke_control();
        unsafe { self.table().release.expect("validated")(self.raw.state) };
    }
}

/// A real native graph component. Role-specific calls are checked at both ends;
/// unsupported interfaces return errors rather than falling back to legacy roles.
pub struct NativeComponentProxy {
    pub(super) inner: Arc<RemoteComponent>,
    descriptor: ComponentDescriptor,
    metadata: FactoryMetadata,
    creation_request: drasi_computation_plugin_sdk::CreateRequest,
    pub(super) codec: Arc<BinaryEnvelopeCodec>,
    schemas: Vec<Arc<Schema>>,
    pending: bool,
    activated: bool,
    construction_scope: Option<drasi_computation_plugin_sdk::Scope>,
    bindings: SourceBindings,
    input_bindings: Vec<InputBinding>,
    batching: Option<abi::batching::PluginBatchingV1>,
    output_snapshots: Option<super::output_snapshots::OutputSnapshotsBinding>,
    processing: Option<super::processing::ProcessingBinding>,
    bootstrap: Option<Box<super::bootstrap_consumer::BootstrapBinding>>,
    bootstrap_replay: std::collections::VecDeque<InputEnvelope>,
    source_schema: Option<Arc<super::source_schema::NativeSourceSchemaReader>>,
    output_reads: Option<super::output_reads::NativeOutputReads>,
    query_descriptions:
        std::collections::BTreeMap<PortId, drasi_computation_contracts::QueryDescription>,
    query_api: Option<Arc<QueryApi>>,
    query_construction: Option<Box<super::query::QueryBinding>>,
    identity: Option<(
        Arc<super::NativeIdentityResource>,
        drasi_computation_plugin_sdk::ActivationCredentialContext,
    )>,
    stop: tokio::sync::Mutex<StopState>,
    deletion: tokio::sync::Mutex<RetirementState>,
    disposal: tokio::sync::Mutex<RetirementState>,
    update: tokio::sync::Mutex<Option<updates::UpdateState>>,
}

struct StopState {
    operation: Option<OperationFuture>,
    completed: bool,
    cleanup_complete: bool,
}

impl Default for StopState {
    fn default() -> Self {
        Self {
            operation: None,
            completed: false,
            cleanup_complete: true,
        }
    }
}

#[derive(Default)]
struct RetirementState {
    operation: Option<OperationFuture>,
    started: bool,
    completed: bool,
}

#[derive(Default)]
pub(super) struct SourceBindings {
    pub(super) admission: Option<(Arc<SourceAdmission>, super::admission::AdmissionBinding)>,
    pub(super) progress: Option<BoundProgress>,
    pub(super) consumer_progress: Option<BoundConsumerProgress>,
    pub(super) input_progress: Option<Arc<QuerySourceProgress>>,
}

pub(super) struct BoundProgress {
    pub(super) owner: Arc<QuerySourceProgress>,
    pub(super) binding: super::progress::ProgressBinding,
    pub(super) extension: abi::recovery::PluginRecoveryV1,
    pub(super) retention: Option<drasi_core::interface::StorageDurability>,
}

pub(super) struct BoundConsumerProgress {
    pub(super) owner: Arc<ConsumerProgressSet>,
    pub(super) binding: super::consumer_progress::ConsumerProgressBinding,
    pub(super) extension: abi::consumer_progress::PluginConsumerProgressV1,
    pub(super) retention: Option<drasi_core::interface::StorageDurability>,
    pub(super) position_policy: Option<Arc<InputPositionPolicy>>,
}

#[cfg(test)]
mod consumer_position_tests {
    use super::super::{tests::consumer_progress_fixture as fixture, NativePlugin};
    use super::*;
    use drasi_computation_contracts::MAX_SOURCE_POSITION_BYTES;
    use drasi_computation_plugin_sdk as sdk;
    use futures_util::poll;

    #[tokio::test(flavor = "current_thread")]
    async fn consumer_progress_input_policy_is_independent_of_pending_processing_and_component_lifetime(
    ) -> anyhow::Result<()> {
        let linked = NativePlugin::from_definition(sdk::PluginDefinition::new(
            "fixture/consumer-positions",
            "1",
            fixture::factories(),
            vec![GraphChangeCodec::schema()],
        )?)?;
        let dynamic = super::super::factory::recovery_tests::sqlite_plugin()?;
        for plugin in [linked, dynamic] {
            for role in [ComponentRole::Source, ComponentRole::Transformer] {
                let factory = plugin
                    .factories()
                    .iter()
                    .find(|factory| {
                        factory.metadata().role == role
                            && factory
                                .metadata()
                                .implementation
                                .name
                                .starts_with("fixture/consumer-progress-")
                    })
                    .unwrap();
                let id = ComponentId::try_new("producer")?;
                let owner = Arc::new(QuerySourceProgress::new(
                    "graph",
                    ComponentId::try_new("query")?,
                )?);
                let progress = Arc::new(ConsumerProgressSet::new("graph", id.clone(), [owner])?);
                let mut proxy = factory.create_component_with_consumer_progress(
                    id,
                    serde_json::json!({"mode": "positions"}),
                    sdk::Scope {
                        graph_id: "graph".into(),
                        instance_id: "graph".into(),
                        generation: 1,
                    },
                    progress.clone(),
                )?;
                let policy = proxy
                    .bindings
                    .consumer_progress
                    .as_ref()
                    .unwrap()
                    .position_policy
                    .clone()
                    .unwrap();
                assert_eq!(policy.key(), &SourceProgressKey::Source("source".into()));
                assert_eq!(
                    proxy.recovery_contract(),
                    ComponentRecovery::default().with_consumer_positions(progress, policy.clone())
                );
                proxy.start().await?;
                let input = InputEnvelope {
                    port: PortId::try_new("in")?,
                    envelope: GraphChangeCodec::encode_change(
                        drasi_core::models::SourceChange::Delete {
                            metadata: drasi_core::models::ElementMetadata {
                                reference: drasi_core::models::ElementReference::new(
                                    "source", "one",
                                ),
                                labels: Arc::from([Arc::from("Item")]),
                                effective_from: 1,
                            },
                        },
                        StreamId::try_new("source/out")?,
                        1,
                        None,
                    )?,
                };
                let mut pending = Box::pin(async {
                    if role == ComponentRole::Source {
                        proxy.next().await.map(|_| ())
                    } else {
                        proxy.transform(input).await.map(|_| ())
                    }
                });
                assert!(poll!(pending.as_mut()).is_pending());
                assert!(policy.is_after(&[0, 7], &[6, 90])?);
                assert!(!policy.is_after(&[1, 89], &[6, 90])?);
                assert!(!policy.is_after(&[1, 90], &[6, 90])?);
                assert!(policy.is_after(&[2, 1], &[6, 90]).is_err());
                assert!(policy.is_after(&[0, 1], &vec![0; MAX_SOURCE_POSITION_BYTES])?);
                assert!(policy
                    .is_after(&vec![0; MAX_SOURCE_POSITION_BYTES + 1], &[0])
                    .is_err());
                assert!(poll!(pending.as_mut()).is_pending());
                drop(pending);
                proxy.stop().await?;
                drop(proxy);
                // This is an immutable retained comparator, not a live reader;
                // the graph owns installation/revocation of its input policy.
                assert!(policy.is_after(&[0, 7], &[6, 90])?);
            }
        }
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn consumer_progress_comparison_panic_quarantines_pending_native_processing(
    ) -> anyhow::Result<()> {
        let plugin = NativePlugin::from_definition(sdk::PluginDefinition::new(
            "fixture/position-panic",
            "1",
            fixture::factories(),
            vec![GraphChangeCodec::schema()],
        )?)?;
        let id = ComponentId::try_new("producer")?;
        let progress = Arc::new(ConsumerProgressSet::new(
            "graph",
            id.clone(),
            [Arc::new(QuerySourceProgress::new(
                "graph",
                ComponentId::try_new("query")?,
            )?)],
        )?);
        let mut proxy = plugin.factories()[0].create_component_with_consumer_progress(
            id,
            serde_json::json!({"mode":"positions"}),
            sdk::Scope {
                graph_id: "graph".into(),
                instance_id: "graph".into(),
                generation: 1,
            },
            progress,
        )?;
        let policy = proxy
            .bindings
            .consumer_progress
            .as_ref()
            .unwrap()
            .position_policy
            .clone()
            .unwrap();
        proxy.start().await?;
        let mut pending = Box::pin(proxy.next());
        assert!(poll!(pending.as_mut()).is_pending());
        let error = policy.is_after(&[255], &[0]).unwrap_err();
        assert!(error.downcast_ref::<ComponentPanic>().is_some());
        let error = tokio::time::timeout(std::time::Duration::from_secs(1), pending)
            .await?
            .unwrap_err();
        assert!(error.chain().any(|cause| cause.is::<ComponentPanic>()));
        assert!(policy
            .is_after(&[0, 1], &[0])
            .unwrap_err()
            .downcast_ref::<ComponentPanic>()
            .is_some());
        proxy.stop().await?;
        Ok(())
    }
}

impl NativeComponentProxy {
    pub(super) unsafe fn new(
        raw: abi::ComponentHandle,
        metadata: FactoryMetadata,
        request: &drasi_computation_plugin_sdk::CreateRequest,
        codec: Arc<BinaryEnvelopeCodec>,
        schemas: Vec<Arc<Schema>>,
        plugin: Arc<PluginOwner>,
        bindings: SourceBindings,
    ) -> anyhow::Result<Self> {
        let inner = Arc::new(unsafe { RemoteComponent::new(raw, plugin)? });
        let mut bindings = bindings;
        let actual = inner.inspect()?;
        anyhow::ensure!(
            actual.descriptor == metadata.descriptor(request.id.clone())?
                && actual.capabilities == metadata.capabilities,
            "constructed native component changed its descriptor/capabilities"
        );
        anyhow::ensure!(
            actual
                .queries
                .keys()
                .collect::<std::collections::BTreeSet<_>>()
                == metadata.capabilities.output_queries.iter().collect(),
            "constructed native component changed its query read declarations"
        );
        metadata.configuration.validate(&inner.configuration()?)?;
        if let Some(progress) = &mut bindings.progress {
            let bytes = unsafe {
                take_reply(progress.extension.inspect.expect("validated")(
                    inner.raw.state,
                ))?
            };
            let recovery: drasi_computation_plugin_sdk::progress::InstanceRecovery =
                drasi_computation_plugin_sdk::progress::decode(&bytes)?;
            anyhow::ensure!(
                recovery.version == abi::recovery::VERSION,
                "unsupported native source recovery declaration"
            );
            progress.retention = recovery.retention;
        }
        if let Some(progress) = &mut bindings.consumer_progress {
            let bytes = unsafe {
                take_reply(progress.extension.inspect.expect("validated")(
                    inner.raw.state,
                ))?
            };
            let recovery: drasi_computation_plugin_sdk::consumer_progress::InstanceProgress =
                drasi_computation_plugin_sdk::consumer_progress::decode(&bytes)?;
            anyhow::ensure!(
                recovery.version == abi::consumer_progress::VERSION,
                "unsupported native consumer progress declaration"
            );
            progress.retention = recovery.retention;
            let mut raw = abi::consumer_progress::InputPositionPolicyV1::null();
            unsafe {
                take_status(progress.extension.position_policy.expect("validated")(
                    inner.raw.state,
                    &mut raw,
                ))?;
            }
            progress.position_policy = unsafe { super::input_position::from_borrowed(raw)? };
            anyhow::ensure!(
                progress.position_policy.is_some() == recovery.position_policy,
                "native producer changed its declared input position policy"
            );
        }
        Ok(Self {
            inner,
            descriptor: actual.descriptor,
            metadata,
            creation_request: request.clone(),
            codec,
            schemas,
            pending: false,
            activated: false,
            construction_scope: request.scope.clone(),
            bindings,
            batching: None,
            output_snapshots: None,
            processing: None,
            bootstrap: None,
            bootstrap_replay: std::collections::VecDeque::new(),
            source_schema: None,
            output_reads: None,
            query_descriptions: actual.queries,
            query_api: None,
            query_construction: None,
            input_bindings: Vec::new(),
            identity: None,
            stop: tokio::sync::Mutex::new(StopState::default()),
            deletion: tokio::sync::Mutex::new(RetirementState::default()),
            disposal: tokio::sync::Mutex::new(RetirementState::default()),
            update: tokio::sync::Mutex::new(None),
        })
    }

    pub(super) fn with_bootstrap(
        mut self,
        resource: Arc<super::NativeBootstrapResource>,
    ) -> anyhow::Result<Self> {
        self.bind_bootstrap(resource)?;
        Ok(self)
    }

    /// Bind an existing provider once, before activation. A progress-bound
    /// provider must retain this proxy's exact local processing-progress owner.
    pub fn bind_bootstrap(
        &mut self,
        resource: Arc<super::NativeBootstrapResource>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.activated && self.bootstrap.is_none(),
            "bootstrap provider must bind once before activation"
        );
        let scope = self.construction_scope.as_ref().ok_or_else(|| {
            anyhow::anyhow!("bootstrap requires the actual component construction scope")
        })?;
        let input = self
            .metadata
            .capabilities
            .quiescent_bootstrap
            .as_ref()
            .or(self.metadata.capabilities.durable_bootstrap.as_ref())
            .cloned()
            .ok_or_else(|| {
                anyhow::anyhow!("native transformer does not declare provider bootstrap")
            })?;
        self.bootstrap = Some(Box::new(super::bootstrap_consumer::BootstrapBinding::new(
            resource,
            input,
            scope,
            self.descriptor.id(),
            self.processing.as_ref(),
        )?));
        Ok(())
    }

    pub(super) fn with_source_schema(
        mut self,
        extension: Option<abi::source_schema::PluginSourceSchemaV1>,
    ) -> anyhow::Result<Self> {
        self.source_schema = extension
            .map(|extension| {
                super::source_schema::NativeSourceSchemaReader::bind(
                    &self.inner,
                    extension,
                    self.metadata.role,
                )
            })
            .transpose()?
            .flatten();
        Ok(self)
    }
    /// Open the declared providers. The extension is required exactly when
    /// factory metadata declares output reads.
    pub(super) fn with_output_reads(
        mut self,
        extension: Option<abi::output_reads::PluginOutputReadsV2>,
        follow: Option<abi::output_follow::PluginOutputFollowV2>,
    ) -> anyhow::Result<Self> {
        self.output_reads = super::output_reads::NativeOutputReads::bind(
            extension,
            follow,
            &self.inner,
            &self.descriptor,
            &self.metadata,
            &self.query_descriptions,
            &self.schemas,
            &self.codec,
        )?;
        if let Some(reads) = &self.output_reads {
            let readers = reads.readers();
            let queries: Vec<_> = readers
                .ports()
                .values()
                .filter_map(|port| port.query().cloned())
                .collect();
            anyhow::ensure!(
                queries.len() <= 1,
                "query API requires one declared query output"
            );
            if let Some(reader) = queries.into_iter().next() {
                self.query_api = Some(QueryApi::from_native_reader(
                    self.descriptor.id().clone(),
                    reader,
                    self.processing_progress_owner(),
                )?);
            }
        }
        Ok(self)
    }
    pub(super) fn with_identity(
        mut self,
        identity: Option<(
            Arc<super::NativeIdentityResource>,
            drasi_computation_plugin_sdk::ActivationCredentialContext,
        )>,
    ) -> Self {
        self.identity = identity;
        self
    }
    pub(super) fn with_output_snapshots(
        mut self,
        snapshots: Option<super::output_snapshots::OutputSnapshotsBinding>,
    ) -> Self {
        self.output_snapshots = snapshots;
        self
    }
    pub(super) fn with_processing(
        mut self,
        processing: Option<super::processing::ProcessingBinding>,
    ) -> Self {
        self.processing = processing;
        self
    }
    pub(super) fn with_batching(
        mut self,
        extension: abi::batching::PluginBatchingV1,
    ) -> anyhow::Result<Self> {
        extension
            .header
            .validate::<abi::batching::PluginBatchingV1>()?;
        anyhow::ensure!(
            extension.version == abi::batching::VERSION
                && extension.reserved == 0
                && extension.factory.is_some()
                && extension.policy.is_some()
                && self.metadata.role == ComponentRole::Sink,
            "invalid native sink batching extension"
        );
        self.batching = Some(extension);
        self.batching()?;
        Ok(self)
    }
    pub fn metadata(&self) -> &FactoryMetadata {
        &self.metadata
    }

    /// Bind an existing query-result catalogue before graph registration and activation.
    /// An actual connected `QueryResultsOutlet` supplies live delivery and backpressure.
    pub fn bind_query_catalog(
        &mut self,
        catalog: QueryResultsCatalog,
        mode: drasi_lib::DispatchMode,
        capacity: usize,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.activated && !self.pending,
            "bind the query result catalogue before activation"
        );
        let api = self
            .query_api
            .as_ref()
            .context("native component does not expose a query read capability")?;
        self.query_api = Some(api.with_native_result_catalog(catalog, mode, capacity)?);
        Ok(())
    }

    pub(super) fn bind_query_construction(
        &mut self,
        construction: QueryConstruction,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.activated && !self.pending && self.query_construction.is_none(),
            "ordinary query resources must bind once before activation"
        );
        if let Some(provider) = &construction.bootstrap {
            self.bind_bootstrap(Arc::new(super::NativeBootstrapResource(provider.clone())))?;
        }
        if let Some(clock) = &construction.scheduling {
            self.processing
                .as_ref()
                .context("query clock requires indexed processing")?
                .bind_clock(clock)?;
        }
        let api = self
            .query_api
            .as_ref()
            .context("native query has no query reader")?;
        if let Some(counter) = &construction.transport_sequence {
            self.processing
                .as_ref()
                .context("query transport requires indexed processing")?
                .bind_transport(counter.clone())?;
        }
        if let Some(config) = &construction.configuration {
            self.query_api = Some(api.with_native_configuration(
                config.clone(),
                construction.catalog.clone(),
                construction.default_dispatch_capacity,
            )?);
        } else if let Some(catalog) = &construction.catalog {
            self.query_api = Some(api.with_native_result_catalog(
                catalog.clone(),
                drasi_lib::DispatchMode::Channel,
                construction.default_dispatch_capacity,
            )?);
        }
        self.query_construction = Some(Box::new(super::query::QueryBinding {
            construction,
            session: None,
            observer: None,
            draining: false,
        }));
        Ok(())
    }

    async fn finish_query_drain(&mut self, outputs: &[OutputEnvelope]) -> anyhow::Result<()> {
        if !outputs.is_empty() || self.pending || !self.bootstrap_replay.is_empty() {
            return Ok(());
        }
        let Some(binding) = self
            .query_construction
            .as_mut()
            .filter(|binding| binding.draining)
        else {
            return Ok(());
        };
        let (sequence, generation, persistent) = self
            .query_api
            .as_ref()
            .context("query drain has no result reader")?
            .output_frontier()
            .await?;
        binding
            .session
            .as_mut()
            .context("query drain has no control session")?
            .complete(
                sequence,
                generation,
                persistent,
                binding.construction.options.publication,
            );
        binding.draining = false;
        Ok(())
    }

    /// Read the actual indexed owner's committed input progress. This data-only
    /// view supplies no local source-retirement proof. Stop revokes old views.
    pub fn committed_input_progress(
        &self,
    ) -> anyhow::Result<Option<drasi_computation_contracts::SourceProgressView>> {
        Ok(self
            .processing
            .as_ref()
            .map(|processing| processing.committed_input_progress())
            .transpose()?
            .flatten())
    }

    /// State used by a bootstrap provider in this component's actual processing
    /// transaction domain. The handle is inactive until its owner is opened.
    pub fn bootstrap_state(&self) -> Option<Arc<dyn drasi_computation_contracts::BootstrapState>> {
        self.processing
            .as_ref()
            .map(|processing| processing.bootstrap_state())
    }

    /// The local progress resource published by this exact processing owner.
    /// Sources/coordinators must bind this handle, not a same-named replacement.
    pub fn processing_progress_owner(&self) -> Option<Arc<QuerySourceProgress>> {
        self.processing
            .as_ref()
            .and_then(|processing| processing.progress_owner())
    }

    /// Acquire actual index resources before an asserted graph captures this
    /// component's recovery contract. Native processing remains inactive until start.
    pub async fn prepare_processing(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.activated,
            "processing resources must prepare before activation"
        );
        let Some(processing) = &mut self.processing else {
            return Ok(());
        };
        let result = processing.prepare().await;
        if let Err(error) = result {
            return match processing.close().await {
                Ok(()) => Err(error),
                Err(cleanup) => Err(drasi_lib::error::OperationFailures::new(
                    "native processing preparation and cleanup failed",
                    vec![error, cleanup],
                )
                .into()),
            };
        }
        Ok(())
    }

    fn begin_processing(&self) -> anyhow::Result<Option<super::processing::ProcessingCall>> {
        if let Some(processing) = &self.processing {
            processing.output_ready()?;
            processing.progress_call()
        } else {
            Ok(None)
        }
    }

    async fn outputs(&mut self, code: u32, input: &[u8]) -> anyhow::Result<Vec<OutputEnvelope>> {
        let _query_timer = self
            .query_api
            .as_ref()
            .map(|api| api.native_operation_timer());
        let progress_call = self
            .processing
            .as_ref()
            .map(|processing| processing.progress_call())
            .transpose()?
            .flatten();
        let reply = self.inner.call(code, input).await?;
        let result: wire::OutputBatch<'_> = wire::decode(&reply)?;
        anyhow::ensure!(
            result.query_observation.is_none() || self.metadata.capabilities.query_observations,
            "native component returned undeclared query observations"
        );
        let observation = result
            .query_observation
            .map(|value| value.into_observation())
            .transpose()?;
        anyhow::ensure!(
            !result.pending || self.metadata.capabilities.continuations,
            "native component returned undeclared continuation work"
        );
        let outputs = result
            .outputs
            .into_iter()
            .map(|output| output.into_output(&self.codec))
            .collect::<anyhow::Result<Vec<_>>>()?;
        for output in &outputs {
            wire::check_port(
                &self.descriptor,
                &output.port,
                &output.envelope,
                PortDirection::Output,
            )?;
        }
        self.pending = result.pending;
        if let Some(api) = &self.query_api {
            api.observe_native_progress(&outputs, observation.as_ref())?;
            if let Some(reads) = self.output_reads.as_ref().filter(|_| observation.is_some()) {
                reads.published();
            }
        } else {
            anyhow::ensure!(
                observation.is_none(),
                "query observation has no query reader"
            );
        }
        if let Some(call) = progress_call {
            call.complete();
        }
        Ok(outputs)
    }
    fn input(&self, input: &InputEnvelope) -> anyhow::Result<Vec<u8>> {
        wire::check_port(
            &self.descriptor,
            &input.port,
            &input.envelope,
            PortDirection::Input,
        )?;
        wire::encode(&wire::Envelope::input(input, &self.codec)?)
    }
    pub(super) fn into_transactional(self) -> anyhow::Result<NativeTransactionalProxy> {
        anyhow::ensure!(
            self.bindings.input_progress.is_none(),
            "graph-owned volatile input progress cannot be used inside a transaction"
        );
        anyhow::ensure!(
            self.metadata.capabilities.transactional,
            "native component is not a transaction participant"
        );
        anyhow::ensure!(
            !self.metadata.capabilities.input_binding_updates,
            "native transaction steps cannot receive graph input-binding updates"
        );
        let schema = |direction| -> anyhow::Result<Arc<Schema>> {
            let port = self
                .descriptor
                .ports()
                .iter()
                .find(|port| port.direction() == direction)
                .ok_or_else(|| anyhow::anyhow!("missing native transaction port"))?;
            self.schemas
                .iter()
                .find(|schema| schema.descriptor() == port.schema())
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("missing native transaction schema"))
        };
        let input = schema(PortDirection::Input)?;
        let output = schema(PortDirection::Output)?;
        Ok(NativeTransactionalProxy {
            component: self,
            input,
            output,
        })
    }
}
impl Drop for NativeComponentProxy {
    fn drop(&mut self) {
        if let Some(schema) = self.source_schema.take() {
            schema.retire();
        }
        if let Some(reads) = self.output_reads.take() {
            reads.retire();
        }
        self.output_snapshots.take();
        self.bindings.admission.take();
        self.bindings.progress.take();
        self.bindings.consumer_progress.take();
        self.inner.revoke_control();
    }
}

#[async_trait]
impl ComputationComponent for NativeComponentProxy {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn bind_query_test_support(
        &mut self,
        inspector: &ComputationInspector,
        generation: ComponentGeneration,
    ) {
        if let Some(binding) = &mut self.query_construction {
            binding.observer = binding
                .construction
                .scheduling
                .as_ref()
                .and_then(|scheduling| {
                    scheduling.observe_query(inspector, self.descriptor.id().clone(), generation)
                });
        }
    }
    fn query_api(&self) -> Option<Arc<QueryApi>> {
        self.query_api.clone()
    }
    fn source_schema_reader(&self) -> Option<Arc<dyn SourceSchemaReader>> {
        self.source_schema
            .as_ref()
            .map(|reader| reader.clone() as Arc<dyn SourceSchemaReader>)
    }
    fn output_readers(&self) -> OutputReaders {
        self.output_reads
            .as_ref()
            .map_or_else(OutputReaders::default, |reads| reads.readers())
    }
    fn recovery_contract(&self) -> ComponentRecovery {
        if let Some(processing) = &self.processing {
            if processing.progress_owner().is_some() {
                return processing.recovery_contract();
            }
        }
        let recovery = if let Some(progress) = &self.bindings.consumer_progress {
            let recovery = progress
                .retention
                .map_or_else(ComponentRecovery::default, ComponentRecovery::admitted)
                .with_consumer_progress(progress.owner.clone());
            match &progress.position_policy {
                Some(policy) => {
                    recovery.with_consumer_positions(progress.owner.clone(), policy.clone())
                }
                None => recovery,
            }
        } else {
            match &self.bindings.progress {
                Some(progress) => {
                    progress
                        .retention
                        .map_or_else(ComponentRecovery::default, |retention| {
                            ComponentRecovery::admitted(retention)
                                .replay_until(progress.owner.component_id().clone())
                        })
                }
                None => ComponentRecovery::default(),
            }
        };
        match &self.bindings.input_progress {
            Some(progress) => recovery.with_volatile_input_progress(progress.clone()),
            None => recovery,
        }
    }
    fn configuration(&self) -> anyhow::Result<serde_json::Value> {
        let value = self.inner.configuration()?;
        self.metadata.configuration.validate(&value)?;
        Ok(value)
    }
    #[allow(
        clippy::misnamed_getters,
        reason = "factory construction bindings and component updates are distinct capabilities"
    )]
    fn binds_inputs(&self) -> bool {
        self.metadata.capabilities.input_binding_updates
    }
    async fn bind_inputs(&mut self, bindings: &[InputBinding]) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.metadata.capabilities.input_binding_updates,
            "native component does not support input-binding updates"
        );
        self.inner
            .unit(abi::operation::BIND_INPUTS, &wire::encode(&bindings)?)
            .await?;
        self.input_bindings = bindings.to_vec();
        Ok(())
    }
    fn bind_control(&mut self, control: ComponentControl) {
        if self.metadata.capabilities.control {
            control::bind(&self.inner, control);
        }
    }
    fn control_handler(&self) -> Option<Arc<dyn ControlHandler>> {
        self.metadata
            .capabilities
            .control
            .then(|| Arc::new(control::Handler(self.inner.clone())) as Arc<dyn ControlHandler>)
    }
    fn requires_readiness_confirmation(&self) -> bool {
        self.metadata.capabilities.readiness
    }
    async fn initialize(&mut self) -> anyhow::Result<()> {
        self.inner.unit(abi::operation::INITIALIZE, &[]).await
    }
    async fn pause_processing(&mut self) -> anyhow::Result<()> {
        self.inner.unit(abi::operation::PAUSE_PROCESSING, &[]).await
    }
    async fn resume_processing(&mut self) -> anyhow::Result<()> {
        self.inner
            .unit(abi::operation::RESUME_PROCESSING, &[])
            .await
    }
    async fn apply_update(&mut self, context: ComponentUpdateContext) -> anyhow::Result<()> {
        self.apply_managed_update(context).await
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.update
                .get_mut()
                .as_ref()
                .is_none_or(updates::UpdateState::completed_successfully),
            "native component cannot start with an unfinished or failed update"
        );
        if self.inner.stopping.load(Ordering::Acquire)
            || self.inner.retiring.load(Ordering::Acquire)
        {
            return Err(operation_failure(Failure::closed()));
        }
        anyhow::ensure!(
            self.metadata.capabilities.quiescent_bootstrap.is_none()
                || self.metadata.capabilities.bootstrap_optional
                || self.bootstrap.is_some(),
            "native bootstrap consumer requires a bound provider"
        );
        anyhow::ensure!(
            !self
                .bootstrap
                .as_ref()
                .is_some_and(|binding| binding.needs_cleanup()),
            "native bootstrap requires successful cleanup before restarting"
        );
        if self.metadata.capabilities.durable_bootstrap.is_some()
            || self.metadata.capabilities.bootstrap_optional
        {
            let status: drasi_computation_contracts::BootstrapStatus = wire::decode(
                &self
                    .inner
                    .call(abi::operation::BOOTSTRAP_STATUS, &[])
                    .await?,
            )?;
            anyhow::ensure!(
                status.configured == self.bootstrap.is_some(),
                "bootstrap configuration requires its actual provider before initialization"
            );
        }
        self.activated = true;
        *self.stop.get_mut() = StopState::default();
        self.stop.get_mut().cleanup_complete = false;
        if let Some(reads) = &self.output_reads {
            reads.advance();
        }
        {
            let binding = self
                .inner
                .control
                .lock()
                .map_err(|_| anyhow::anyhow!("native control binding poisoned"))?;
            if let Some(error) = &binding.error {
                return Err(operation_failure(error.clone()));
            }
        }
        // Caller-owned resolution is cancelled with this activation. Nothing is
        // cached on the proxy or sent to native code until resolution succeeds.
        let credentials = match &self.identity {
            Some((identity, endpoint)) => wire::encode(&identity.resolve(endpoint).await?)?,
            None => Vec::new(),
        };
        if self.inner.stopping.load(Ordering::Acquire) {
            return Err(operation_failure(Failure::closed()));
        }
        let activation = self
            .bindings
            .consumer_progress
            .as_ref()
            .map(|progress| progress.binding.activate())
            .transpose()?;
        if let Some(binding) = &self.output_snapshots {
            binding.activate()?;
        }
        let progress_call = self
            .processing
            .as_ref()
            .map(|processing| processing.progress_call())
            .transpose()?
            .flatten();
        let result = async {
            if let Some(binding) = &self.query_construction {
                let construction = &binding.construction;
                validate_query_sources(
                    !construction.provider.is_volatile(),
                    &construction.definition.id,
                    &construction.source_hosts,
                )?;
            }
            if let Some(processing) = &mut self.processing {
                processing.open().await?;
            }
            self.inner.unit(abi::operation::START, &credentials).await?;
            if let Some(bootstrap) = &mut self.bootstrap {
                let bindings = self
                    .metadata
                    .capabilities
                    .input_binding_updates
                    .then_some(self.input_bindings.as_slice());
                self.bootstrap_replay =
                    Box::pin(bootstrap.initialize(&self.inner, &self.codec, bindings))
                        .await?
                        .into();
            } else if self.metadata.capabilities.durable_bootstrap.is_some()
                || self.metadata.capabilities.bootstrap_optional
            {
                let status: drasi_computation_contracts::BootstrapStatus = wire::decode(
                    &self
                        .inner
                        .call(abi::operation::BOOTSTRAP_STATUS, &[])
                        .await?,
                )?;
                anyhow::ensure!(
                    !status.configured && status.complete,
                    "durable bootstrap requires a bound provider"
                );
                let completion = if self.metadata.capabilities.durable_bootstrap.is_some() {
                    wire::encode(&drasi_computation_plugin_sdk::bootstrap::Completed::from(
                        drasi_computation_contracts::BootstrapCompletion::default(),
                    ))?
                } else {
                    Vec::new()
                };
                self.inner
                    .unit(abi::operation::BOOTSTRAP_COMPLETE, &completion)
                    .await?;
            }
            if let Some(processing) = &self.processing {
                processing.progress_ready()?;
                if let Some(binding) = &mut self.query_construction {
                    if let Some(scheduling) = &binding.construction.scheduling {
                        processing.bind_scheduling(scheduling)?;
                        binding.session = scheduling.connect_test_control()?;
                    }
                }
            }
            if let Some(api) = &self.query_api {
                api.initialize_native_progress().await?;
            }
            Ok::<(), anyhow::Error>(())
        }
        .await;
        if result.is_err()
            && (self.bootstrap.is_some()
                || self
                    .metadata
                    .capabilities
                    .committed_input_progress
                    .is_some())
        {
            let error = result.expect_err("native initialization failed");
            return match self.stop().await {
                Ok(()) => Err(error),
                Err(cleanup) => Err(drasi_lib::error::OperationFailures::new(
                    "native startup and owned initialization cleanup failed",
                    vec![error, cleanup],
                )
                .into()),
            };
        }
        if result.is_err() {
            if let Some(binding) = &self.output_snapshots {
                binding.deactivate();
            }
            if let Some(processing) = &mut self.processing {
                if let Err(cleanup) = processing.close().await {
                    return Err(drasi_lib::error::OperationFailures::new(
                        "native startup and indexed processing cleanup failed",
                        vec![result.expect_err("startup failed"), cleanup],
                    )
                    .into());
                }
            }
        } else if let Some(activation) = activation {
            activation.complete();
        }
        if result.is_ok() {
            self.pending = self.metadata.capabilities.startup_continuation;
            if let Some(reads) = &self.output_reads {
                reads.recovery_ready();
            }
            if let Some(call) = progress_call {
                call.complete();
            }
        }
        result
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        if self.inner.retiring.load(Ordering::Acquire) {
            return Err(operation_failure(Failure::closed()));
        }
        if let Some(reads) = &self.output_reads {
            reads.advance();
        }
        if let Some(binding) = &mut self.query_construction {
            binding.session.take();
            binding.draining = false;
            if let Some(scheduling) = &binding.construction.scheduling {
                scheduling.suspend();
            }
        }
        self.inner.stopping.store(true, Ordering::Release);
        if let Some(processing) = &self.processing {
            processing.suspend_progress()?;
        }
        if let Some(bootstrap) = &mut self.bootstrap {
            bootstrap.stop().await?;
        }
        if !self.stop.get_mut().completed {
            if self.stop.get_mut().operation.is_none() {
                self.stop.get_mut().operation =
                    Some(self.inner.begin(abi::operation::STOP, &[], None)?);
            }
            // Retain the accepted STOP and its readers across caller cancellation
            // or timeout. A retry polls this same operation.
            let result = self
                .stop
                .get_mut()
                .operation
                .as_mut()
                .expect("owned stop operation")
                .await;
            self.stop.get_mut().operation.take();
            anyhow::ensure!(
                result.map_err(operation_failure)?.is_empty(),
                "native unit operation returned unexpected data"
            );
            self.stop.get_mut().completed = true;
        }
        if let Some(progress) = &self.bindings.consumer_progress {
            progress.binding.deactivate();
        }
        if let Some(binding) = &self.output_snapshots {
            binding.deactivate();
        }
        if let Some(binding) = &self.output_snapshots {
            binding.ensure_quiesced()?;
        }
        if let Some(progress) = &self.bindings.consumer_progress {
            progress.binding.stopped()?;
        }
        if let Some(processing) = &mut self.processing {
            processing.close().await?;
        }
        self.stop.get_mut().cleanup_complete = true;
        self.bootstrap_replay.clear();
        if let Some(reads) = &self.output_reads {
            reads.recovery_ready();
        }
        self.inner.stopping.store(false, Ordering::Release);
        Ok(())
    }
    async fn delete_owned_resources(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.metadata.capabilities.delete_owned_resources,
            "native component does not support permanent state removal"
        );
        anyhow::ensure!(
            self.stop.get_mut().cleanup_complete && !self.inner.stopping.load(Ordering::Acquire),
            "native resource deletion requires successfully stopped work"
        );
        self.inner.retiring.store(true, Ordering::Release);
        if let Some(reads) = &self.output_reads {
            reads.retire();
        }
        self.activated = true;
        let result = async {
            if !self.deletion.get_mut().completed && self.deletion.get_mut().operation.is_none() {
                if let Some(processing) = &mut self.processing {
                    if processing.needs_cleanup()? {
                        processing.close().await?;
                    }
                    processing.open().await?;
                }
            }
            let state = self.deletion.get_mut();
            if !state.completed {
                if state.operation.is_none() {
                    state.operation = Some(self.inner.begin(
                        abi::operation::DELETE_OWNED_RESOURCES,
                        &[],
                        None,
                    )?);
                    state.started = true;
                }
                let result = state
                    .operation
                    .as_mut()
                    .expect("owned resource-deletion operation")
                    .await;
                state.operation.take();
                anyhow::ensure!(
                    result.map_err(operation_failure)?.is_empty(),
                    "native unit operation returned unexpected data"
                );
                state.completed = true;
            }
            Ok::<(), anyhow::Error>(())
        }
        .await;
        let cleanup = match &mut self.processing {
            Some(processing) => processing.close().await,
            None => Ok(()),
        };
        match (result, cleanup) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
            (Err(error), Err(cleanup)) => Err(drasi_lib::error::OperationFailures::new(
                "native state deletion and processing cleanup failed",
                vec![error, cleanup],
            )
            .into()),
        }
    }
    async fn dispose(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.stop.get_mut().cleanup_complete && !self.inner.stopping.load(Ordering::Acquire),
            "native disposal requires successfully stopped work"
        );
        anyhow::ensure!(
            !self.deletion.get_mut().started || self.deletion.get_mut().completed,
            "native resource deletion must finish before disposal"
        );
        self.inner.retiring.store(true, Ordering::Release);
        if let Some(schema) = &self.source_schema {
            schema.retire();
        }
        if let Some(reads) = &self.output_reads {
            reads.retire();
        }
        if let Some(processing) = &mut self.processing {
            processing.close().await?;
        }
        let state = self.disposal.get_mut();
        if state.completed {
            return Ok(());
        }
        if state.operation.is_none() {
            state.operation = Some(self.inner.begin(abi::operation::DISPOSE, &[], None)?);
            state.started = true;
        }
        let result = state
            .operation
            .as_mut()
            .expect("owned dispose operation")
            .await;
        state.operation.take();
        anyhow::ensure!(
            result.map_err(operation_failure)?.is_empty(),
            "native disposal returned unexpected data"
        );
        state.completed = true;
        Ok(())
    }
}
#[async_trait]
impl EnvelopeSource for NativeComponentProxy {
    fn recovery_progress(&self) -> Option<Arc<QuerySourceProgress>> {
        self.bindings
            .progress
            .as_ref()
            .map(|progress| progress.owner.clone())
    }
    fn admission(&self) -> Option<Arc<SourceAdmission>> {
        self.bindings
            .admission
            .as_ref()
            .map(|(admission, _)| admission.clone())
    }
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        let reply = self.inner.call(abi::operation::NEXT, &[]).await?;
        let output: Option<wire::Envelope<'_>> = wire::decode(&reply)?;
        let output = output
            .map(|output| output.into_output(&self.codec))
            .transpose()?;
        if let Some(output) = &output {
            wire::check_port(
                &self.descriptor,
                &output.port,
                &output.envelope,
                PortDirection::Output,
            )?;
        }
        Ok(output)
    }
}
#[async_trait]
impl Transformer for NativeComponentProxy {
    async fn bind_output_destinations(
        &mut self,
        bindings: &drasi_lib::computation::v1::OutputBindings,
    ) -> anyhow::Result<()> {
        match &mut self.processing {
            Some(processing) => processing.bind_outputs(bindings).await?,
            None => {
                anyhow::ensure!(
                    bindings.destinations().is_empty(),
                    "native transformer does not track durable output destinations"
                );
            }
        }
        if self.metadata.capabilities.durable_output {
            let call = self
                .processing
                .as_ref()
                .expect("durable processing")
                .progress_call()?;
            self.inner
                .unit(abi::operation::DELIVERY_RESUME, &[])
                .await?;
            self.pending = true;
            if let Some(call) = call {
                call.complete();
            }
        }
        Ok(())
    }
    async fn transform(&mut self, input: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>> {
        let progress_call = self.begin_processing()?;
        let input = self.input(&input)?;
        let outputs = self.outputs(abi::operation::TRANSFORM, &input).await?;
        if let Some(call) = progress_call {
            call.complete();
        }
        Ok(outputs)
    }
    async fn delivery_completed(&mut self, outputs: &[OutputEnvelope]) -> anyhow::Result<()> {
        let progress_call = self.begin_processing()?;
        let outputs = outputs
            .iter()
            .map(|output| wire::Envelope::output(output, &self.codec))
            .collect::<anyhow::Result<Vec<_>>>()?;
        self.inner
            .unit(abi::operation::DELIVERY_COMPLETED, &wire::encode(&outputs)?)
            .await?;
        if let Some(call) = progress_call {
            call.complete();
        }
        Ok(())
    }
    fn has_pending_emissions(&self) -> bool {
        self.pending
            || !self.bootstrap_replay.is_empty()
            || self
                .query_construction
                .as_ref()
                .is_some_and(|binding| binding.draining)
    }
    async fn continue_transform(&mut self) -> anyhow::Result<Vec<OutputEnvelope>> {
        if !self.pending {
            if let Some(input) = self.bootstrap_replay.front().cloned() {
                let outputs = self.transform(input).await?;
                self.bootstrap_replay.pop_front();
                return Ok(outputs);
            }
        }
        let progress_call = self.begin_processing()?;
        let outputs = self.outputs(abi::operation::CONTINUE, &[]).await?;
        self.finish_query_drain(&outputs).await?;
        if let Some(call) = progress_call {
            call.complete();
        }
        Ok(outputs)
    }
    fn wakeup_source(&self) -> Option<Arc<dyn WakeupSource>> {
        if let Some(binding) = &self.query_construction {
            if binding.construction.scheduling.is_some() {
                return binding
                    .session
                    .as_ref()
                    .and_then(QuerySchedulingSession::wakeup_source);
            }
        }
        self.metadata
            .capabilities
            .wakeups
            .then(|| Arc::new(NativeWakeup(self.inner.clone())) as Arc<dyn WakeupSource>)
    }
    async fn on_wakeup(&mut self) -> anyhow::Result<Vec<OutputEnvelope>> {
        if !self.pending && self.bootstrap_replay.is_empty() {
            if let Some(binding) = &mut self.query_construction {
                if let Some(session) = &mut binding.session {
                    if !session.begin() {
                        return Ok(Vec::new());
                    }
                    binding.draining = true;
                }
            }
        }
        let progress_call = self.begin_processing()?;
        let outputs = self.outputs(abi::operation::ON_WAKEUP, &[]).await?;
        self.finish_query_drain(&outputs).await?;
        if let Some(call) = progress_call {
            call.complete();
        }
        Ok(outputs)
    }
}
#[async_trait]
impl EnvelopeSink for NativeComponentProxy {
    fn batching(&self) -> anyhow::Result<Option<SinkBatching>> {
        let Some(extension) = &self.batching else {
            return Ok(None);
        };
        let bytes = unsafe {
            take_reply(extension.policy.expect("validated")(self.inner.state()))
                .map_err(operation_failure)?
        };
        let policy: Option<SinkBatching> = wire::decode(&bytes)?;
        if let Some(policy) = policy {
            policy.validate()?;
        }
        Ok(policy)
    }
    async fn handle_batch(&mut self, inputs: Vec<InputEnvelope>) -> anyhow::Result<()> {
        let policy = self
            .batching()?
            .ok_or_else(|| anyhow::anyhow!("native batching is disabled"))?;
        policy.validate_inputs(&inputs)?;
        let batch = inputs
            .iter()
            .map(|input| {
                wire::check_port(
                    &self.descriptor,
                    &input.port,
                    &input.envelope,
                    PortDirection::Input,
                )?;
                wire::Envelope::input(input, &self.codec)
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        self.inner
            .unit(abi::operation::HANDLE_BATCH, &wire::encode(&batch)?)
            .await
    }
    /// Exposed only for the input ports the factory metadata lists, which the
    /// plugin validated against its actual handlers at construction.
    fn snapshot_consumer(&mut self, port: &PortId) -> Option<&mut dyn SnapshotConsumer> {
        if self.metadata.capabilities.snapshot_inputs.contains(port) {
            Some(self)
        } else {
            None
        }
    }
    fn completion(&self) -> SinkCompletion {
        self.metadata.completion.expect("native sink role")
    }
    async fn handle(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        self.inner
            .unit(abi::operation::HANDLE, &self.input(&input)?)
            .await
    }
}
#[async_trait]
impl SnapshotConsumer for NativeComponentProxy {
    async fn replace_snapshot(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.metadata
                .capabilities
                .snapshot_inputs
                .contains(&input.port),
            "native sink input {} does not support snapshot replacement",
            input.port
        );
        self.inner
            .unit(abi::operation::SNAPSHOT, &self.input(&input)?)
            .await
    }
}
#[async_trait]
impl ComputationService for NativeComponentProxy {
    async fn run(&mut self) -> anyhow::Result<()> {
        self.inner.unit(abi::operation::RUN, &[]).await
    }
}
struct NativeWakeup(Arc<RemoteComponent>);
#[async_trait]
impl WakeupSource for NativeWakeup {
    async fn wait(&self) -> anyhow::Result<()> {
        self.0.unit(abi::operation::WAKE_WAIT, &[]).await
    }
    async fn has_pending(&self) -> anyhow::Result<bool> {
        wire::decode(&self.0.call(abi::operation::WAKE_PENDING, &[]).await?)
    }
}

pub(super) struct NativeTransactionalProxy {
    component: NativeComponentProxy,
    input: Arc<Schema>,
    output: Arc<Schema>,
}
#[async_trait]
impl ComputationComponent for NativeTransactionalProxy {
    fn descriptor(&self) -> &ComponentDescriptor {
        self.component.descriptor()
    }
    fn configuration(&self) -> anyhow::Result<serde_json::Value> {
        self.component.configuration()
    }
    fn output_readers(&self) -> OutputReaders {
        self.component.output_readers()
    }
    async fn initialize(&mut self) -> anyhow::Result<()> {
        self.component.initialize().await
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        self.component.start().await
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        self.component.stop().await
    }
    async fn pause_processing(&mut self) -> anyhow::Result<()> {
        self.component.pause_processing().await
    }
    async fn resume_processing(&mut self) -> anyhow::Result<()> {
        self.component.resume_processing().await
    }
    async fn apply_update(&mut self, context: ComponentUpdateContext) -> anyhow::Result<()> {
        self.component.apply_update(context).await
    }
    async fn dispose(&mut self) -> anyhow::Result<()> {
        self.component.dispose().await
    }
}
#[async_trait]
impl Transformer for NativeTransactionalProxy {
    async fn transform(&mut self, input: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>> {
        self.component.transform(input).await
    }
    async fn delivery_completed(&mut self, outputs: &[OutputEnvelope]) -> anyhow::Result<()> {
        self.component.delivery_completed(outputs).await
    }
}
#[async_trait]
impl TransactionalTransformer for NativeTransactionalProxy {
    fn transaction_input_schema(&self) -> Arc<Schema> {
        self.input.clone()
    }
    fn transaction_output_schema(&self) -> Arc<Schema> {
        self.output.clone()
    }
    async fn transform_in_transaction(
        &self,
        input: ChangeEnvelope,
        context: &TransactionContext<'_>,
    ) -> anyhow::Result<ChangeEnvelope> {
        anyhow::ensure!(
            context.step_id() == self.component.descriptor.id(),
            "native transaction step identity mismatch"
        );
        anyhow::ensure!(
            input.changes().schema() == self.input.descriptor(),
            "native transaction input schema mismatch"
        );
        let output = super::transaction::run(&self.component, input, context).await?;
        anyhow::ensure!(
            output.changes().schema() == self.output.descriptor(),
            "native transaction output schema mismatch"
        );
        Ok(output)
    }
}
