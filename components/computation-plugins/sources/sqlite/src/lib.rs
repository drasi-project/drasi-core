// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

mod component;
mod coordinator;

use anyhow::{Context, Result};
use drasi_computation_contracts::{
    ComponentId, ComponentRole, GraphChangeCodec, ImplementationIdentity, SourceTransactionCodec,
    StreamId,
};
use drasi_computation_plugin_sdk::{
    Component, ConfigField, ConfigSchema, ConfigType, ControlSender, CreateRequest,
    CreatedComponent, Factory, FactoryMetadata, NativeConsumerProgress,
    NativeSourceBootstrapCoordinator, NativeSourceProgress, PluginDefinition,
    SourceBootstrapProgress,
};
use std::sync::Arc;

pub use component::SqliteSource;
pub use drasi_sqlite_common::{
    native::{
        SqliteConfig, SqliteHandle, SqliteOutput, SqliteReplayConfig, SqliteRow,
        SqliteTransactionHandle,
    },
    RestApiConfig, SqliteParam, TableKeyConfig,
};

pub const PLUGIN_ID: &str = env!("CARGO_PKG_NAME");
pub const SQLITE_CHANGES: &str = "drasi.sqlite/changes";
pub const SQLITE_TRANSACTIONS: &str = "drasi.sqlite/transactions";
pub const SQLITE_REPLAY: &str = "drasi.sqlite/replay-transactions";
pub const SQLITE_SHARED: &str = "drasi.sqlite/shared-transactions";

#[derive(Clone, Copy)]
enum Mode {
    Changes,
    Transactions,
    Replay,
    Shared,
}

struct SqliteFactory(Mode);

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Configuration {
    stream: String,
    settings: SqliteConfig,
    rest_api: Option<RestApiConfig>,
    #[serde(default)]
    coordinated_snapshot: bool,
}

pub fn plugin() -> Result<PluginDefinition> {
    PluginDefinition::new(
        PLUGIN_ID,
        env!("CARGO_PKG_VERSION"),
        [
            Mode::Changes,
            Mode::Transactions,
            Mode::Replay,
            Mode::Shared,
        ]
        .into_iter()
        .map(|mode| Arc::new(SqliteFactory(mode)) as Arc<dyn Factory>)
        .collect(),
        vec![GraphChangeCodec::schema(), SourceTransactionCodec::schema()],
    )
}

impl SqliteFactory {
    fn name(&self) -> &'static str {
        match self.0 {
            Mode::Changes => SQLITE_CHANGES,
            Mode::Transactions => SQLITE_TRANSACTIONS,
            Mode::Replay => SQLITE_REPLAY,
            Mode::Shared => SQLITE_SHARED,
        }
    }
    fn output(&self) -> SqliteOutput {
        match self.0 {
            Mode::Changes => SqliteOutput::Changes,
            _ => SqliteOutput::Transactions,
        }
    }
    fn configuration(&self, request: &CreateRequest) -> Result<Configuration> {
        self.metadata()
            .configuration
            .validate(&request.configuration)?;
        let config: Configuration = serde_json::from_value(request.configuration.clone())?;
        config.settings.validate()?;
        if let Some(rest) = &config.rest_api {
            rest.validate()?;
        }
        anyhow::ensure!(
            config.settings.output == self.output(),
            "SQLite factory output mode does not match settings"
        );
        anyhow::ensure!(
            config.settings.replay.is_some() == matches!(self.0, Mode::Replay | Mode::Shared),
            "SQLite factory replay mode does not match settings"
        );
        anyhow::ensure!(
            !config.coordinated_snapshot || matches!(self.0, Mode::Replay | Mode::Shared),
            "SQLite coordinated initialization requires persistent transaction replay"
        );
        Ok(config)
    }
    fn component(
        &self,
        source: SqliteSource,
        rest: Option<RestApiConfig>,
    ) -> Result<CreatedComponent> {
        let source = match rest {
            Some(rest) => source.with_rest_api(rest)?,
            None => source,
        };
        Ok(Component::Source(Box::new(source)).into())
    }
}

impl Factory for SqliteFactory {
    fn metadata(&self) -> FactoryMetadata {
        let mut configuration = ConfigSchema::from_fields([
            ("stream", ConfigType::String, true),
            ("settings", ConfigType::Object, true),
            ("rest_api", ConfigType::Object, false),
        ]);
        if matches!(self.0, Mode::Replay | Mode::Shared) {
            configuration.fields.insert(
                "coordinated_snapshot".into(),
                ConfigField {
                    value_type: ConfigType::Boolean,
                    required: false,
                    secret: false,
                },
            );
        }
        FactoryMetadata {
            implementation: ImplementationIdentity::try_new(self.name(), "1")
                .expect("constant implementation"),
            role: ComponentRole::Source,
            completion: None,
            ports: SqliteSource::describe(
                ComponentId::try_new("sqlite").expect("constant id"),
                self.output(),
            )
            .expect("source descriptor")
            .ports()
            .to_vec(),
            configuration_version: 1,
            configuration,
            capabilities: Default::default(),
        }
    }
    fn supports_source_progress(&self) -> bool {
        matches!(self.0, Mode::Replay)
    }
    fn supports_consumer_progress(&self) -> bool {
        matches!(self.0, Mode::Shared)
    }
    fn supports_bootstrap_coordination(&self) -> bool {
        matches!(self.0, Mode::Replay | Mode::Shared)
    }
    fn create_bootstrap_coordinator(
        &self,
        request: &CreateRequest,
        progress: SourceBootstrapProgress,
    ) -> Result<Arc<dyn NativeSourceBootstrapCoordinator>> {
        let config = self.configuration(request)?;
        anyhow::ensure!(
            config.coordinated_snapshot,
            "SQLite coordinator requires coordinated_snapshot=true"
        );
        anyhow::ensure!(
            matches!(
                (&self.0, &progress),
                (Mode::Replay, SourceBootstrapProgress::Single(_))
                    | (Mode::Shared, SourceBootstrapProgress::Shared { .. })
            ),
            "SQLite coordinator progress mode does not match its source factory"
        );
        Ok(Arc::new(coordinator::Coordinator::new(request, progress)?))
    }
    fn create(&self, request: &CreateRequest, _: ControlSender) -> Result<CreatedComponent> {
        anyhow::ensure!(
            !matches!(self.0, Mode::Replay | Mode::Shared),
            "SQLite replay requires actual consumer progress"
        );
        let config = self.configuration(request)?;
        self.component(
            SqliteSource::new(
                request.id.clone(),
                StreamId::try_new(config.stream)?,
                config.settings,
            )?,
            config.rest_api,
        )
    }
    fn create_with_progress(
        &self,
        request: &CreateRequest,
        _: ControlSender,
        progress: NativeSourceProgress,
    ) -> Result<CreatedComponent> {
        anyhow::ensure!(
            matches!(self.0, Mode::Replay),
            "SQLite factory does not accept single-owner progress"
        );
        let config = self.configuration(request)?;
        anyhow::ensure!(
            !config.coordinated_snapshot,
            "SQLite coordinated source requires its actual coordinator resource"
        );
        let scope = request
            .scope
            .as_ref()
            .context("SQLite replay requires graph scope")?;
        anyhow::ensure!(
            scope.graph_id == progress.reader().graph_id(),
            "SQLite progress belongs to another graph"
        );
        self.component(
            SqliteSource::new_replayable(
                request.id.clone(),
                StreamId::try_new(config.stream)?,
                config.settings,
                progress,
            )?,
            config.rest_api,
        )
    }
    fn create_with_consumer_progress(
        &self,
        request: &CreateRequest,
        _: ControlSender,
        progress: NativeConsumerProgress,
    ) -> Result<CreatedComponent> {
        anyhow::ensure!(
            matches!(self.0, Mode::Shared),
            "SQLite factory does not accept a consumer group"
        );
        let config = self.configuration(request)?;
        anyhow::ensure!(
            !config.coordinated_snapshot,
            "SQLite coordinated source requires its actual coordinator resource"
        );
        let scope = request
            .scope
            .as_ref()
            .context("SQLite replay requires graph scope")?;
        anyhow::ensure!(
            scope.graph_id == progress.reader().graph_id(),
            "SQLite consumer group belongs to another graph"
        );
        self.component(
            SqliteSource::new_replayable_with_consumers(
                request.id.clone(),
                StreamId::try_new(config.stream)?,
                config.settings,
                progress,
            )?,
            config.rest_api,
        )
    }
}

#[cfg(feature = "dynamic-plugin")]
drasi_computation_plugin_sdk::export_computation_plugin!(plugin());
