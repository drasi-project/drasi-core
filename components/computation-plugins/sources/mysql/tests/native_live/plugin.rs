// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The shipped plugin factories, linked in process through the Host SDK, with
//! their real flat configuration and declared descriptions.

use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_host_sdk::computation::{
    NativeBootstrapFactory, NativeBootstrapProxy, NativeFactory, NativePlugin,
    NativeSourceBootstrapProgress,
};
use drasi_lib::computation::v1::{
    ComponentFailure, ComponentId, ComponentSpecification, ComputationGraphBuilder,
    ConfigurationResolver, ConfigurationValue, GraphError, GraphRevision, GraphSelection,
    QuerySourceProgress, ResourceId,
};
use drasi_mysql_common::native::{
    settings::StartFrom, MySqlConfig, MySqlRetention, MySqlSourceSettings, MySqlStartPosition,
    MySqlTableKey,
};
use serde_json::{json, Value};
use std::sync::Arc;

/// The secret store key of the source password.
const PASSWORD: &str = "source-password";

/// The in-process source plugin.
pub fn source_plugin() -> Result<Arc<NativePlugin>> {
    NativePlugin::from_definition(drasi_computation_source_mysql::plugin()?)
}

/// The shipped source factory named `implementation`.
pub fn source_factory(implementation: &str) -> Result<Arc<NativeFactory>> {
    source_plugin()?
        .factories()
        .iter()
        .find(|factory| factory.metadata().implementation.name.as_ref() == implementation)
        .cloned()
        .with_context(|| format!("MySQL source factory {implementation}"))
}

/// The shipped bootstrap factory named `implementation`.
pub fn bootstrap_factory(implementation: &str) -> Result<Arc<NativeBootstrapFactory>> {
    NativePlugin::from_definition(drasi_computation_bootstrap_mysql::plugin()?)?
        .bootstrap_factories()
        .iter()
        .find(|factory| factory.metadata().implementation.name.as_ref() == implementation)
        .cloned()
        .with_context(|| format!("MySQL bootstrap factory {implementation}"))
}

/// The flat factory configuration equivalent to the engine `settings`, with
/// the mode field `stream` and, for tracked factories, `retention`.
pub fn configuration(settings: &MySqlConfig, stream: &str) -> Result<Value> {
    let connection = &settings.connection;
    let mut flat = MySqlSourceSettings::new(
        connection.host.clone(),
        connection.port,
        connection.database.clone(),
        connection.user.clone(),
    );
    flat.password = connection.password.clone();
    flat.ssl_mode = connection.ssl_mode;
    flat.tls_ca_pem = connection.tls_ca_pem.clone();
    flat.tls_client_cert_pem = connection.tls_client_cert_pem.clone();
    flat.tls_client_key_pem = connection.tls_client_key_pem.clone();
    flat.server_id = Some(connection.server_id);
    flat.heartbeat_interval_ms = connection.heartbeat_interval_ms;
    flat.tables = settings.tables.clone();
    flat.table_keys = settings
        .table_keys
        .iter()
        .map(|key| MySqlTableKey {
            table: key.table.clone(),
            key_columns: key.key_columns.clone(),
        })
        .collect();
    match &settings.start {
        MySqlStartPosition::End => flat.start_from = StartFrom::End,
        MySqlStartPosition::Start => flat.start_from = StartFrom::Beginning,
        MySqlStartPosition::Gtid { set } => {
            flat.start_from = StartFrom::Gtid;
            flat.start_gtid_set = Some(set.clone());
        }
        MySqlStartPosition::Position { file, position } => {
            flat.start_from = StartFrom::Position;
            flat.start_binlog_file = Some(file.clone());
            flat.start_binlog_position = Some(*position);
        }
        MySqlStartPosition::Snapshot { lock_timeout_ms } => {
            flat.start_from = StartFrom::Snapshot;
            flat.snapshot_lock_timeout_ms = *lock_timeout_ms;
        }
    }
    flat.max_transaction_changes = settings.transactions.max_changes;
    flat.max_transaction_bytes = settings.transactions.max_bytes;
    flat.max_transaction_duration_ms = settings.transactions.max_duration_ms;
    flat.max_protocol_bytes = settings.max_protocol_bytes;
    flat.io_timeout_ms = settings.io_timeout_ms;
    flat.reconnect_budget_ms = settings.reconnect_budget_ms;
    let mut value = serde_json::to_value(flat)?;
    let object = value.as_object_mut().context("flat MySQL settings")?;
    object.insert("stream".into(), json!(stream));
    if let Some(retention) = settings.replay {
        object.insert(
            "retention".into(),
            json!(match retention {
                MySqlRetention::Server => "server",
                MySqlRetention::UntilProcessed => "untilProcessed",
            }),
        );
    }
    Ok(value)
}

/// The managed definition of source `id`: the factory's declared descriptor
/// and literal settings, with the password as a secret reference into the
/// `settings` resource (see [`Secrets`]).
pub fn specification(
    factory: &NativeFactory,
    id: ComponentId,
    settings: &MySqlConfig,
    stream: &str,
) -> Result<ComponentSpecification> {
    let mut literal = configuration(settings, stream)?;
    literal
        .as_object_mut()
        .context("flat MySQL settings")?
        .remove("password");
    let mut specification = factory.specification(id, literal)?;
    specification.configuration.insert(
        "password".into(),
        ConfigurationValue::Reference {
            resource: ResourceId::try_new("settings")?,
            key: PASSWORD.into(),
            secret: true,
        },
    );
    Ok(specification)
}

/// The secret store holding the source password of `settings`.
pub struct Secrets(pub String);

impl Secrets {
    pub fn of(settings: &MySqlConfig) -> Arc<Self> {
        Arc::new(Self(settings.connection.password.clone()))
    }
}

#[async_trait]
impl ConfigurationResolver for Secrets {
    fn validate_reference(&self, key: &str) -> Result<()> {
        anyhow::ensure!(key == PASSWORD, "unknown MySQL secret reference");
        Ok(())
    }
    async fn resolve(&self, key: &str) -> Result<Value> {
        self.validate_reference(key)?;
        Ok(Value::String(self.0.clone()))
    }
}

/// The scope of a graph whose queries use the default recovery scope (the
/// graph ID).
pub fn scope(graph: &str) -> drasi_computation_plugin_sdk::Scope {
    drasi_computation_plugin_sdk::Scope {
        instance_id: graph.into(),
        graph_id: graph.into(),
        generation: 1,
    }
}

/// The coordinated snapshot providers of `source`'s queries, as the graph
/// creates them: the source factory's declared bootstrap coordinator, opened
/// from the source's own configuration and progress, and one paired
/// `bootstrap` provider per query owner.
pub fn paired_providers(
    source_implementation: &str,
    bootstrap_implementation: &str,
    source: &ComponentId,
    settings: &MySqlConfig,
    stream: &str,
    graph: &str,
    progress: NativeSourceBootstrapProgress,
) -> Result<Vec<(ComponentId, Arc<NativeBootstrapProxy>)>> {
    let factory = source_factory(source_implementation)?;
    let bootstrap = bootstrap_factory(bootstrap_implementation)?;
    anyhow::ensure!(
        bootstrap.requires_source_bootstrap(),
        "{bootstrap_implementation} is not paired with its source"
    );
    let owners: Vec<Arc<QuerySourceProgress>> = match &progress {
        NativeSourceBootstrapProgress::Single(owner) => vec![owner.clone()],
        NativeSourceBootstrapProgress::Shared(consumers) => {
            consumers.owners().values().cloned().collect()
        }
    };
    let coordinator = factory.create_source_bootstrap(
        source.clone(),
        configuration(settings, stream)?,
        scope(graph),
        progress,
    )?;
    owners
        .into_iter()
        .map(|owner| {
            let query = owner.component_id().clone();
            let provider = bootstrap.create_paired_query_provider(
                query.clone(),
                json!({}),
                scope(graph),
                owner,
                coordinator.clone(),
                None,
            )?;
            Ok((query, provider))
        })
        .collect()
}

/// Runs the graph `builder` (which declares the source `source` and nothing
/// that can start successfully without it) and returns the failure the graph
/// recorded on the source's node. The source must never start.
pub async fn node_failure(
    builder: ComputationGraphBuilder,
    source: &ComponentId,
) -> Result<ComponentFailure> {
    let mut graph = builder.build()?;
    let run = graph.run()?;
    let control = run.control();
    let (run, outcome) = tokio::join!(run, async {
        let outcome = async {
            control
                .start_components(GraphRevision(1), GraphSelection::All)
                .await?;
            let node = control.observed().components[source].clone();
            let failure = node.failure.context("the source node records no failure")?;
            control
                .stop_components(GraphRevision(1), GraphSelection::All)
                .await?;
            Ok::<_, anyhow::Error>(failure)
        }
        .await;
        control.cancel();
        outcome
    });
    let cleanup = graph.shutdown().await;
    let failure = outcome?;
    anyhow::ensure!(
        matches!(run, Ok(()) | Err(GraphError::Cancelled)),
        "graph run: {run:?}"
    );
    cleanup?;
    Ok(failure)
}
