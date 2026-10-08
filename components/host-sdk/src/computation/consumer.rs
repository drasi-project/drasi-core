// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::{
    proxy::{NativeComponentProxy, RemoteComponent},
    transaction::{RequestScope, TransactionRequests},
};
use async_trait::async_trait;
use drasi_computation_plugin_abi::{self as abi, consumer::PluginConsumerV1};
use drasi_computation_plugin_sdk::{
    consumer::{BeginBatch, ConsumerMode, HandleOperation},
    transport::{take_status, Failure, OperationFuture},
    wire, Scope,
};
use drasi_core::{
    computation::{ComputationIndexProvider, ComputationIndexes, ComputationResourceCleanup},
    interface::StorageDurability,
};
use drasi_lib::computation::v1::*;
use std::{num::NonZeroUsize, sync::Arc};

/// Actual graph-owned delivery binding; neither the provider nor its options
/// are exposed to the plugin. Ordinary sinks do not require this resource.
pub type NativeConsumerResource = ConsumerDeliveryResource;

/// Opt-in native sink whose progress/transactions remain owned by DeliveryRunner.
/// No consumer ledger or request machinery is constructed by ordinary factories.
pub struct NativeConsumerProxy {
    component: NativeComponentProxy,
    extension: PluginConsumerV1,
    mode: ConsumerMode,
    scope: DeliveryScope,
    graph: String,
    provider: Arc<dyn ComputationIndexProvider>,
    schemas: Vec<Arc<Schema>>,
    options: DeliveryOptions,
    runner: Option<DeliveryRunner>,
    pending_cleanup: Option<Arc<dyn ComputationResourceCleanup>>,
    durability: StorageDurability,
    recovery: ComponentRecovery,
    retirement: Arc<DeliveryRetirementState>,
    running: bool,
    cleanup_required: bool,
}
impl NativeConsumerProxy {
    pub(super) async fn new(
        component: NativeComponentProxy,
        extension: PluginConsumerV1,
        mode: ConsumerMode,
        scope: Scope,
        provider: Arc<dyn ComputationIndexProvider>,
        options: DeliveryOptions,
        schemas: Vec<Arc<Schema>>,
    ) -> anyhow::Result<Self> {
        let scope_id = DeliveryScope::new(
            scope.instance_id,
            scope.graph_id.clone(),
            component.descriptor().id().clone(),
        )?;
        let codec = codec(&schemas)?;
        let indexes = provider
            .create_indexes(&scope.graph_id, component.descriptor().id().as_str())
            .await?;
        let durability = indexes.durability();
        let (runner, recovery) =
            open_runner(indexes, mode, &scope_id, codec, &options, None).await?;
        let retirement = DeliveryRetirementState::new(&provider);
        let recovery = recovery.with_delivery_retirement(retirement.clone());
        Ok(Self {
            component,
            extension,
            mode,
            scope: scope_id,
            graph: scope.graph_id,
            provider,
            schemas,
            options,
            runner: Some(runner),
            pending_cleanup: None,
            durability,
            recovery,
            retirement,
            running: false,
            cleanup_required: false,
        })
    }
    async fn reopen(&mut self) -> anyhow::Result<()> {
        self.retirement.invalidate()?;
        if self.runner.is_some() {
            return Ok(());
        }
        let codec = codec(&self.schemas)?;
        let indexes = self
            .provider
            .create_indexes(&self.graph, self.component.descriptor().id().as_str())
            .await?;
        self.pending_cleanup = indexes.cleanup().cloned();
        let (runner, recovery) = assemble_runner(
            indexes,
            self.mode,
            &self.scope,
            codec,
            &self.options,
            Some(self.durability),
        )?;
        self.runner = Some(runner);
        self.pending_cleanup = None;
        anyhow::ensure!(
            recovery.with_delivery_retirement(self.retirement.clone()) == self.recovery,
            "native consumer recovery contract changed"
        );
        Ok(())
    }
    pub async fn progress(&mut self) -> anyhow::Result<Vec<DeliveryProgress>> {
        self.runner
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("native consumer progress is closed"))?
            .progress()
            .await
    }
    fn validate_mode(&self) -> anyhow::Result<()> {
        let response = unsafe {
            drasi_computation_plugin_sdk::transport::take_reply(self
                .extension
                .inspect
                .expect("validated")(
                self.component.inner.state()
            ))?
        };
        let actual: drasi_computation_plugin_sdk::consumer::FactoryConsumer =
            wire::decode(&response)?;
        anyhow::ensure!(
            actual.version == abi::consumer::VERSION && actual.mode == Some(self.mode),
            "native consumer changed its negotiated mode"
        );
        Ok(())
    }
}
impl Drop for NativeConsumerProxy {
    fn drop(&mut self) {
        if let Some(cleanup) = &self.pending_cleanup {
            cleanup.cancel();
        }
    }
}
struct CleanupOnError(Option<Arc<dyn ComputationResourceCleanup>>);
impl Drop for CleanupOnError {
    fn drop(&mut self) {
        if let Some(cleanup) = &self.0 {
            cleanup.cancel();
        }
    }
}
async fn open_runner(
    indexes: ComputationIndexes,
    mode: ConsumerMode,
    scope: &DeliveryScope,
    codec: EnvelopeCodec,
    options: &DeliveryOptions,
    expected: Option<StorageDurability>,
) -> anyhow::Result<(DeliveryRunner, ComponentRecovery)> {
    let mut cleanup = CleanupOnError(indexes.cleanup().cloned());
    let result = assemble_runner(indexes, mode, scope, codec, options, expected);
    match result {
        Ok(value) => {
            cleanup.0 = None;
            Ok(value)
        }
        Err(error) => {
            if let Some(owner) = &cleanup.0 {
                owner.cancel();
                if let Err(cleanup_error) = owner.shutdown().await {
                    return Err(drasi_lib::error::OperationFailures::new(
                        "native consumer construction and storage cleanup failed",
                        vec![error, cleanup_error.into()],
                    )
                    .into());
                }
            }
            cleanup.0 = None;
            Err(error)
        }
    }
}
fn assemble_runner(
    indexes: ComputationIndexes,
    mode: ConsumerMode,
    scope: &DeliveryScope,
    codec: EnvelopeCodec,
    options: &DeliveryOptions,
    expected: Option<StorageDurability>,
) -> anyhow::Result<(DeliveryRunner, ComponentRecovery)> {
    anyhow::ensure!(
        expected.is_none_or(|expected| indexes.durability() == expected),
        "native consumer storage guarantee changed on restart"
    );
    let recovery = match mode {
        ConsumerMode::External => ComponentRecovery::default(),
        ConsumerMode::Transactional => ComponentRecovery::transactional_consumer(&indexes)?,
    };
    let runner = match mode {
        ConsumerMode::External => {
            DeliveryRunner::new(scope.clone(), indexes, codec, options.clone())
        }
        ConsumerMode::Transactional => {
            DeliveryRunner::new_transactional(scope.clone(), indexes, codec, options.clone())
        }
    }?;
    Ok((runner, recovery))
}
fn codec(schemas: &[Arc<Schema>]) -> anyhow::Result<EnvelopeCodec> {
    let mut codec =
        EnvelopeCodec::new(NonZeroUsize::new(abi::MAX_MESSAGE_BYTES).expect("wire bound"));
    for schema in schemas {
        codec.register_schema(schema.clone())?;
    }
    Ok(codec)
}
#[async_trait]
impl ComputationComponent for NativeConsumerProxy {
    fn descriptor(&self) -> &ComponentDescriptor {
        self.component.descriptor()
    }
    fn configuration(&self) -> anyhow::Result<serde_json::Value> {
        self.component.configuration()
    }
    fn recovery_contract(&self) -> ComponentRecovery {
        self.recovery.clone()
    }
    fn bind_control(&mut self, control: ComponentControl) {
        self.component.bind_control(control);
    }
    fn control_handler(&self) -> Option<Arc<dyn ControlHandler>> {
        self.component.control_handler()
    }
    fn requires_readiness_confirmation(&self) -> bool {
        self.component.requires_readiness_confirmation()
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.running && !self.cleanup_required,
            "native consumer requires completed cleanup before starting"
        );
        self.cleanup_required = true;
        self.reopen().await?;
        self.runner.as_mut().expect("opened").progress().await?;
        self.validate_mode()?;
        self.component.start().await?;
        self.validate_mode()?;
        self.running = true;
        self.cleanup_required = false;
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        self.running = false;
        self.cleanup_required = true;
        self.component.stop().await?;
        if let Some(runner) = &mut self.runner {
            self.retirement.close(runner).await?;
        }
        self.runner = None;
        if let Some(cleanup) = &self.pending_cleanup {
            cleanup.cancel();
            cleanup.shutdown().await?;
        }
        self.pending_cleanup = None;
        self.cleanup_required = false;
        Ok(())
    }
}
#[async_trait]
impl EnvelopeSink for NativeConsumerProxy {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.running && !self.cleanup_required,
            "native consumer is not ready"
        );
        wire::check_port(
            self.component.descriptor(),
            &input.port,
            &input.envelope,
            PortDirection::Input,
        )?;
        self.cleanup_required = true;
        let runner = self.runner.as_mut().expect("running consumer");
        let mut batch = Batch::begin(
            self.component.inner.clone(),
            self.extension,
            self.component.codec.clone(),
            &input,
            runner.batch_identity(&input)?,
        )
        .await?;
        match self.mode {
            ConsumerMode::External => runner.deliver(&input, &mut batch).await?,
            ConsumerMode::Transactional => runner.deliver_transactional(&input, &batch).await?,
        }
        batch.finish()?;
        self.cleanup_required = false;
        Ok(())
    }
}

struct Batch {
    component: Arc<RemoteComponent>,
    extension: PluginConsumerV1,
    codec: Arc<BinaryEnvelopeCodec>,
    generation: u64,
    active: bool,
}
impl Batch {
    async fn begin(
        component: Arc<RemoteComponent>,
        extension: PluginConsumerV1,
        codec: Arc<BinaryEnvelopeCodec>,
        input: &InputEnvelope,
        identity: DeliveryBatchIdentity,
    ) -> anyhow::Result<Self> {
        let input = BeginBatch {
            port: input.port.clone(),
            envelope: codec.encode(&input.envelope)?.into(),
            identity,
        };
        let operation = begin(
            &component,
            extension,
            abi::consumer::BEGIN_BATCH,
            &wire::encode(&input)?,
            None,
        )?;
        let generation: u64 = wire::decode(&operation.await?)?;
        anyhow::ensure!(
            generation != 0,
            "native consumer returned invalid batch generation"
        );
        Ok(Self {
            component,
            extension,
            codec,
            generation,
            active: true,
        })
    }
    fn request(&self, item: &DeliveryItem<'_>) -> anyhow::Result<Vec<u8>> {
        wire::encode(&HandleOperation {
            generation: self.generation,
            index: item.position.operation(),
            id: item.id.as_str().into(),
        })
    }
    fn finish(&mut self) -> anyhow::Result<()> {
        self.active = false;
        unsafe {
            take_status(self.extension.end_batch.expect("validated")(
                self.component.state(),
                self.generation,
            ))?
        };
        Ok(())
    }
}
impl Drop for Batch {
    fn drop(&mut self) {
        if self.active {
            if let Err(error) = self.finish() {
                tracing::error!(%error, "native consumer batch cleanup failed; component cleanup remains required");
            }
        }
    }
}
fn begin(
    component: &RemoteComponent,
    extension: PluginConsumerV1,
    code: u32,
    input: &[u8],
    context: Option<&abi::Transaction>,
) -> anyhow::Result<OperationFuture> {
    let mut operation = abi::OperationHandle::null();
    unsafe {
        take_status(extension.begin.expect("validated")(
            component.state(),
            code,
            abi::BorrowedBytes::new(input),
            context.map_or(std::ptr::null(), |context| context),
            &mut operation,
        ))?;
        Ok(OperationFuture::new(operation)?)
    }
}
fn retryable(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<Failure>()
        .is_some_and(|failure| failure.code == abi::consumer::RETRYABLE)
}
#[async_trait]
impl DeliveryHandler for Batch {
    fn retryable(&self, error: &anyhow::Error) -> bool {
        retryable(error)
    }
    async fn handle(&mut self, item: DeliveryItem<'_>) -> anyhow::Result<()> {
        let operation = begin(
            &self.component,
            self.extension,
            abi::consumer::HANDLE,
            &self.request(&item)?,
            None,
        )?;
        anyhow::ensure!(
            operation.await?.is_empty(),
            "native consumer handling returned unexpected data"
        );
        Ok(())
    }
}
#[async_trait]
impl TransactionalDeliveryHandler for Batch {
    fn retryable(&self, error: &anyhow::Error) -> bool {
        retryable(error)
    }
    async fn handle(
        &self,
        item: DeliveryItem<'_>,
        context: &TransactionContext<'_>,
    ) -> anyhow::Result<()> {
        let mut scope = RequestScope::new(
            abi::transaction::GET..=abi::transaction::DERIVE,
            abi::MAX_MESSAGE_BYTES,
        );
        let operation = {
            let table = scope.table();
            begin(
                &self.component,
                self.extension,
                abi::consumer::HANDLE,
                &self.request(&item)?,
                Some(&table),
            )?
        };
        let handler = TransactionRequests {
            context,
            codec: &self.codec,
        };
        tokio::pin!(operation);
        let result = tokio::select! {
            biased;
            result = &mut operation => result.map_err(anyhow::Error::from),
            result = scope.serve(&handler) => {
                result?;
                unreachable!("request service never completes successfully")
            }
        };
        scope.revoke();
        anyhow::ensure!(
            result?.is_empty(),
            "native transactional consumer returned unexpected data"
        );
        scope.finish()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::computation::tests::consumer_tests::{
        batch, factory, id, options, plugin, provider, scope,
    };
    use serde_json::json;

    #[tokio::test(flavor = "current_thread")]
    async fn native_consumer_rejects_forged_content_stale_batches_and_oversized_controls(
    ) -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let mut consumer = factory(plugin()?.as_ref(), ConsumerMode::External)
            .create_consumer(
                id("consumer"),
                json!({"path":directory.path().join("effects.db")}),
                scope(),
                provider(&directory.path().join("progress"))?,
                options(),
            )
            .await?;
        consumer.start().await?;
        let input = batch()?;
        let identity = consumer.runner.as_ref().unwrap().batch_identity(&input)?;
        let mut forged = serde_json::to_value(&identity)?;
        forged["digest"][0] = json!(forged["digest"][0].as_u64().unwrap() ^ 1);
        let remote = consumer.component.inner.clone();
        let extension = consumer.extension;
        let codec = consumer.component.codec.clone();
        let request = BeginBatch {
            port: input.port.clone(),
            envelope: codec.encode(&input.envelope)?.into(),
            identity: serde_json::from_value(forged)?,
        };
        assert!(begin(
            &remote,
            extension,
            abi::consumer::BEGIN_BATCH,
            &wire::encode(&request)?,
            None
        )?
        .await
        .is_err());
        let mut first = Batch::begin(
            remote.clone(),
            extension,
            codec.clone(),
            &input,
            identity.clone(),
        )
        .await?;
        assert!(Batch::begin(
            remote.clone(),
            extension,
            codec.clone(),
            &input,
            identity.clone()
        )
        .await
        .is_err());
        let local = DeliveryBatch::new(
            consumer.scope.clone(),
            input.clone(),
            &super::codec(&consumer.schemas)?,
        )?;
        let item = local.item(0)?;
        for (index, key) in [(0, "forged"), (3, item.id.as_str())] {
            let request = HandleOperation {
                generation: first.generation,
                index,
                id: key.into(),
            };
            assert!(begin(
                &remote,
                extension,
                abi::consumer::HANDLE,
                &wire::encode(&request)?,
                None
            )?
            .await
            .is_err());
        }
        assert!(first.request(&item)?.len() < abi::consumer::MAX_CONTROL_BYTES);
        assert!(begin(
            &remote,
            extension,
            abi::consumer::HANDLE,
            &vec![0; abi::consumer::MAX_CONTROL_BYTES + 1],
            None
        )
        .is_err());
        first.finish()?;
        let mut second = Batch::begin(remote.clone(), extension, codec, &input, identity).await?;
        assert!(second.generation > first.generation);
        assert!(first.finish().is_err());
        assert!(begin(
            &remote,
            extension,
            abi::consumer::HANDLE,
            &first.request(&item)?,
            None
        )?
        .await
        .is_err());
        DeliveryHandler::handle(&mut second, local.item(0)?).await?;
        second.finish()?;
        consumer.stop().await?;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn native_consumer_active_call_blocks_batch_retirement_and_cancel_fences_reuse(
    ) -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let mut consumer = factory(plugin()?.as_ref(), ConsumerMode::External)
            .create_consumer(
                id("consumer"),
                json!({"mode":"pending"}),
                scope(),
                provider(directory.path())?,
                options(),
            )
            .await?;
        consumer.start().await?;
        let input = batch()?;
        let identity = consumer.runner.as_ref().unwrap().batch_identity(&input)?;
        let remote = consumer.component.inner.clone();
        let mut batch = Batch::begin(
            remote.clone(),
            consumer.extension,
            consumer.component.codec.clone(),
            &input,
            identity.clone(),
        )
        .await?;
        let local = DeliveryBatch::new(
            consumer.scope.clone(),
            input.clone(),
            &codec(&consumer.schemas)?,
        )?;
        {
            let mut operation = begin(
                &remote,
                consumer.extension,
                abi::consumer::HANDLE,
                &batch.request(&local.item(0)?)?,
                None,
            )?;
            assert!(futures_util::poll!(&mut operation).is_pending());
            let error = batch.finish().unwrap_err();
            assert_eq!(
                error.downcast_ref::<Failure>().unwrap().code,
                abi::status::BUSY
            );
        }
        assert!(Batch::begin(
            remote.clone(),
            consumer.extension,
            consumer.component.codec.clone(),
            &input,
            identity.clone()
        )
        .await
        .is_err());
        consumer.stop().await?;
        consumer.start().await?;
        let mut replacement = Batch::begin(
            remote,
            consumer.extension,
            consumer.component.codec.clone(),
            &input,
            identity,
        )
        .await?;
        assert!(replacement.generation > batch.generation);
        replacement.finish()?;
        consumer.stop().await?;
        Ok(())
    }
}
