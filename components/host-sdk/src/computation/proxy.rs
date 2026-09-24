// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use async_trait::async_trait;
use drasi_computation_plugin_abi as abi;
use drasi_computation_plugin_sdk::{
    metadata::FactoryMetadata,
    transport::{checked_table, take_reply, take_status, Failure, OperationFuture},
    wire::{self, InstanceMetadata},
};
use drasi_lib::computation::v1::*;
use std::sync::{Arc, Mutex};

use super::{
    control::{self, ControlBinding},
    loader::PluginOwner,
};

pub(super) struct RemoteComponent {
    raw: abi::ComponentHandle,
    _plugin: Arc<PluginOwner>,
    pub(super) control: Mutex<ControlBinding>,
}
unsafe impl Send for RemoteComponent {}
unsafe impl Sync for RemoteComponent {}
impl RemoteComponent {
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
        })
    }
    fn table(&self) -> &abi::ComponentVTable {
        unsafe { &*self.raw.vtable }
    }
    fn inspect(&self) -> anyhow::Result<InstanceMetadata> {
        let bytes =
            unsafe { take_reply(self.table().inspect.expect("validated")(self.raw.state))? };
        Ok(serde_json::from_slice(&bytes)?)
    }
    fn configuration(&self) -> anyhow::Result<serde_json::Value> {
        let bytes = unsafe {
            take_reply(self.table().configuration.expect("validated")(
                self.raw.state,
            ))?
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
            ))?;
            Ok(OperationFuture::new(operation)?)
        }
    }
    pub(super) async fn call(&self, code: u32, input: &[u8]) -> anyhow::Result<Vec<u8>> {
        Ok(self.begin(code, input, None)?.await?)
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
    pub(super) codec: Arc<EnvelopeCodec>,
    schemas: Vec<Arc<Schema>>,
    pending: bool,
}
impl NativeComponentProxy {
    pub(super) unsafe fn new(
        raw: abi::ComponentHandle,
        metadata: FactoryMetadata,
        id: &ComponentId,
        codec: Arc<EnvelopeCodec>,
        schemas: Vec<Arc<Schema>>,
        plugin: Arc<PluginOwner>,
    ) -> anyhow::Result<Self> {
        let inner = Arc::new(unsafe { RemoteComponent::new(raw, plugin)? });
        let actual = inner.inspect()?;
        anyhow::ensure!(
            actual.descriptor == metadata.descriptor(id.clone())?
                && actual.capabilities == metadata.capabilities,
            "constructed native component changed its descriptor/capabilities"
        );
        metadata.configuration.validate(&inner.configuration()?)?;
        Ok(Self {
            inner,
            descriptor: actual.descriptor,
            metadata,
            codec,
            schemas,
            pending: false,
        })
    }
    pub fn metadata(&self) -> &FactoryMetadata {
        &self.metadata
    }
    async fn outputs(&mut self, code: u32, input: &[u8]) -> anyhow::Result<Vec<OutputEnvelope>> {
        let result: wire::OutputBatch = wire::decode(&self.inner.call(code, input).await?)?;
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
        self.inner.revoke_control();
    }
}

#[async_trait]
impl ComputationComponent for NativeComponentProxy {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
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
        {
            let binding = self
                .inner
                .control
                .lock()
                .map_err(|_| anyhow::anyhow!("native control binding poisoned"))?;
            if let Some(error) = &binding.error {
                return Err(error.clone().into());
            }
        }
        self.inner.unit(abi::operation::START, &[]).await
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        self.inner.unit(abi::operation::STOP, &[]).await
    }
}
#[async_trait]
impl EnvelopeSource for NativeComponentProxy {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        let output: Option<wire::Envelope> =
            wire::decode(&self.inner.call(abi::operation::NEXT, &[]).await?)?;
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
    fn supports_snapshot(&self) -> bool {
        self.metadata.capabilities.snapshot
    }
    async fn replace_snapshot(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        self.inner
            .unit(abi::operation::SNAPSHOT, &self.input(&input)?)
            .await
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
