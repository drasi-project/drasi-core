// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

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
        if code != abi::operation::STOP && self.stopping.load(Ordering::Acquire) {
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
    pub(super) codec: Arc<BinaryEnvelopeCodec>,
    schemas: Vec<Arc<Schema>>,
    pending: bool,
    bindings: SourceBindings,
    batching: Option<abi::batching::PluginBatchingV1>,
    output_snapshots: Option<super::output_snapshots::OutputSnapshotsBinding>,
    source_schema: Option<Arc<super::source_schema::NativeSourceSchemaReader>>,
    output_reads: Option<super::output_reads::NativeOutputReads>,
    identity: Option<(
        Arc<super::NativeIdentityResource>,
        drasi_computation_plugin_sdk::ActivationCredentialContext,
    )>,
    stop: tokio::sync::Mutex<StopState>,
}

#[derive(Default)]
struct StopState {
    operation: Option<OperationFuture>,
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
        id: &ComponentId,
        codec: Arc<BinaryEnvelopeCodec>,
        schemas: Vec<Arc<Schema>>,
        plugin: Arc<PluginOwner>,
        bindings: SourceBindings,
    ) -> anyhow::Result<Self> {
        let inner = Arc::new(unsafe { RemoteComponent::new(raw, plugin)? });
        let mut bindings = bindings;
        let actual = inner.inspect()?;
        anyhow::ensure!(
            actual.descriptor == metadata.descriptor(id.clone())?
                && actual.capabilities == metadata.capabilities,
            "constructed native component changed its descriptor/capabilities"
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
            codec,
            schemas,
            pending: false,
            bindings,
            batching: None,
            output_snapshots: None,
            source_schema: None,
            output_reads: None,
            identity: None,
            stop: tokio::sync::Mutex::new(StopState::default()),
        })
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
        extension: Option<abi::output_reads::PluginOutputReadsV1>,
        follow: Option<abi::output_follow::PluginOutputFollowV1>,
    ) -> anyhow::Result<Self> {
        self.output_reads = super::output_reads::NativeOutputReads::bind(
            extension,
            follow,
            &self.inner,
            &self.descriptor,
            &self.metadata,
            &self.schemas,
            &self.codec,
        )?;
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
    async fn outputs(&mut self, code: u32, input: &[u8]) -> anyhow::Result<Vec<OutputEnvelope>> {
        let reply = self.inner.call(code, input).await?;
        let result: wire::OutputBatch<'_> = wire::decode(&reply)?;
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
    async fn start(&mut self) -> anyhow::Result<()> {
        if self.inner.stopping.load(Ordering::Acquire) {
            return Err(operation_failure(Failure::closed()));
        }
        *self.stop.get_mut() = StopState::default();
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
        let result = self.inner.unit(abi::operation::START, &credentials).await;
        if result.is_err() {
            if let Some(binding) = &self.output_snapshots {
                binding.deactivate();
            }
        } else if let Some(activation) = activation {
            activation.complete();
        }
        result
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        if let Some(reads) = &self.output_reads {
            reads.advance();
        }
        self.inner.stopping.store(true, Ordering::Release);
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
        self.inner.stopping.store(false, Ordering::Release);
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
    async fn transform(&mut self, input: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>> {
        let input = self.input(&input)?;
        self.outputs(abi::operation::TRANSFORM, &input).await
    }
    async fn delivery_completed(&mut self, outputs: &[OutputEnvelope]) -> anyhow::Result<()> {
        let outputs = outputs
            .iter()
            .map(|output| wire::Envelope::output(output, &self.codec))
            .collect::<anyhow::Result<Vec<_>>>()?;
        self.inner
            .unit(abi::operation::DELIVERY_COMPLETED, &wire::encode(&outputs)?)
            .await
    }
    fn has_pending_emissions(&self) -> bool {
        self.pending
    }
    async fn continue_transform(&mut self) -> anyhow::Result<Vec<OutputEnvelope>> {
        self.outputs(abi::operation::CONTINUE, &[]).await
    }
    fn wakeup_source(&self) -> Option<Arc<dyn WakeupSource>> {
        self.metadata
            .capabilities
            .wakeups
            .then(|| Arc::new(NativeWakeup(self.inner.clone())) as Arc<dyn WakeupSource>)
    }
    async fn on_wakeup(&mut self) -> anyhow::Result<Vec<OutputEnvelope>> {
        self.outputs(abi::operation::ON_WAKEUP, &[]).await
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
    async fn quiesce(&mut self) -> anyhow::Result<()> {
        self.inner.unit(abi::operation::QUIESCE, &[]).await
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
    async fn start(&mut self) -> anyhow::Result<()> {
        self.component.start().await
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        self.component.stop().await
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
