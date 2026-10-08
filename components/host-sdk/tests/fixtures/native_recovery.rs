// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Test-only cdylib: the real SQLite replay source behind recovery-v1.
//! Includes a synthetic query-owned bootstrap provider, not production plugin registration.

mod native_bootstrap;
mod native_consumer;

use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_computation_plugin_sdk::{self as sdk, Factory};
use drasi_lib::computation::v1::*;
use drasi_source_sqlite::native::{SqliteConfig, SqliteHandle, SqliteSource};
use serde::Deserialize;
use std::sync::Arc;

struct SqliteFactory;

impl Factory for SqliteFactory {
    fn metadata(&self) -> sdk::FactoryMetadata {
        sdk::FactoryMetadata {
            implementation: ImplementationIdentity::try_new("fixture/sqlite-replay", "1")
                .expect("constant fixture identity"),
            role: ComponentRole::Source,
            configuration_version: 1,
            configuration: sdk::ConfigSchema {
                fields: [
                    ("stream", sdk::ConfigType::String, true),
                    ("settings", sdk::ConfigType::Object, true),
                    ("coordinated_snapshot", sdk::ConfigType::Boolean, false),
                ]
                .into_iter()
                .map(|(name, value_type, required)| {
                    (
                        name.into(),
                        sdk::ConfigField {
                            value_type,
                            required,
                            secret: false,
                        },
                    )
                })
                .collect(),
                allow_additional: false,
            },
            ports: SqliteSource::describe(
                ComponentId::try_new("source").expect("constant source identity"),
                drasi_source_sqlite::native::SqliteOutput::Transactions,
            )
            .expect("fixed SQLite transaction descriptor")
            .ports()
            .to_vec(),
            completion: None,
            capabilities: sdk::Capabilities {
                control: true,
                ..Default::default()
            },
        }
    }

    fn supports_source_progress(&self) -> bool {
        true
    }

    fn create(
        &self,
        _: &sdk::CreateRequest,
        _: sdk::ControlSender,
    ) -> Result<sdk::CreatedComponent> {
        anyhow::bail!("fixture SQLite replay requires the source_progress binding")
    }

    fn create_with_progress(
        &self,
        request: &sdk::CreateRequest,
        _: sdk::ControlSender,
        progress: sdk::NativeSourceProgress,
    ) -> Result<sdk::CreatedComponent> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Configuration {
            stream: StreamId,
            settings: SqliteConfig,
            #[serde(default)]
            coordinated_snapshot: bool,
        }
        let config: Configuration = serde_json::from_value(request.configuration.clone())?;
        anyhow::ensure!(
            !config.coordinated_snapshot,
            "fixture has no native bootstrap interface"
        );
        let source = SqliteSource::new_replayable(
            request.id.clone(),
            config.stream,
            config.settings,
            progress.reader(),
        )?;
        let commands = Commands {
            sql: source.handle(),
            progress,
        };
        Ok(sdk::CreatedComponent {
            component: sdk::Component::Source(Box::new(source)),
            control_handler: Some(Arc::new(commands)),
        })
    }
}

struct Commands {
    sql: SqliteHandle,
    progress: sdk::NativeSourceProgress,
}

#[async_trait]
impl sdk::NativeControlHandler for Commands {
    async fn on_message(&self, message: sdk::ControlMessage, _: sdk::ControlSender) -> Result<()> {
        if let sdk::ControlNotification::Custom { kind, payload } = message.notification {
            match kind.as_str() {
                "fixture.sql" => {
                    self.sql
                        .execute_batch(payload.as_str().context("expected SQL text")?)
                        .await?;
                }
                "fixture.progress" => {
                    self.progress.reader().snapshot()?;
                }
                "fixture.flush" => {
                    self.sql.query("SELECT id FROM items").await?;
                }
                _ => anyhow::bail!("unsupported fixture control {kind}"),
            }
        }
        Ok(())
    }
}

sdk::export_computation_plugin!(sdk::PluginDefinition::new(
    "fixture/native-recovery",
    env!("CARGO_PKG_VERSION"),
    vec![
        Arc::new(SqliteFactory),
        Arc::new(native_consumer::Factory(sdk::ConsumerMode::External)),
        Arc::new(native_consumer::Factory(sdk::ConsumerMode::Transactional)),
    ],
    vec![GraphChangeCodec::schema(), SourceTransactionCodec::schema()],
)
.and_then(|definition| definition.with_bootstrap_factories(vec![
    Arc::new(native_bootstrap::Factory { progress: false }),
    Arc::new(native_bootstrap::Factory { progress: true }),
])));
