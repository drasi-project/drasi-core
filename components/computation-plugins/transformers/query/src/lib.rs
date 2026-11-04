// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

mod bootstrap;
mod durable;
mod indexed;
mod output;

use std::{num::NonZeroUsize, sync::Arc};

use anyhow::Context;
use async_trait::async_trait;
use chrono::Utc;
use drasi_computation_contracts::profiling::timestamp_ns;
use drasi_computation_contracts::ComputationQueryLanguage as QueryLanguage;
use drasi_computation_contracts::{
    BootstrapCompletion, BootstrapStatus, ChangeEnvelope, ComponentDescriptor, ComponentRole,
    ComponentSemanticKind, GraphChangeCodec, GraphProducerProgress, ImplementationIdentity,
    InputBinding, InputEnvelope, OutputCursor, OutputEnvelope, OutputProgress, OutputReaders,
    PipeRequirements, PortDescriptor, PortDirection, PortId, QueryChangeCodec, QueryDescription,
    QueryExecutionSettings, ResultNodeProjector, StreamId, WakeupSource,
};
use drasi_computation_plugin_sdk::{
    Capabilities, Component, ConfigSchema, ConfigType, ControlSender, CreateRequest,
    CreatedComponent, Factory, FactoryMetadata, NativeComponent, NativeOutputSnapshots,
    NativeProcessing, NativeTransformer, PluginDefinition,
};
use drasi_core::{
    computation::QueryInputBatch, middleware::MiddlewareTypeRegistry, query::QueryEvaluator,
};
use drasi_query_common::{
    query_builder, ContinuousQueryDefinition, QueryExecutionSettingsExt, QueryInputScopes,
    QueryOperation, QuerySchedule,
};

const IMPLEMENTATION: &str = "drasi.experimental/volatile-query";
const INDEXED_IMPLEMENTATION: &str = "drasi.experimental/indexed-query";
const PROVIDER_IMPLEMENTATION: &str = "drasi.experimental/provider-query";

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
        vec![
            Arc::new(QueryFactory::new(middleware.clone())),
            Arc::new(IndexedQueryFactory::new(middleware.clone())),
            Arc::new(ProviderQueryFactory::new(middleware)),
        ],
        vec![GraphChangeCodec::schema()],
    )
}

#[cfg(feature = "dynamic-plugin")]
drasi_computation_plugin_sdk::export_computation_plugin!(plugin());

pub struct QueryFactory {
    middleware: Arc<MiddlewareTypeRegistry>,
}

impl QueryFactory {
    pub fn new(middleware: Arc<MiddlewareTypeRegistry>) -> Self {
        Self { middleware }
    }
}

impl Default for QueryFactory {
    fn default() -> Self {
        Self::new(default_middleware_registry())
    }
}

impl Factory for QueryFactory {
    fn metadata(&self) -> FactoryMetadata {
        FactoryMetadata {
            implementation: ImplementationIdentity::try_new(IMPLEMENTATION, "1")
                .expect("constant implementation"),
            role: ComponentRole::Transformer,
            configuration_version: 1,
            configuration: ConfigSchema::from_fields([
                ("query", ConfigType::String, true),
                ("volatile", ConfigType::Boolean, true),
                ("language", ConfigType::String, false),
                ("joins", ConfigType::Array, false),
                ("inputs", ConfigType::Array, false),
                ("middleware", ConfigType::Array, false),
                ("result", ConfigType::Object, false),
                ("stream", ConfigType::String, false),
                ("outbox_capacity", ConfigType::Integer, false),
                ("bootstrap", ConfigType::Array, false),
            ]),
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
                binds_inputs: true,
                output_snapshots: vec![PortId::try_new("out").expect("constant port")],
                output_follows: vec![PortId::try_new("out").expect("constant port")],
                output_queries: vec![PortId::try_new("out").expect("constant port")],
                query_observations: true,
                query_recovery_views: true,
                continuations: true,
                wakeups: true,
                input_binding_updates: true,
                ..Capabilities::default()
            },
        }
    }

    fn create(
        &self,
        request: &CreateRequest,
        control: ControlSender,
    ) -> anyhow::Result<CreatedComponent> {
        self.create_with_output_snapshots(request, control, None)
    }

    fn supports_output_snapshots(&self) -> bool {
        true
    }

    fn create_with_output_snapshots(
        &self,
        request: &CreateRequest,
        _: ControlSender,
        snapshots: Option<NativeOutputSnapshots>,
    ) -> anyhow::Result<CreatedComponent> {
        anyhow::ensure!(
            request.configuration["volatile"] == true,
            "this experimental query transformer requires volatile: true"
        );
        let bootstrap = bootstrap::SnapshotBootstrap::from_request(request, snapshots)?;
        create_query(
            request,
            self.metadata(),
            None,
            bootstrap,
            self.middleware.clone(),
        )
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

impl Factory for IndexedQueryFactory {
    fn metadata(&self) -> FactoryMetadata {
        let mut metadata = QueryFactory::new(self.middleware.clone()).metadata();
        metadata.implementation =
            ImplementationIdentity::try_new(INDEXED_IMPLEMENTATION, "1").expect("constant");
        metadata.configuration = ConfigSchema::from_fields([
            ("query", ConfigType::String, true),
            ("volatile", ConfigType::Boolean, false),
            ("publication", ConfigType::String, false),
            ("language", ConfigType::String, false),
            ("joins", ConfigType::Array, false),
            ("inputs", ConfigType::Array, false),
            ("middleware", ConfigType::Array, false),
            ("result", ConfigType::Object, false),
            ("stream", ConfigType::String, false),
            ("outbox_capacity", ConfigType::Integer, false),
            ("bootstrap", ConfigType::Boolean, false),
            ("source_transactions", ConfigType::Object, false),
            ("recovery", ConfigType::String, false),
            ("reset_configuration", ConfigType::Boolean, false),
        ]);
        metadata.capabilities.indexed_processing = true;
        metadata.capabilities.volatile_processing = true;
        metadata.capabilities.non_atomic_publication = true;
        metadata.capabilities.committed_input_progress =
            Some(PortId::try_new("in").expect("constant port"));
        metadata.capabilities.durable_bootstrap =
            Some(PortId::try_new("in").expect("constant port"));
        metadata.capabilities.output_replays = vec![PortId::try_new("out").expect("constant port")];
        metadata.capabilities.continuations = true;
        metadata.capabilities.startup_continuation = true;
        metadata.capabilities.durable_output = true;
        metadata.capabilities.deprovision = true;
        metadata.capabilities.binds_inputs = false;
        metadata
    }

    fn create(&self, _: &CreateRequest, _: ControlSender) -> anyhow::Result<CreatedComponent> {
        anyhow::bail!("indexed query requires host-owned indexed processing")
    }

    fn create_with_processing(
        &self,
        request: &CreateRequest,
        _: ControlSender,
        processing: NativeProcessing,
    ) -> anyhow::Result<CreatedComponent> {
        create_query(
            request,
            self.metadata(),
            Some(processing),
            None,
            self.middleware.clone(),
        )
    }
}

pub struct ProviderQueryFactory {
    middleware: Arc<MiddlewareTypeRegistry>,
}

impl ProviderQueryFactory {
    pub fn new(middleware: Arc<MiddlewareTypeRegistry>) -> Self {
        Self { middleware }
    }
}

impl Default for ProviderQueryFactory {
    fn default() -> Self {
        Self::new(default_middleware_registry())
    }
}

impl Factory for ProviderQueryFactory {
    fn metadata(&self) -> FactoryMetadata {
        let mut metadata = QueryFactory::new(self.middleware.clone()).metadata();
        metadata.implementation =
            ImplementationIdentity::try_new(PROVIDER_IMPLEMENTATION, "1").expect("constant");
        metadata.configuration = ConfigSchema::from_fields([
            ("query", ConfigType::String, true),
            ("volatile", ConfigType::Boolean, true),
            ("language", ConfigType::String, false),
            ("joins", ConfigType::Array, false),
            ("inputs", ConfigType::Array, false),
            ("middleware", ConfigType::Array, false),
            ("result", ConfigType::Object, false),
            ("stream", ConfigType::String, false),
            ("outbox_capacity", ConfigType::Integer, false),
        ]);
        metadata.capabilities.binds_inputs = false;
        metadata.capabilities.quiescent_bootstrap =
            Some(PortId::try_new("in").expect("constant port"));
        metadata
    }

    fn create(
        &self,
        request: &CreateRequest,
        _: ControlSender,
    ) -> anyhow::Result<CreatedComponent> {
        anyhow::ensure!(
            request.configuration["volatile"] == true,
            "this experimental provider query requires volatile: true"
        );
        create_query(
            request,
            self.metadata(),
            None,
            None,
            self.middleware.clone(),
        )
    }
}

fn create_query(
    request: &CreateRequest,
    metadata: FactoryMetadata,
    processing: Option<NativeProcessing>,
    bootstrap: Option<bootstrap::SnapshotBootstrap>,
    middleware: Arc<MiddlewareTypeRegistry>,
) -> anyhow::Result<CreatedComponent> {
    let query = request.configuration["query"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("query must be a string"))?;
    let language = match request.configuration.get("language") {
        None => QueryLanguage::Cypher,
        Some(serde_json::Value::String(language)) if language == "cypher" => QueryLanguage::Cypher,
        Some(serde_json::Value::String(language)) if language == "gql" => QueryLanguage::Gql,
        Some(_) => anyhow::bail!("query language must be 'cypher' or 'gql'"),
    };
    let execution: QueryExecutionSettings = serde_json::from_value(serde_json::Value::Object(
        [
            "joins",
            "result",
            "source_transactions",
            "inputs",
            "middleware",
        ]
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
    execution.validate(Some(&middleware))?;
    let input_scopes = if !request.inputs.is_empty()
        && execution
            .inputs
            .iter()
            .any(|input| !input.pipeline.is_empty())
    {
        Some(drasi_query_common::query_input_scopes(
            request.id.as_str(),
            &execution.inputs,
            &request.inputs,
        )?)
    } else {
        None
    };
    let id = request.id.clone();
    let provider_bootstrap = metadata.capabilities.quiescent_bootstrap.is_some();
    let stream = match request.configuration.get("stream") {
        Some(value) => StreamId::try_new(value.as_str().context("query stream must be a string")?)?,
        None => StreamId::try_new(format!("{id}/out"))?,
    };
    let capacity = request
        .configuration
        .get("outbox_capacity")
        .map(|value| serde_json::from_value::<NonZeroUsize>(value.clone()))
        .transpose()
        .context("invalid query output retention capacity")?
        .unwrap_or_else(|| NonZeroUsize::new(1).expect("positive default retention"));
    let definition = Arc::new(ContinuousQueryDefinition {
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
    });
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
        definition.configuration_hash(&execution)?,
        QueryDescription {
            graph_id: definition.graph_id.clone(),
            query: definition.query.clone(),
            language,
            execution: execution.clone(),
            output_stream: stream.clone(),
            outbox_capacity: capacity,
            bootstrap: provider_bootstrap
                || request
                    .configuration
                    .get("bootstrap")
                    .is_some_and(|value| value == true || value.is_array()),
            auto_reset: request
                .configuration
                .get("recovery")
                .and_then(serde_json::Value::as_str)
                == Some("auto_reset"),
            volatile: processing.is_none()
                || request
                    .configuration
                    .get("volatile")
                    .is_some_and(|value| value == true),
            configuration_hash: definition.configuration_hash(&execution)?,
        },
    ));
    let schedule = QuerySchedule::default();
    let durable = processing
        .as_ref()
        .map(|_| {
            durable::DurableQuery::new(
                request,
                &definition,
                output.clone(),
                &execution,
                schedule.clone(),
            )
        })
        .transpose()?;
    Ok(Component::Transformer(Box::new(QueryTransformer {
        descriptor: metadata.descriptor(id.clone())?,
        configuration: request.configuration.clone(),
        definition,
        execution: Arc::new(execution),
        middleware,
        input_scopes,
        projector,
        output,
        processing,
        durable,
        bootstrap,
        provider_bootstrap,
        provider_rows: None,
        input_cursors: None,
        evaluator: None,
        started: false,
        sequence: 0,
        failed: false,
        schedule,
        draining_futures: false,
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
    configuration: serde_json::Value,
    definition: Arc<ContinuousQueryDefinition>,
    execution: Arc<QueryExecutionSettings>,
    middleware: Arc<MiddlewareTypeRegistry>,
    input_scopes: Option<QueryInputScopes>,
    projector: ResultNodeProjector,
    output: Arc<output::QueryOutput>,
    processing: Option<NativeProcessing>,
    durable: Option<durable::DurableQuery>,
    bootstrap: Option<bootstrap::SnapshotBootstrap>,
    provider_bootstrap: bool,
    provider_rows: Option<output::Rows>,
    input_cursors: Option<bootstrap::InputCursors>,
    evaluator: Option<QueryEvaluator>,
    started: bool,
    sequence: u64,
    failed: bool,
    schedule: QuerySchedule,
    draining_futures: bool,
}

#[async_trait]
impl NativeComponent for QueryTransformer {
    fn supports_input_binding_updates(&self) -> bool {
        true
    }

    async fn bind_inputs(&mut self, bindings: &[InputBinding]) -> anyhow::Result<()> {
        self.input_scopes = if self
            .execution
            .inputs
            .iter()
            .any(|input| !input.pipeline.is_empty())
        {
            Some(drasi_query_common::query_input_scopes(
                self.definition.id.as_str(),
                &self.execution.inputs,
                bindings,
            )?)
        } else {
            None
        };
        Ok(())
    }

    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }

    fn configuration(&self) -> anyhow::Result<serde_json::Value> {
        Ok(self.configuration.clone())
    }

    fn output_readers(&self) -> OutputReaders {
        let readers = OutputReaders::new()
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
            );
        if self.durable.is_some() {
            readers.with_replay(
                PortId::try_new("out").expect("constant port"),
                self.output.clone(),
            )
        } else {
            readers
        }
    }

    fn supports_deprovision(&self) -> bool {
        self.durable.is_some()
    }

    async fn deprovision(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(!self.started, "query deprovision requires completed stop");
        let processing = self
            .processing
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("query does not own persistent state"))?;
        let volatile = self.durable.as_ref().is_some_and(|query| query.volatile);
        let non_atomic = self.durable.as_ref().is_some_and(|query| query.non_atomic);
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

    async fn start(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.started && self.provider_rows.is_none(),
            "query is already started or initializing"
        );
        let was_failed = self.failed;
        if self.processing.is_none() && !self.provider_bootstrap && self.bootstrap.is_none() {
            anyhow::ensure!(
                !self.failed,
                "failed volatile query requires explicit reconstruction and bootstrap"
            );
            if let Some(evaluator) = &self.evaluator {
                self.schedule
                    .publish(evaluator.future_queue().peek_due_time().await?)?;
                self.output.activate()?;
                self.started = true;
                return Ok(());
            }
        }
        self.output.begin_activation()?;
        self.schedule.publish(None)?;
        self.draining_futures = false;
        self.failed = true;
        let mut input_cursors = None;
        if let Some(processing) = &self.processing {
            let reset = self
                .durable
                .as_ref()
                .expect("indexed state")
                .reset_failed_volatile(was_failed)?;
            self.durable
                .as_mut()
                .expect("indexed state")
                .start(
                    processing,
                    self.definition.clone(),
                    self.execution.clone(),
                    self.middleware.clone(),
                    reset,
                )
                .await?;
        } else {
            let output = &self.output;
            let evaluator = evaluator(
                &self.definition.query,
                self.definition.language,
                &self.execution,
                None,
                self.middleware.clone(),
            )
            .await?;
            if self.provider_bootstrap {
                self.evaluator = Some(evaluator);
                self.provider_rows = Some(output::Rows::new());
                self.input_cursors = None;
                self.sequence = 0;
                self.failed = false;
                return Ok(());
            }
            let rows = match &self.bootstrap {
                Some(bootstrap) => {
                    let (cursors, rows) = bootstrap
                        .load(&evaluator, output, self.input_scopes.as_ref())
                        .await?;
                    input_cursors = Some(cursors);
                    rows
                }
                None => output::Rows::new(),
            };
            output.initialize(rows)?;
            self.schedule
                .publish(evaluator.future_queue().peek_due_time().await?)?;
            self.evaluator = Some(evaluator);
        }
        self.input_cursors = input_cursors;
        self.started = true;
        self.sequence = 0;
        self.failed = false;
        Ok(())
    }

    async fn stop(&mut self) -> anyhow::Result<()> {
        if self.processing.is_some() || self.failed {
            self.evaluator = None;
        }
        self.provider_rows = None;
        self.input_cursors = None;
        self.started = false;
        self.draining_futures = false;
        self.schedule.publish(None)?;
        self.output.suspend()?;
        if let Some(durable) = &mut self.durable {
            durable.stop(self.failed)?;
        }
        Ok(())
    }
}

#[async_trait]
impl NativeTransformer for QueryTransformer {
    fn resume_delivery(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.started && !self.failed,
            "query is not ready for delivery resumption"
        );
        self.durable
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("query is not indexed"))?
            .resume_delivery();
        Ok(())
    }
    fn has_pending_emissions(&self) -> bool {
        self.draining_futures
            || self
                .durable
                .as_ref()
                .is_some_and(durable::DurableQuery::has_pending_emissions)
    }

    async fn continue_transform(&mut self) -> anyhow::Result<Vec<OutputEnvelope>> {
        anyhow::ensure!(
            self.started && !self.failed,
            "query is not ready for continuation"
        );
        if !self
            .durable
            .as_ref()
            .is_some_and(durable::DurableQuery::has_pending_emissions)
        {
            return if self.draining_futures {
                self.on_wakeup().await
            } else {
                Ok(Vec::new())
            };
        }
        let processing = self
            .processing
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("query has no indexed continuation"))?;
        self.failed = true;
        let output = self
            .durable
            .as_mut()
            .expect("indexed state")
            .continue_output(processing)
            .await?;
        self.failed = false;
        Ok(output
            .into_iter()
            .map(|envelope| OutputEnvelope {
                port: PortId::try_new("out").expect("constant port"),
                envelope,
            })
            .collect())
    }

    async fn delivery_completed(&mut self, outputs: &[OutputEnvelope]) -> anyhow::Result<()> {
        if let (Some(processing), Some(durable)) = (&self.processing, &mut self.durable) {
            anyhow::ensure!(
                self.started && !self.failed,
                "query is not ready for confirmation"
            );
            self.failed = true;
            durable.delivery_completed(processing, outputs).await?;
            self.failed = false;
        }
        Ok(())
    }

    fn quiescent_bootstrap_input(&self) -> Option<PortId> {
        self.provider_bootstrap
            .then(|| PortId::try_new("in").expect("constant port"))
    }

    fn durable_bootstrap_input(&self) -> Option<PortId> {
        self.durable
            .as_ref()
            .map(|_| PortId::try_new("in").expect("constant port"))
    }

    fn bootstrap_status(&self) -> anyhow::Result<BootstrapStatus> {
        Ok(self
            .durable
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("query has no durable bootstrap"))?
            .bootstrap_status())
    }

    async fn reset_bootstrap(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(!self.failed, "query initialization is fenced");
        let processing = self
            .processing
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("query is not indexed"))?;
        self.failed = true;
        self.durable
            .as_mut()
            .expect("indexed state")
            .start(
                processing,
                self.definition.clone(),
                self.execution.clone(),
                self.middleware.clone(),
                true,
            )
            .await?;
        self.failed = false;
        Ok(())
    }

    async fn bootstrap(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        anyhow::ensure!(!self.failed, "query initialization is fenced");
        anyhow::ensure!(input.port.as_str() == "in", "invalid query bootstrap input");
        let scope = self.input_scopes.as_ref().and_then(|scopes| {
            scopes
                .get(&(input.port.clone(), input.envelope.system().stream().clone()))
                .cloned()
        });
        if let (Some(processing), Some(durable)) = (&self.processing, &self.durable) {
            self.failed = true;
            durable
                .bootstrap(
                    processing,
                    self.definition.clone(),
                    self.projector.clone(),
                    input,
                    scope,
                )
                .await?;
            self.failed = false;
            return Ok(());
        }
        anyhow::ensure!(
            self.provider_bootstrap && self.provider_rows.is_some() && !self.failed,
            "query is not accepting provider initialization"
        );
        anyhow::ensure!(input.port.as_str() == "in", "invalid query bootstrap input");
        let changes = GraphChangeCodec::decode_changes(&input.envelope)?;
        self.failed = true;
        let results = self
            .evaluator
            .as_ref()
            .expect("initializing evaluator")
            .evaluate_input_batch::<false>(
                QueryInputBatch::new(changes, input.envelope.system().ordering())
                    .with_optional_input_scope(scope),
            )
            .await?;
        self.output.apply_initial(
            self.provider_rows.as_mut().expect("initializing rows"),
            &results,
        )?;
        self.failed = false;
        Ok(())
    }

    async fn bootstrap_completed(&mut self, completion: BootstrapCompletion) -> anyhow::Result<()> {
        anyhow::ensure!(!self.failed, "query initialization is fenced");
        if let (Some(processing), Some(durable)) = (&self.processing, &mut self.durable) {
            self.failed = true;
            durable.complete_bootstrap(processing, completion).await?;
            self.started = true;
            self.failed = false;
            return Ok(());
        }
        anyhow::ensure!(
            completion.watermarks.is_empty() && completion.state.is_none(),
            "volatile query cannot commit durable bootstrap completion"
        );
        anyhow::ensure!(
            self.provider_bootstrap && self.provider_rows.is_some() && !self.failed,
            "query is not awaiting provider completion"
        );
        self.failed = true;
        let rows = self.provider_rows.take().expect("initializing rows");
        self.output.initialize(rows)?;
        self.schedule.publish(
            self.evaluator
                .as_ref()
                .expect("initializing evaluator")
                .future_queue()
                .peek_due_time()
                .await?,
        )?;
        self.started = true;
        self.failed = false;
        Ok(())
    }

    async fn transform(&mut self, input: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>> {
        anyhow::ensure!(!self.failed, "query is fenced after a processing failure");
        anyhow::ensure!(self.started, "query is not started");
        anyhow::ensure!(input.port.as_str() == "in", "invalid query input");
        let futures_due = self.execution.source_transactions.is_none()
            && GraphChangeCodec::is_futures_due(&input.envelope);
        if futures_due {
            let progress = GraphProducerProgress::from_envelope(&input.envelope)?;
            if progress.as_ref().is_none_or(|progress| {
                !progress.identity().persistent()
                    && self.durable.as_ref().is_none_or(|query| query.volatile)
            }) {
                return self.on_wakeup().await;
            }
        }
        if let Some(processing) = &self.processing {
            let scope = drasi_query_common::query_input_scope(self.input_scopes.as_ref(), &input)?;
            self.failed = true;
            let publication = self.output.processing();
            let mut output = self
                .durable
                .as_mut()
                .expect("indexed state")
                .transform(
                    processing,
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
            return Ok(output
                .into_iter()
                .map(|envelope| OutputEnvelope {
                    port: PortId::try_new("out").expect("constant port"),
                    envelope,
                })
                .collect());
        }
        let input_cursor = if let Some(cursors) = &self.input_cursors {
            let key = (input.port.clone(), input.envelope.system().stream().clone());
            let saved = cursors.get(&key).ok_or_else(|| {
                anyhow::anyhow!("bootstrapped query received an undeclared input port/stream")
            })?;
            let progress = GraphProducerProgress::from_envelope(&input.envelope)?
                .ok_or_else(|| anyhow::anyhow!("bootstrapped query requires producer progress"))?;
            let cursor = OutputCursor::try_new(progress.identity().clone(), progress.sequence())?;
            saved.ensure_same_epoch(&cursor)?;
            if cursor.sequence() <= saved.sequence() {
                return Ok(Vec::new());
            }
            Some((key, cursor))
        } else {
            None
        };
        let changes = GraphChangeCodec::decode_changes(&input.envelope)?;
        let scope = drasi_query_common::query_input_scope(self.input_scopes.as_ref(), &input)?;
        self.failed = true;
        let publication = self.output.processing();
        let mut output = {
            let evaluator = self.evaluator.as_ref().expect("started volatile query");
            let operation = QueryOperation::new(&input.envelope)?;
            let results = evaluator
                .evaluate_input_batch::<false>(
                    QueryInputBatch::new(changes, input.envelope.system().ordering())
                        .with_optional_input_scope(scope),
                )
                .await?;
            let sequence = QueryOperation::output_sequence(self.sequence, &results)?;
            operation.project(&self.definition, &self.projector, sequence, &results)?
        };
        let core_return_ns = timestamp_ns();
        if let Some(envelope) = &mut output {
            QueryOperation::committed(envelope, &self.definition.id, core_return_ns)?;
            self.output.publish(envelope)?;
        }
        self.schedule.publish(
            self.evaluator
                .as_ref()
                .expect("started volatile query")
                .future_queue()
                .peek_due_time()
                .await?,
        )?;
        publication.complete();
        self.failed = false;
        if let Some((key, cursor)) = input_cursor {
            self.input_cursors
                .as_mut()
                .expect("bootstrapped input cursors")
                .insert(key, cursor);
        }
        if futures_due {
            return self.on_wakeup().await;
        }
        if let Some(envelope) = output {
            self.sequence = QueryChangeCodec::query_sequence(&envelope)?;
            Ok(vec![OutputEnvelope {
                port: PortId::try_new("out").expect("constant port"),
                envelope,
            }])
        } else {
            Ok(Vec::new())
        }
    }

    fn wakeup_source(&self) -> Option<Arc<dyn WakeupSource>> {
        Some(self.schedule.wakeup_source())
    }

    async fn on_wakeup(&mut self) -> anyhow::Result<Vec<OutputEnvelope>> {
        anyhow::ensure!(
            self.started && !self.failed,
            "query is not ready for scheduled work"
        );
        if self
            .durable
            .as_ref()
            .is_some_and(durable::DurableQuery::has_pending_emissions)
        {
            return self.continue_transform().await;
        }
        self.failed = true;
        let publication = self.output.processing();
        let (mut output, now) =
            if let (Some(processing), Some(durable)) = (&self.processing, &mut self.durable) {
                durable
                    .on_wakeup(processing, self.definition.clone(), self.projector.clone())
                    .await?
            } else {
                let now = u64::try_from(Utc::now().timestamp_millis())?;
                let evaluator = self.evaluator.as_ref().expect("started volatile query");
                let next_due = evaluator.future_queue().peek_due_time().await?;
                let output = if next_due.is_some_and(|deadline| deadline <= now) {
                    match evaluator.evaluate_next_future().await? {
                        Some(due) => {
                            let sequence =
                                QueryOperation::output_sequence(self.sequence, &due.results)?;
                            drasi_query_common::project_due_query_output(
                                &self.definition,
                                &self.projector,
                                sequence,
                                &due,
                            )?
                        }
                        None => None,
                    }
                } else {
                    None
                };
                self.schedule
                    .publish(evaluator.future_queue().peek_due_time().await?)?;
                (output, now)
            };
        if let Some(output) = &mut output {
            self.output.publish(output)?;
            if self.processing.is_none() {
                self.sequence = QueryChangeCodec::query_sequence(output)?;
            }
        }
        self.draining_futures = self
            .schedule
            .next_due()?
            .is_some_and(|deadline| deadline <= now);
        publication.complete();
        self.failed = false;
        Ok(output
            .into_iter()
            .map(|envelope| OutputEnvelope {
                port: PortId::try_new("out").expect("constant port"),
                envelope,
            })
            .collect())
    }
}
