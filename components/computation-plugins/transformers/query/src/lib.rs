// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

mod durable;
mod indexed;
mod observations;
mod output;
mod update;

use std::{num::NonZeroUsize, sync::Arc};

use anyhow::Context;
use async_trait::async_trait;
use chrono::Utc;
use drasi_computation_contracts::profiling::timestamp_ns;
use drasi_computation_contracts::telemetry::{
    ExecutionContext, QueryObservationKind, QueryObservationOutcome, QueryObservationPhase,
};
use drasi_computation_contracts::ComputationQueryLanguage as QueryLanguage;
use drasi_computation_contracts::{
    BootstrapCompletion, BootstrapStatus, ChangeEnvelope, ComponentDescriptor, ComponentRole,
    ComponentSemanticKind, ComponentUpdateData, ComponentUpdateMode, ComponentUpdatePlan,
    GraphChangeCodec, GraphProducerProgress, ImplementationIdentity, InputBinding, InputEnvelope,
    OutputCursor, OutputEnvelope, OutputIdentity, OutputProgress, OutputReaders, PipeRequirements,
    PortDescriptor, PortDirection, PortId, QueryChangeCodec, QueryDescription,
    QueryExecutionSettings, ResultNodeProjector, StreamId, WakeupSource,
};
use drasi_computation_plugin_sdk::{
    lifecycle::{
        nothing_to_do, ApplyContext, ApplyError, Assessment, DisposeContext, InitializeContext,
        LifecycleResult, PauseContext, ResumeContext, StartContext, StopContext,
    },
    ApplyUpdateRequest, BootstrapInput, Capabilities, Component, ConfigDetails, ConfigSchema,
    ConfigType, ControlSender, CreateRequest, CreatedComponent, Factory, FactoryMetadata,
    NativeBootstrapInput, NativeComponent, NativeDurableBootstrapInput, NativeDurableOutput,
    NativeInputBindings, NativeManagement, NativeOutputSnapshots, NativeProcessing,
    NativeTransformer, PluginDefinition, ServiceBindings, UpdateRequest,
};
use drasi_core::{
    computation::QueryInputBatch, middleware::MiddlewareTypeRegistry, query::QueryEvaluator,
};
use drasi_query_common::{
    query_builder, ContinuousQueryDefinition, QueryExecutionSettingsExt, QueryInputScopes,
    QueryOperation, QuerySchedule,
};

const INDEXED_IMPLEMENTATION: &str = "drasi.query/indexed";

pub fn plugin() -> anyhow::Result<PluginDefinition> {
    plugin_with_middleware_registry(default_middleware_registry())
}

pub fn default_middleware_registry() -> Arc<MiddlewareTypeRegistry> {
    let mut registry = MiddlewareTypeRegistry::new();
    for factory in drasi_middleware::source_middleware_factories() {
        registry.register(factory);
    }
    Arc::new(registry)
}

pub fn plugin_with_middleware_registry(
    middleware: Arc<MiddlewareTypeRegistry>,
) -> anyhow::Result<PluginDefinition> {
    PluginDefinition::new(
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION"),
        vec![Arc::new(IndexedQueryFactory::new(middleware))],
        vec![GraphChangeCodec::schema()],
    )
}

#[cfg(feature = "dynamic-plugin")]
drasi_computation_plugin_sdk::export_computation_plugin!(plugin());

/// The `middleware` field declares the registry's kinds, so the graph rejects a
/// definition naming an unregistered kind when it binds the query.
fn query_configuration_schema(
    fields: impl IntoIterator<Item = (&'static str, ConfigType, bool)>,
    middleware: &MiddlewareTypeRegistry,
) -> ConfigSchema {
    let mut schema = ConfigSchema::from_fields(fields);
    schema
        .constrain(
            "inputs",
            drasi_computation_contracts::ConfigurationConstraint::query_inputs(),
        )
        .expect("query schema declares inputs");
    schema
        .constrain(
            "middleware",
            drasi_computation_contracts::ConfigurationConstraint::middleware_declarations(
                &middleware.kinds(),
            )
            .expect("middleware kinds are valid schema values"),
        )
        .expect("query schema declares middleware");
    schema
}

/// `sourceTransactions`: the camelCase form of the shared
/// [`drasi_computation_contracts::SourceTransactionLimits`].
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SourceTransactionSettings {
    max_changes: NonZeroUsize,
    max_bytes: NonZeroUsize,
    max_duration_ms: std::num::NonZeroU64,
}

impl From<SourceTransactionSettings> for drasi_computation_contracts::SourceTransactionLimits {
    fn from(settings: SourceTransactionSettings) -> Self {
        Self {
            max_changes: settings.max_changes,
            max_bytes: settings.max_bytes,
            max_duration_ms: settings.max_duration_ms,
        }
    }
}

pub struct IndexedQueryFactory {
    middleware: Arc<MiddlewareTypeRegistry>,
}

impl IndexedQueryFactory {
    pub fn new(middleware: Arc<MiddlewareTypeRegistry>) -> Self {
        Self { middleware }
    }
}

impl Default for IndexedQueryFactory {
    fn default() -> Self {
        Self::new(default_middleware_registry())
    }
}

/// Descriptions of every indexed-query configuration field.
fn query_configuration_details() -> ConfigDetails {
    let describe =
        |description: &str| drasi_computation_plugin_sdk::configuration::ConfigFieldDetails {
            description: description.into(),
            ..Default::default()
        };
    ConfigDetails {
        fields: [
            ("query", describe("The continuous query text.")),
            (
                "volatile",
                describe("Use the explicitly volatile processing profile (in-memory providers)."),
            ),
            (
                "publication",
                describe("Output publication: atomic (default) or explicit non-atomic."),
            ),
            ("language", describe("Query language: cypher (default) or gql.")),
            ("joins", describe("Synthetic joins between input labels.")),
            ("inputs", describe("The query's declared inputs and their labels.")),
            ("middleware", describe("Input-local middleware applied before evaluation.")),
            ("result", describe("Result dataset settings, such as its label.")),
            ("stream", describe("Output stream; defaults to <component>/out.")),
            (
                "outboxCapacity",
                describe("Retained output capacity; a positive count, one by default."),
            ),
            (
                "bootstrap",
                describe("Initialize from the provider bound on the bootstrap input."),
            ),
            (
                "sourceTransactions",
                describe("Limits for complete source transactions (maxChanges, maxBytes, maxDurationMs)."),
            ),
            ("recovery", describe("Recovery policy: strict (default) or auto_reset.")),
            (
                "resetConfiguration",
                describe("Request a reset of retained state for a changed configuration."),
            ),
        ]
        .into_iter()
        .map(|(name, details)| (name.to_string(), details))
        .collect(),
    }
}

impl Factory for IndexedQueryFactory {
    fn supports_query_instrumentation(&self) -> bool {
        true
    }

    fn configuration_details(&self) -> ConfigDetails {
        query_configuration_details()
    }
    /// Outputs follow the durability the Host's bound processing provider declares.
    fn describe(
        &self,
        request: &drasi_computation_plugin_sdk::wire::DescribeRequest,
    ) -> anyhow::Result<drasi_computation_plugin_sdk::ComponentDescription> {
        use drasi_computation_plugin_sdk::{
            ComponentDescription, OutputDescription, ProgressPersistence, ResourceDurability,
        };
        let durability = request
            .bound_durability("processing")
            .unwrap_or(ResourceDurability::Undeclared);
        let progress = match durability {
            ResourceDurability::Durable => ProgressPersistence::Durable,
            ResourceDurability::Volatile => ProgressPersistence::Volatile,
            ResourceDurability::Undeclared => ProgressPersistence::Undeclared,
        };
        Ok(ComponentDescription::new(progress).with_outputs(
            &request.descriptor,
            OutputDescription::following(durability),
        ))
    }
    fn metadata(&self) -> FactoryMetadata {
        let input = || PortId::try_new("in").expect("constant port");
        let output = || PortId::try_new("out").expect("constant port");
        FactoryMetadata {
            implementation: ImplementationIdentity::try_new(INDEXED_IMPLEMENTATION, "1")
                .expect("constant implementation"),
            role: ComponentRole::Transformer,
            configuration_version: 1,
            configuration: query_configuration_schema(
                [
                    ("query", ConfigType::String, true),
                    ("volatile", ConfigType::Boolean, false),
                    ("publication", ConfigType::String, false),
                    ("language", ConfigType::String, false),
                    ("joins", ConfigType::Array, false),
                    ("inputs", ConfigType::Array, false),
                    ("middleware", ConfigType::Array, false),
                    ("result", ConfigType::Object, false),
                    ("stream", ConfigType::String, false),
                    ("outboxCapacity", ConfigType::Integer, false),
                    ("bootstrap", ConfigType::Boolean, false),
                    ("sourceTransactions", ConfigType::Object, false),
                    ("recovery", ConfigType::String, false),
                    ("resetConfiguration", ConfigType::Boolean, false),
                ],
                &self.middleware,
            ),
            ports: [("in", PortDirection::Input), ("out", PortDirection::Output)]
                .into_iter()
                .map(|(name, direction)| {
                    PortDescriptor::new(
                        PortId::try_new(name).expect("constant port"),
                        direction,
                        GraphChangeCodec::schema().descriptor().clone(),
                        PipeRequirements::default(),
                    )
                })
                .collect(),
            completion: None,
            capabilities: Capabilities {
                semantic_kind: Some(ComponentSemanticKind::Query),
                output_progress: Some(OutputProgress::Origin),
                output_snapshots: vec![output()],
                output_follows: vec![output()],
                output_queries: vec![output()],
                output_replays: vec![output()],
                query_observations: true,
                query_recovery_views: true,
                continuations: true,
                startup_continuation: true,
                wakeups: true,
                input_binding_updates: true,
                management: true,
                update_modes: vec![ComponentUpdateMode::InPlace, ComponentUpdateMode::Recreate],
                indexed_processing: true,
                volatile_processing: true,
                non_atomic_publication: true,
                committed_input_progress: Some(input()),
                durable_bootstrap: Some(input()),
                durable_output: true,
                erases_owned_state: true,
                ..Capabilities::default()
            },
        }
    }

    fn create(
        &self,
        request: &CreateRequest,
        _: ControlSender,
        services: ServiceBindings,
    ) -> anyhow::Result<CreatedComponent> {
        create_query(
            request,
            self.metadata(),
            services.processing.ok_or_else(|| {
                anyhow::anyhow!("indexed query requires host-owned indexed processing")
            })?,
            self.middleware.clone(),
        )
    }
    fn update_planner(&self) -> Option<&dyn drasi_computation_plugin_sdk::UpdatePlanner> {
        Some(self)
    }
}

impl drasi_computation_plugin_sdk::UpdatePlanner for IndexedQueryFactory {
    fn assess(&self, request: &UpdateRequest) -> anyhow::Result<Assessment> {
        update::plan(request, &self.middleware, &self.metadata()).map(Assessment::from)
    }
}

fn query_configuration(
    request: &CreateRequest,
    middleware: &MiddlewareTypeRegistry,
) -> anyhow::Result<(ContinuousQueryDefinition, QueryExecutionSettings)> {
    let query = request.configuration["query"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("query must be a string"))?;
    let language = match request.configuration.get("language") {
        None => QueryLanguage::Cypher,
        Some(serde_json::Value::String(language)) if language == "cypher" => QueryLanguage::Cypher,
        Some(serde_json::Value::String(language)) if language == "gql" => QueryLanguage::Gql,
        Some(_) => anyhow::bail!("query language must be 'cypher' or 'gql'"),
    };
    let mut execution: QueryExecutionSettings = serde_json::from_value(serde_json::Value::Object(
        ["joins", "result", "inputs", "middleware"]
            .into_iter()
            .filter_map(|name| {
                request
                    .configuration
                    .get(name)
                    .cloned()
                    .map(|value| (name.into(), value))
            })
            .collect(),
    ))
    .context("invalid query execution settings")?;
    execution.source_transactions = request
        .configuration
        .get("sourceTransactions")
        .map(|value| serde_json::from_value::<SourceTransactionSettings>(value.clone()))
        .transpose()
        .context("invalid sourceTransactions")?
        .map(Into::into);
    execution.validate(Some(middleware))?;
    let id = request.id.clone();
    let stream = match request.configuration.get("stream") {
        Some(value) => StreamId::try_new(value.as_str().context("query stream must be a string")?)?,
        None => StreamId::try_new(format!("{id}/out"))?,
    };
    let capacity = request
        .configuration
        .get("outboxCapacity")
        .map(|value| serde_json::from_value::<NonZeroUsize>(value.clone()))
        .transpose()
        .context("invalid query output retention capacity")?
        .unwrap_or_else(|| NonZeroUsize::new(1).expect("positive default retention"));
    let definition = ContinuousQueryDefinition {
        graph_id: request
            .scope
            .as_ref()
            .map_or("native-standalone", |scope| scope.graph_id.as_str())
            .into(),
        id: id.clone(),
        query: query.to_owned(),
        language,
        output_stream: stream.clone(),
        outbox_capacity: capacity,
    };
    Ok((definition, execution))
}

fn create_query(
    request: &CreateRequest,
    metadata: FactoryMetadata,
    processing: NativeProcessing,
    middleware: Arc<MiddlewareTypeRegistry>,
) -> anyhow::Result<CreatedComponent> {
    update::validate_creation(request)?;
    let (definition, execution) = query_configuration(request, &middleware)?;
    let definition = Arc::new(definition);
    let telemetry = observations::QueryObservations::capture()?;
    let id = definition.id.clone();
    let stream = definition.output_stream.clone();
    let capacity = definition.outbox_capacity;
    let language = definition.language;
    let input_scopes = if !request.inputs.is_empty()
        && execution
            .inputs
            .iter()
            .any(|input| !input.pipeline.is_empty())
    {
        Some(drasi_query_common::query_input_scopes(
            &request.id,
            &execution.inputs,
            &request.inputs,
        )?)
    } else {
        None
    };
    let instance = request
        .scope
        .as_ref()
        .map_or("native-standalone", |scope| scope.instance_id.as_str());
    let projector = definition.projector(instance, &execution)?;
    let output = Arc::new(output::QueryOutput::new(
        request.scope.as_ref(),
        id.clone(),
        stream.clone(),
        projector.clone(),
        capacity,
        telemetry.clone(),
        QueryDescription {
            graph_id: definition.graph_id.clone(),
            query: definition.query.clone(),
            language,
            execution: execution.clone(),
            output_stream: stream.clone(),
            outbox_capacity: capacity,
            bootstrap: request
                .configuration
                .get("bootstrap")
                .is_some_and(|value| value == true),
            auto_reset: request
                .configuration
                .get("recovery")
                .and_then(serde_json::Value::as_str)
                == Some("auto_reset"),
            volatile: request
                .configuration
                .get("volatile")
                .is_some_and(|value| value == true),
            configuration_hash: definition.configuration_hash(&execution)?,
        },
    ));
    let schedule = QuerySchedule::default();
    let durable = durable::DurableQuery::new(
        request,
        &definition,
        output.clone(),
        &execution,
        schedule.clone(),
        telemetry.clone(),
    )?;
    Ok(Component::Transformer(Box::new(QueryTransformer {
        descriptor: metadata.descriptor(id.clone())?,
        metadata,
        configuration: request.configuration.clone(),
        definition,
        execution: Arc::new(execution),
        middleware,
        input_scopes,
        projector,
        output,
        processing,
        durable,
        started: false,
        failed: false,
        schedule,
        draining_futures: false,
        telemetry,
        bootstrap_run: None,
        processing_context: ExecutionContext::default(),
    }))
    .into())
}

async fn evaluator(
    query: &str,
    language: QueryLanguage,
    execution: &QueryExecutionSettings,
    indexes: Option<Arc<indexed::Indexes>>,
    middleware: Arc<MiddlewareTypeRegistry>,
) -> anyhow::Result<QueryEvaluator> {
    let mut builder = query_builder(query, language, execution, Some(middleware))?;
    if let Some(indexes) = indexes {
        builder = builder
            .with_element_index(indexes.clone())
            .with_archive_index(indexes.clone())
            .with_result_index(indexes.clone())
            .with_future_queue(indexes);
    }
    Ok(builder.try_build_evaluator().await?)
}

struct QueryTransformer {
    descriptor: ComponentDescriptor,
    metadata: FactoryMetadata,
    configuration: serde_json::Value,
    definition: Arc<ContinuousQueryDefinition>,
    execution: Arc<QueryExecutionSettings>,
    middleware: Arc<MiddlewareTypeRegistry>,
    input_scopes: Option<QueryInputScopes>,
    projector: ResultNodeProjector,
    output: Arc<output::QueryOutput>,
    processing: NativeProcessing,
    durable: durable::DurableQuery,
    started: bool,
    failed: bool,
    schedule: QuerySchedule,
    draining_futures: bool,
    telemetry: observations::QueryObservations,
    bootstrap_run: Option<observations::QueryRun>,
    processing_context: ExecutionContext,
}

#[async_trait]
impl NativeComponent for QueryTransformer {
    fn management(&mut self) -> Option<&mut dyn NativeManagement> {
        Some(self)
    }

    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }

    fn configuration(&self) -> anyhow::Result<serde_json::Value> {
        Ok(self.configuration.clone())
    }

    fn output_readers(&self) -> OutputReaders {
        OutputReaders::new()
            .with_snapshot(
                PortId::try_new("out").expect("constant port"),
                self.output.clone(),
            )
            .with_snapshot_follow(
                PortId::try_new("out").expect("constant port"),
                self.output.clone(),
            )
            .with_query(
                PortId::try_new("out").expect("constant port"),
                self.output.clone(),
            )
            .with_replay(
                PortId::try_new("out").expect("constant port"),
                self.output.clone(),
            )
    }

    async fn initialize(&mut self, _ctx: &InitializeContext) -> LifecycleResult {
        nothing_to_do()
    }

    async fn start(&mut self, _ctx: &mut StartContext) -> LifecycleResult {
        Ok(self.start_activation().await?)
    }

    // Transformation, continuations and wakeups are driven by the host; the
    // query owns no independent workers.
    async fn pause(&mut self, _ctx: &PauseContext) -> LifecycleResult {
        nothing_to_do()
    }

    async fn resume(&mut self, _ctx: &ResumeContext) -> LifecycleResult {
        nothing_to_do()
    }

    async fn stop(&mut self, _ctx: &StopContext) -> LifecycleResult {
        Ok(self.stop_activation()?)
    }

    async fn dispose(&mut self, ctx: &DisposeContext) -> LifecycleResult {
        if ctx.erases_owned_state() {
            self.erase_owned_state().await?;
        }
        Ok(())
    }
    fn input_bindings(&mut self) -> Option<&mut dyn NativeInputBindings> {
        Some(self)
    }
}

#[async_trait]
impl NativeManagement for QueryTransformer {
    fn assess(&self, request: &UpdateRequest) -> anyhow::Result<Assessment> {
        update::plan(request, &self.middleware, &self.metadata).map(Assessment::from)
    }

    async fn apply(
        &mut self,
        _ctx: &ApplyContext,
        request: &ApplyUpdateRequest,
    ) -> Result<(), ApplyError> {
        Ok(self.apply_update(request).await?)
    }
}

impl QueryTransformer {
    async fn apply_update(&mut self, request: &ApplyUpdateRequest) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.started && !self.failed,
            "query update requires completed activation and a paused processing boundary"
        );
        anyhow::ensure!(
            request.request.current.id == *self.descriptor.id()
                && request.request.current.configuration == self.configuration
                && request.plan
                    == update::plan(&request.request, &self.middleware, &self.metadata)?
                && request.plan.mode == ComponentUpdateMode::InPlace
                && request.plan.data == ComponentUpdateData::Preserve,
            "retained query update does not match the approved configuration and plan"
        );
        let (definition, _) = query_configuration(&request.request.proposed, &self.middleware)?;
        self.output.increase_retention(definition.outbox_capacity)?;
        self.durable.set_retention(definition.outbox_capacity);
        Arc::make_mut(&mut self.definition).outbox_capacity = definition.outbox_capacity;
        self.configuration = request.request.proposed.configuration.clone();
        Ok(())
    }

    async fn erase_owned_state(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.started,
            "query resource deletion requires completed stop"
        );
        let processing = &self.processing;
        let volatile = self.durable.volatile;
        let non_atomic = self.durable.non_atomic;
        if non_atomic {
            processing
                .run(|context| async move {
                    context.begin_non_atomic_reset(false).await?;
                    Ok(())
                })
                .await?;
        }
        processing
            .run(move |context| async move {
                if volatile {
                    context.reset_volatile_state(false).await?;
                } else if non_atomic {
                    context.reset_non_atomic_state(false).await?;
                } else {
                    context.reset_state(false).await?;
                }
                Ok(())
            })
            .await?;
        if non_atomic {
            processing
                .run(|context| async move {
                    context.finish_non_atomic_reset().await?;
                    Ok(())
                })
                .await?;
        }
        self.output.clear()?;
        Ok(())
    }

    async fn start_activation(&mut self) -> anyhow::Result<()> {
        let telemetry = self.telemetry.clone();
        let result = telemetry
            .transaction(QueryObservationPhase::Bootstrap, async {
                anyhow::ensure!(!self.started, "query is already started or initializing");
                let was_failed = self.failed;
                self.output.begin_activation()?;
                self.schedule.publish(None)?;
                self.draining_futures = false;
                self.failed = true;
                self.bootstrap_run = Some(telemetry.run(QueryObservationPhase::Bootstrap));
                let reset = self.durable.reset_failed_volatile(was_failed)?;
                self.durable
                    .start(
                        &self.processing,
                        self.definition.clone(),
                        self.execution.clone(),
                        self.middleware.clone(),
                        reset,
                    )
                    .await?;
                self.started = true;
                self.failed = false;
                Ok(())
            })
            .await;
        if let Some(run) = self.bootstrap_run.take() {
            run.finish(
                if result.is_ok() {
                    QueryObservationOutcome::Success
                } else {
                    QueryObservationOutcome::Error
                },
                None,
            )?;
        }
        result
    }

    fn stop_activation(&mut self) -> anyhow::Result<()> {
        self.bootstrap_run.take();
        self.processing_context = ExecutionContext::default();
        self.started = false;
        self.draining_futures = false;
        self.schedule.publish(None)?;
        self.output.suspend()?;
        self.durable.stop(self.failed)?;
        Ok(())
    }
}

#[async_trait]
impl NativeTransformer for QueryTransformer {
    fn has_pending_emissions(&self) -> bool {
        self.draining_futures || self.durable.has_pending_emissions()
    }

    async fn continue_transform(&mut self) -> anyhow::Result<Vec<OutputEnvelope>> {
        anyhow::ensure!(
            self.started && !self.failed,
            "query is not ready for continuation"
        );
        if !self.durable.has_pending_emissions() {
            return if self.draining_futures {
                self.on_wakeup().await
            } else {
                Ok(Vec::new())
            };
        }
        let run = self.telemetry.run(QueryObservationPhase::Steady);
        run.start(QueryObservationKind::OutputReplay)?;
        self.failed = true;
        let output = run
            .scope(self.durable.continue_output(&self.processing))
            .await;
        self.processing_context = run.context()?.unwrap_or_default();
        run.finish(
            if output.is_ok() {
                QueryObservationOutcome::Success
            } else {
                QueryObservationOutcome::Error
            },
            Some(u64::from(output.as_ref().is_ok_and(Option::is_some))),
        )?;
        let output = output?;
        self.failed = false;
        Ok(output
            .into_iter()
            .map(|envelope| {
                OutputEnvelope::new(PortId::try_new("out").expect("constant port"), envelope)
                    .with_execution_context(self.processing_context.clone())
            })
            .collect())
    }

    async fn transform(&mut self, input: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>> {
        let telemetry = self.telemetry.clone();
        telemetry
            .transaction(QueryObservationPhase::Steady, async {
                anyhow::ensure!(!self.failed, "query is fenced after a processing failure");
                anyhow::ensure!(self.started, "query is not started");
                anyhow::ensure!(input.port.as_str() == "in", "invalid query input");
                let futures_due = self.execution.source_transactions.is_none()
                    && GraphChangeCodec::is_futures_due(&input.envelope);
                if futures_due {
                    let progress = GraphProducerProgress::from_envelope(&input.envelope)?;
                    if progress
                        .as_ref()
                        .is_none_or(|progress| !progress.identity().persistent() && self.durable.volatile)
                    {
                        return self.on_wakeup().await;
                    }
                }
                let scope = drasi_query_common::query_input_scope(self.input_scopes.as_ref(), &input)?;
                self.failed = true;
                let publication = self.output.processing();
                let mut output = self
                    .durable
                    .transform(
                        &self.processing,
                        self.definition.clone(),
                        self.projector.clone(),
                        input,
                        scope,
                    )
                    .await?;
                if let Some(output) = &mut output {
                    self.output.publish(output)?;
                }
                publication.complete();
                self.failed = false;
                if futures_due {
                    return self.on_wakeup().await;
                }
                Ok(output
                    .into_iter()
                    .map(|envelope| {
                        OutputEnvelope::new(PortId::try_new("out").expect("constant port"), envelope)
                            .with_current_context()
                    })
                    .collect())
            })
            .await
    }

    fn wakeup_source(&self) -> Option<Arc<dyn WakeupSource>> {
        Some(self.schedule.wakeup_source())
    }

    async fn on_wakeup(&mut self) -> anyhow::Result<Vec<OutputEnvelope>> {
        self.processing_context = ExecutionContext::default();
        anyhow::ensure!(
            self.started && !self.failed,
            "query is not ready for scheduled work"
        );
        if self.durable.has_pending_emissions() {
            return self.continue_transform().await;
        }
        let telemetry = self.telemetry.clone();
        let run = telemetry.run(QueryObservationPhase::Steady);
        let result: anyhow::Result<Vec<OutputEnvelope>> = telemetry
            .transaction(QueryObservationPhase::Steady, async {
                self.failed = true;
                let publication = self.output.processing();
                let (mut output, now) = self
                    .durable
                    .on_wakeup(
                        &self.processing,
                        self.definition.clone(),
                        self.projector.clone(),
                        run.clone(),
                    )
                    .await?;
                run.scope(async {
                    if let Some(output) = &mut output {
                        self.output.publish(output)?;
                    }
                    self.draining_futures = self
                        .schedule
                        .next_due()?
                        .is_some_and(|deadline| deadline <= now);
                    publication.complete();
                    self.failed = false;
                    Ok(output
                        .into_iter()
                        .map(|envelope| {
                            OutputEnvelope::new(
                                PortId::try_new("out").expect("constant port"),
                                envelope,
                            )
                            .with_current_context()
                        })
                        .collect())
                })
                .await
            })
            .await;
        self.processing_context = run.context()?.unwrap_or_default();
        run.finish(
            if result.is_ok() {
                QueryObservationOutcome::Success
            } else {
                QueryObservationOutcome::Error
            },
            result.as_ref().ok().map(|outputs| outputs.len() as u64),
        )?;
        Ok(result?
            .into_iter()
            .map(|output| output.with_execution_context(self.processing_context.clone()))
            .collect())
    }
    fn bootstrap_input(&mut self) -> Option<BootstrapInput<'_>> {
        Some(BootstrapInput::Durable(self))
    }

    fn durable_output(&mut self) -> Option<&mut dyn NativeDurableOutput> {
        Some(self)
    }
}

#[async_trait]
impl NativeInputBindings for QueryTransformer {
    async fn bind_inputs(&mut self, bindings: &[InputBinding]) -> anyhow::Result<()> {
        self.input_scopes = if self
            .execution
            .inputs
            .iter()
            .any(|input| !input.pipeline.is_empty())
        {
            Some(drasi_query_common::query_input_scopes(
                &self.definition.id,
                &self.execution.inputs,
                bindings,
            )?)
        } else {
            None
        };
        Ok(())
    }
}

#[async_trait]
impl NativeDurableOutput for QueryTransformer {
    async fn delivery_completed(&mut self, outputs: &[OutputEnvelope]) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.started && !self.failed,
            "query is not ready for confirmation"
        );
        self.failed = true;
        self.telemetry
            .transaction(
                QueryObservationPhase::Steady,
                self.durable.delivery_completed(&self.processing, outputs),
            )
            .await?;
        self.failed = false;
        Ok(())
    }

    fn resume_delivery(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.started && !self.failed,
            "query is not ready for delivery resumption"
        );
        self.durable.resume_delivery();
        Ok(())
    }
}

#[async_trait]
impl NativeBootstrapInput for QueryTransformer {
    fn bootstrap_status(&self) -> anyhow::Result<BootstrapStatus> {
        Ok(self.durable.bootstrap_status())
    }

    async fn bootstrap(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        let telemetry = self.telemetry.clone();
        let run = self
            .bootstrap_run
            .get_or_insert_with(|| telemetry.run(QueryObservationPhase::Bootstrap))
            .clone();
        run.start(QueryObservationKind::Bootstrap)?;
        let result = run
            .scope(async {
                anyhow::ensure!(!self.failed, "query initialization is fenced");
                anyhow::ensure!(input.port.as_str() == "in", "invalid query bootstrap input");
                let scope = self.input_scopes.as_ref().and_then(|scopes| {
                    scopes
                        .get(&(input.port.clone(), input.envelope.system().stream().clone()))
                        .cloned()
                });
                self.failed = true;
                self.durable
                    .bootstrap(
                        &self.processing,
                        self.definition.clone(),
                        self.projector.clone(),
                        input,
                        scope,
                    )
                    .await?;
                self.failed = false;
                Ok(())
            })
            .await;
        if result.is_err() {
            run.finish(QueryObservationOutcome::Error, None)?;
            self.bootstrap_run.take();
        }
        result
    }

    async fn bootstrap_completed(&mut self, completion: BootstrapCompletion) -> anyhow::Result<()> {
        let telemetry = self.telemetry.clone();
        let needs_initialization = !self.durable.bootstrap_status().complete;
        let run = self
            .bootstrap_run
            .get_or_insert_with(|| telemetry.run(QueryObservationPhase::Bootstrap))
            .clone();
        if needs_initialization {
            run.start(QueryObservationKind::Bootstrap)?;
        }
        let result = run
            .scope(async {
                anyhow::ensure!(!self.failed, "query initialization is fenced");
                self.failed = true;
                self.durable
                    .complete_bootstrap(&self.processing, completion)
                    .await?;
                self.started = true;
                self.failed = false;
                Ok(())
            })
            .await;
        run.finish(
            if result.is_ok() {
                QueryObservationOutcome::Success
            } else {
                QueryObservationOutcome::Error
            },
            None,
        )?;
        self.bootstrap_run.take();
        result
    }
}

#[async_trait]
impl NativeDurableBootstrapInput for QueryTransformer {
    async fn reset_bootstrap(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(!self.failed, "query initialization is fenced");
        self.failed = true;
        self.durable
            .start(
                &self.processing,
                self.definition.clone(),
                self.execution.clone(),
                self.middleware.clone(),
                true,
            )
            .await?;
        self.failed = false;
        Ok(())
    }
}

#[cfg(test)]
mod declared_middleware_tests {
    use super::*;
    use serde_json::json;

    fn configuration(kind: &str) -> serde_json::Value {
        json!({"query": "MATCH (n) RETURN n",
               "middleware": [{"name": "m", "kind": kind, "config": {}}]})
    }

    #[test]
    fn the_factory_declares_exactly_the_registered_middleware_kinds() {
        let factory = IndexedQueryFactory::default();
        let schema = factory.metadata().configuration;
        for kind in default_middleware_registry().kinds() {
            assert!(schema.validate(&configuration(kind)).is_ok(), "{kind}");
        }
        assert!(schema.validate(&configuration("unregistered")).is_err());
        assert!(schema
            .validate(&json!({"query": "MATCH (n) RETURN n",
                              "middleware": [{"name": "m", "kind": "map"}]}))
            .is_err());

        let mut registry = MiddlewareTypeRegistry::new();
        registry.register(Arc::new(
            drasi_middleware::relabel::RelabelMiddlewareFactory::new(),
        ));
        let narrowed = IndexedQueryFactory::new(Arc::new(registry))
            .metadata()
            .configuration;
        assert!(narrowed.validate(&configuration("relabel")).is_ok());
        assert!(narrowed.validate(&configuration("map")).is_err());
    }
}
