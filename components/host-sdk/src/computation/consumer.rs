// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::{
    proxy::{NativeComponentProxy, RemoteComponent},
    transaction::{RequestScope, TransactionRequests},
};
use async_trait::async_trait;
use drasi_computation_contracts::{
    delivery, lifecycle::LifecycleErrorKind, output_identity, DeliveryGuarantee,
};
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
    /// Identity codec for at-least-once delivery, which bypasses the runner.
    codec: EnvelopeCodec,
    options: DeliveryOptions,
    runner: Option<DeliveryRunner>,
    pending_cleanup: Option<Arc<dyn ComputationResourceCleanup>>,
    durability: StorageDurability,
    recovery: ComponentRecovery,
    retirement: Arc<DeliveryRetirementState>,
    running: bool,
    cleanup_required: bool,
    disposing: bool,
}
impl NativeConsumerProxy {
    /// Attach the plugin's declaration for this instance. An external
    /// consumer may declare `PermanentFailurePolicy::Skip`: it then settles
    /// each undeliverable operation itself (recording the skip with its
    /// destination progress) and reports it as
    /// `DeliveryFailureClass::Skipped`, which the `DeliveryRunner` records as
    /// completed. A transactional consumer's operation commits with the
    /// runner's own transaction, which cannot record a skip, so it supports
    /// only `stall`.
    pub(super) fn with_description(
        mut self,
        description: drasi_lib::computation::v1::ComponentDescription,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            self.mode == ConsumerMode::External
                || description.component.on_permanent_failure == PermanentFailurePolicy::Stall,
            "a transactional native consumer supports only onPermanentFailure: stall"
        );
        self.component.set_description(description);
        Ok(self)
    }

    /// Enforces the declared permanent-failure policy on a failed delivery.
    /// Under `skip`, the graph acknowledges a permanently failed input, so the
    /// failure must be the runner's report of settled skips; an operation the
    /// consumer could not settle (its destination progress refused it) would
    /// leave unfinished progress behind, so it fails the node instead. Under
    /// `stall`, a consumer must never settle a skip.
    fn enforce_policy(&self, error: anyhow::Error) -> anyhow::Error {
        let policy = self.component.description().component.on_permanent_failure;
        let class = drasi_computation_contracts::delivery_failure_class(&error);
        let found = |skipped: bool| {
            error.chain().any(|cause| {
                cause.downcast_ref::<DeliveryError>().is_some_and(|error| {
                    if skipped {
                        matches!(error, DeliveryError::Skipped { .. })
                    } else {
                        matches!(error, DeliveryError::Handling { .. })
                    }
                })
            })
        };
        let (reported, handling) = (found(true), found(false));
        match (policy, class) {
            (PermanentFailurePolicy::Stall, _)
                if reported || class == Some(DeliveryFailureClass::Skipped) =>
            {
                LifecycleError::terminal(error.context(
                    "native consumer settled a skip although it declares onPermanentFailure: stall",
                ))
                .into()
            }
            (PermanentFailurePolicy::Skip, Some(class))
                if class.is_permanent() && handling && !reported =>
            {
                LifecycleError::terminal(error.context(
                    "native consumer could not settle this undeliverable operation as skipped; \
                     its delivery progress must be repaired before more input",
                ))
                .into()
            }
            _ => error,
        }
    }
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
        let volatile_codec = codec(&schemas)?;
        let codec = codec(&schemas)?;
        let indexes = provider
            .create_indexes(&scope.graph_id, component.descriptor().id())
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
            codec: volatile_codec,
            options,
            runner: Some(runner),
            pending_cleanup: None,
            durability,
            recovery,
            retirement,
            running: false,
            cleanup_required: false,
            disposing: false,
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
            .create_indexes(&self.graph, self.component.descriptor().id())
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
    async fn close_delivery(&mut self) -> anyhow::Result<()> {
        if let Some(runner) = &mut self.runner {
            self.retirement.close(runner).await?;
        }
        self.runner = None;
        if let Some(cleanup) = &self.pending_cleanup {
            cleanup.cancel();
            cleanup.shutdown().await?;
        }
        self.pending_cleanup = None;
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
    fn description(&self) -> drasi_lib::computation::v1::ComponentDescription {
        self.component.description()
    }
    fn descriptor(&self) -> &ComponentDescriptor {
        self.component.descriptor()
    }
    fn configuration(&self) -> anyhow::Result<serde_json::Value> {
        self.component.configuration()
    }
    fn recovery_contract(&self) -> ComponentRecovery {
        self.recovery.clone()
    }
    fn binds_inputs(&self) -> bool {
        self.component.binds_inputs()
    }
    async fn bind_inputs(&mut self, bindings: &[InputBinding]) -> anyhow::Result<()> {
        self.component.bind_inputs(bindings).await
    }
    fn bind_control(&mut self, control: ComponentControl) {
        self.component.bind_control(control);
    }
    fn control_handler(&self) -> Option<Arc<dyn ControlHandler>> {
        self.component.control_handler()
    }
    fn worker_failures(&self) -> Option<Arc<dyn drasi_lib::computation::v1::WorkerFailures>> {
        self.component.worker_failures()
    }
    fn requires_readiness_confirmation(&self) -> bool {
        self.component.requires_readiness_confirmation()
    }
    async fn initialize(&mut self, context: &InitializeContext) -> anyhow::Result<()> {
        self.component.initialize(context).await
    }
    async fn pause(&mut self, context: &PauseContext) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.running && !self.cleanup_required,
            "native consumer cannot pause with incomplete work"
        );
        self.component.pause(context).await
    }
    async fn resume(&mut self, context: &ResumeContext) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.running && !self.cleanup_required,
            "native consumer cannot resume with incomplete work"
        );
        self.component.resume(context).await
    }
    fn management(&self) -> Option<&dyn ComponentManagement> {
        self.component
            .management()
            .is_some()
            .then_some(self as &dyn ComponentManagement)
    }
    fn management_mut(&mut self) -> Option<&mut dyn ComponentManagement> {
        if self.component.management().is_some() {
            Some(self)
        } else {
            None
        }
    }
    async fn start(&mut self, context: &StartContext) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.running && !self.cleanup_required && !self.disposing,
            "native consumer requires completed cleanup before starting"
        );
        self.cleanup_required = true;
        self.reopen().await?;
        self.runner.as_mut().expect("opened").progress().await?;
        self.validate_mode()?;
        self.component.start(context).await?;
        self.validate_mode()?;
        self.running = true;
        self.cleanup_required = false;
        Ok(())
    }
    async fn stop(&mut self, context: &StopContext) -> anyhow::Result<()> {
        self.running = false;
        self.cleanup_required = true;
        self.component.stop(context).await?;
        self.close_delivery().await?;
        self.cleanup_required = false;
        Ok(())
    }
    async fn dispose(&mut self, context: &DisposeContext) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.running && (!self.cleanup_required || self.disposing),
            "native consumer disposal requires completed cleanup"
        );
        self.disposing = true;
        self.cleanup_required = true;
        self.close_delivery().await?;
        self.component.dispose(context).await?;
        self.cleanup_required = false;
        Ok(())
    }
}
#[async_trait]
impl ComponentManagement for NativeConsumerProxy {
    fn assess(
        &self,
        current: &ComponentSpecification,
        proposed: &ComponentSpecification,
    ) -> anyhow::Result<Option<Assessment>> {
        match self.component.management() {
            Some(management) => management.assess(current, proposed),
            None => Ok(None),
        }
    }
    async fn apply(
        &mut self,
        context: &ApplyContext,
        update: ComponentUpdateContext,
    ) -> std::result::Result<(), ApplyError> {
        if self.cleanup_required || self.disposing {
            return Err(ApplyError::Failed(anyhow::anyhow!(
                "native consumer update requires completed delivery and cleanup"
            )));
        }
        self.component
            .management_mut()
            .ok_or_else(|| {
                ApplyError::Failed(anyhow::anyhow!(
                    "native component does not export management"
                ))
            })?
            .apply(context, update)
            .await?;
        self.validate_mode().map_err(ApplyError::Damaged)
    }
}
#[async_trait]
impl EnvelopeSink for NativeConsumerProxy {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        if !self.running || self.cleanup_required {
            // An interrupted or uncertain delivery left work that only a
            // restart can clean up; retrying the input cannot help.
            return Err(LifecycleError::terminal(anyhow::anyhow!(
                "native consumer is not ready: incomplete delivery requires a restart"
            ))
            .into());
        }
        wire::check_port(
            self.component.descriptor(),
            &input.port,
            &input.envelope,
            PortDirection::Input,
        )?;
        // The declared input guarantee alone selects the delivery mode.
        let guarantee = self
            .component
            .description()
            .input(&input.port)
            .delivery
            .guarantee()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "native consumer input '{}' declares no keyed delivery",
                    input.port
                )
            })?;
        // An uncertain progress commit fenced the runner's storage owner (a
        // storage failure during live processing). Reopening it from storage
        // reloads the committed ledger, resolving that uncertainty, so the
        // graph's retry of the same input resumes at the completed prefix. If
        // storage still fails, this attempt fails transiently and the graph
        // retries it later.
        if guarantee == DeliveryGuarantee::ExactlyOnce
            && self
                .runner
                .as_ref()
                .is_none_or(DeliveryRunner::recovery_required)
        {
            self.close_delivery().await?;
            self.reopen().await?;
        }
        // Held while a batch may be open on the plugin: a cancelled delivery
        // (this future dropped mid-await) leaves it set, so cleanup is
        // required before more input.
        self.cleanup_required = true;
        let delivered = match guarantee {
            DeliveryGuarantee::ExactlyOnce => self.deliver_exactly_once(&input).await,
            DeliveryGuarantee::AtLeastOnce => self.deliver_at_least_once(&input).await,
        }
        .map_err(|error| self.enforce_policy(error));
        match &delivered {
            Ok(()) => self.cleanup_required = false,
            // A failure that settled before or during handling keeps only the
            // completed operation prefix (runner ledger or destination
            // cursor) and never skips the failed operation, so the graph may
            // deliver the same input again without Stop/Start. A
            // transactional failure is settled only after a confirmed
            // rollback, which the runner reports as `DeliveryError::Handling`;
            // anything else may leave commit uncertainty that fences the owner.
            Err(error) => {
                let closed = matches!(
                    drasi_computation_contracts::lifecycle::lifecycle_error_kind(error),
                    Some(LifecycleErrorKind::Terminal)
                );
                // An uncertain commit closed the batch but fenced the
                // runner; the next attempt reopens it from storage first.
                let fenced = self
                    .runner
                    .as_ref()
                    .is_some_and(DeliveryRunner::recovery_required);
                if !closed
                    && (self.mode == ConsumerMode::External
                        || fenced
                        || error
                            .downcast_ref::<DeliveryError>()
                            .is_some_and(|error| matches!(error, DeliveryError::Handling { .. })))
                {
                    self.cleanup_required = false;
                }
            }
        }
        delivered
    }
}

/// An envelope that does not carry the producer identity its input's declared
/// guarantee requires. The graph checked the producer's declaration when it
/// bound the edge, so this input can never be delivered.
fn missing_identity(guarantee: DeliveryGuarantee, error: anyhow::Error) -> anyhow::Error {
    if output_identity::is_missing_progress(&error) {
        DeliveryFailure::permanent(error.context(format!(
            "the envelope lacks the producer identity that {guarantee:?} delivery requires"
        )))
        .into()
    } else {
        error
    }
}

impl NativeConsumerProxy {
    /// Exactly-once delivery through the host `DeliveryRunner` ledger, keyed
    /// on the producer's durable identity.
    async fn deliver_exactly_once(&mut self, input: &InputEnvelope) -> anyhow::Result<()> {
        let runner = self.runner.as_mut().expect("running consumer");
        let identity = runner
            .batch_identity(input)
            .map_err(|error| missing_identity(DeliveryGuarantee::ExactlyOnce, error))?;
        let mut batch = match Batch::begin(
            self.component.inner.clone(),
            self.extension,
            self.component.codec.clone(),
            input,
            identity,
        )
        .await
        {
            Ok(batch) => batch,
            Err(error) => {
                // No batch was opened, so nothing remains to clean up.
                self.cleanup_required = false;
                return Err(error);
            }
        };
        let delivered = match self.mode {
            ConsumerMode::External => runner.deliver(input, &mut batch).await,
            ConsumerMode::Transactional => runner.deliver_transactional(input, &batch).await,
        };
        // The batch is closed whatever the outcome; a failed close keeps the
        // cleanup requirement.
        batch.finish().map_err(LifecycleError::terminal)?;
        delivered
    }

    /// At-least-once delivery. It bypasses the `DeliveryRunner`: the
    /// destination cursor, keyed by the batch's producer identity (and a
    /// volatile producer's epoch), deduplicates redelivery. Operations are
    /// handled in order and stop at the first failure.
    async fn deliver_at_least_once(&mut self, input: &InputEnvelope) -> anyhow::Result<()> {
        let candidate = match output_identity::Candidate::new(&input.envelope, &self.codec) {
            Err(error) if output_identity::is_missing_progress(&error) => {
                output_identity::Candidate::new_volatile(&input.envelope, &self.codec)
            }
            candidate => candidate,
        }
        .map_err(|error| missing_identity(DeliveryGuarantee::AtLeastOnce, error))?;
        let identity = delivery::batch_identity(&self.scope, input, &candidate)?;
        let mut batch = match Batch::begin(
            self.component.inner.clone(),
            self.extension,
            self.component.codec.clone(),
            input,
            identity.clone(),
        )
        .await
        {
            Ok(batch) => batch,
            Err(error) => {
                // No batch was opened, so nothing remains to clean up.
                self.cleanup_required = false;
                return Err(error);
            }
        };
        let delivered = async {
            // As the runner does: an operation the consumer settled as
            // skipped is passed, and the input reports the skips at the end.
            let mut skipped: Option<(usize, anyhow::Error)> = None;
            for (index, operation) in input.envelope.changes().operations().iter().enumerate() {
                let id =
                    delivery::operation_id(&self.scope, input, &candidate, operation.ordinal())?;
                let item =
                    delivery::delivery_item(input, &candidate, &identity, index, operation, id);
                if let Err(error) = DeliveryHandler::handle(&mut batch, item).await {
                    if drasi_computation_contracts::delivery_failure_class(&error)
                        != Some(DeliveryFailureClass::Skipped)
                    {
                        return Err(DeliveryError::Handling {
                            id: delivery::operation_id(
                                &self.scope,
                                input,
                                &candidate,
                                operation.ordinal(),
                            )?,
                            source: error,
                        }
                        .into());
                    }
                    skipped = Some(match skipped {
                        Some((count, first)) => (count + 1, first),
                        None => (1, error),
                    });
                }
            }
            match skipped {
                None => anyhow::Ok(()),
                Some((skipped, first)) => {
                    Err(
                        DeliveryFailure::permanent(DeliveryError::Skipped { skipped, first })
                            .into(),
                    )
                }
            }
        }
        .await;
        batch.finish().map_err(LifecycleError::terminal)?;
        delivered
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
        let generation: u64 = wire::decode(
            &operation
                .await
                .map_err(super::proxy::native_operation_cause)?,
        )?;
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
        ))
        .map_err(super::proxy::native_operation_cause)?;
        OperationFuture::new(operation).map_err(super::proxy::native_operation_cause)
    }
}
#[async_trait]
impl DeliveryHandler for Batch {
    async fn handle(&mut self, item: DeliveryItem<'_>) -> anyhow::Result<()> {
        let operation = begin(
            &self.component,
            self.extension,
            abi::consumer::HANDLE,
            &self.request(&item)?,
            None,
        )?;
        anyhow::ensure!(
            operation
                .await
                .map_err(super::proxy::native_operation_cause)?
                .is_empty(),
            "native consumer handling returned unexpected data"
        );
        Ok(())
    }
}
#[async_trait]
impl TransactionalDeliveryHandler for Batch {
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
            result = &mut operation => result.map_err(super::proxy::native_operation_cause),
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
        batch, factory, id, initialized_consumer, options, plugin, provider, scope,
    };
    use serde_json::json;

    #[tokio::test(flavor = "current_thread")]
    async fn native_consumer_rejects_forged_content_stale_batches_and_oversized_controls(
    ) -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let plugin = plugin()?;
        let factory = factory(plugin.as_ref(), ConsumerMode::External);
        let mut consumer = initialized_consumer(
            factory.as_ref(),
            id(423),
            json!({"path":directory.path().join("effects.db")}),
            scope(),
            provider(&directory.path().join("progress"))?,
            options(),
        )
        .await?;
        consumer
            .start(&crate::computation::test_hooks::start_ctx())
            .await?;
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
        consumer
            .stop(&crate::computation::test_hooks::stop_ctx())
            .await?;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn native_consumer_active_call_blocks_batch_retirement_and_cancel_fences_reuse(
    ) -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let plugin = plugin()?;
        let factory = factory(plugin.as_ref(), ConsumerMode::External);
        let mut consumer = initialized_consumer(
            factory.as_ref(),
            id(423),
            json!({"mode":"pending"}),
            scope(),
            provider(directory.path())?,
            options(),
        )
        .await?;
        consumer
            .start(&crate::computation::test_hooks::start_ctx())
            .await?;
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
        consumer
            .stop(&crate::computation::test_hooks::stop_ctx())
            .await?;
        consumer
            .start(&crate::computation::test_hooks::start_ctx())
            .await?;
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
        consumer
            .stop(&crate::computation::test_hooks::stop_ctx())
            .await?;
        Ok(())
    }
}
