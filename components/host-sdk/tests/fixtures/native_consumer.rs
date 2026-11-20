// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Test-only completion handlers; not production plugin registration.

use async_trait::async_trait;
use drasi_computation_plugin_sdk::{self as sdk, Factory as _};
use drasi_core::models::ElementValue;
use drasi_lib::computation::v1::*;
use std::sync::Mutex;

pub struct Factory(pub sdk::ConsumerMode);

impl sdk::Factory for Factory {
    /// Every field described (fixture).
    fn configuration_details(&self) -> sdk::ConfigDetails {
        sdk::ConfigDetails {
            fields: sdk::Factory::metadata(self)
                .configuration
                .fields
                .into_keys()
                .map(|name| {
                    let details = sdk::configuration::ConfigFieldDetails {
                        description: format!("Fixture field {name}."),
                        ..Default::default()
                    };
                    (name, details)
                })
                .collect(),
        }
    }
    /// Inputs deliver keyed on producer identity with the configured
    /// `deliveryGuarantee` (a transactional consumer only exactly once).
    fn describe(
        &self,
        request: &sdk::wire::DescribeRequest,
    ) -> anyhow::Result<sdk::ComponentDescription> {
        let guarantee: sdk::DeliveryGuarantee = match self.0 {
            sdk::ConsumerMode::Transactional => sdk::DeliveryGuarantee::ExactlyOnce,
            sdk::ConsumerMode::External => {
                serde_json::from_value(request.request.configuration["deliveryGuarantee"].clone())
                    .unwrap_or_default()
            }
        };
        let policy =
            serde_json::from_value(request.request.configuration["onPermanentFailure"].clone())
                .unwrap_or_default();
        Ok(request.consumer(guarantee, policy))
    }
    fn metadata(&self) -> sdk::FactoryMetadata {
        sdk::FactoryMetadata {
            implementation: ImplementationIdentity::try_new(
                match self.0 {
                    sdk::ConsumerMode::External => "fixture/consumer-external",
                    sdk::ConsumerMode::Transactional => "fixture/consumer-transactional",
                },
                "1",
            )
            .expect("constant fixture identity"),
            role: ComponentRole::Sink,
            configuration_version: 1,
            configuration: sdk::ConfigSchema {
                fields: [
                    "path",
                    "mode",
                    "signal",
                    "deliveryGuarantee",
                    "onPermanentFailure",
                ]
                .into_iter()
                .map(|name| {
                    (
                        name.into(),
                        sdk::ConfigField {
                            constraint: None,
                            value_type: sdk::ConfigType::String,
                            required: false,
                            secret: false,
                        },
                    )
                })
                .collect(),
                allow_additional: false,
            },
            ports: vec![PortDescriptor::new(
                PortId::try_new("in").expect("constant input port"),
                PortDirection::Input,
                GraphChangeCodec::schema().descriptor().clone(),
                PipeRequirements::default(),
            )],
            completion: Some(SinkCompletion::Handled),
            capabilities: sdk::Capabilities {
                services: sdk::HostServices {
                    consumer: Some(self.0),
                    ..Default::default()
                },
                ..Default::default()
            },
        }
    }
    fn create(
        &self,
        request: &sdk::CreateRequest,
        _: sdk::ControlSender,
        _services: sdk::ServiceBindings,
    ) -> anyhow::Result<sdk::CreatedComponent> {
        let handler = Box::new(Handler {
            descriptor: self.metadata().descriptor(request.id.clone())?,
            configuration: request.configuration.clone(),
            connection: Mutex::new(None),
            attempts: 0,
        });
        Ok(match self.0 {
            sdk::ConsumerMode::External => sdk::Component::Consumer(handler),
            sdk::ConsumerMode::Transactional => sdk::Component::TransactionalConsumer(handler),
        }
        .into())
    }
}

struct Handler {
    descriptor: ComponentDescriptor,
    configuration: serde_json::Value,
    connection: Mutex<Option<rusqlite::Connection>>,
    attempts: usize,
}
impl Handler {
    fn mode(&self) -> &str {
        self.configuration["mode"].as_str().unwrap_or("normal")
    }
    async fn injected_failure(&self, operation: usize) -> anyhow::Result<()> {
        match self.mode() {
            "fail-second-once" if operation == 1 => {
                let path = self.configuration["signal"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("one-shot fault requires a signal file"))?;
                match std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(path)
                {
                    // Permanent, so the graph stalls the consumer with the
                    // operation prefix committed instead of retrying in
                    // place; only a later activation delivers the input.
                    Ok(_) => Err(drasi_computation_contracts::DeliveryFailure::permanent(
                        anyhow::anyhow!("fixture one-shot second operation failure"),
                    )
                    .into()),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
                    Err(error) => Err(error.into()),
                }
            }
            "fail-second" if operation == 1 => {
                if let Some(path) = self.configuration["signal"].as_str() {
                    std::fs::write(path, b"failed")?;
                }
                // The destination rejects this operation every time.
                Err(
                    drasi_computation_contracts::DeliveryFailure::permanent(anyhow::anyhow!(
                        "fixture second operation failed"
                    ))
                    .into(),
                )
            }
            // The destination rejects this operation and the consumer
            // records the skip with its own progress.
            "skip-second" if operation == 1 => Err(
                drasi_computation_contracts::DeliveryFailure::skipped(anyhow::anyhow!(
                    "fixture second operation skipped"
                ))
                .into(),
            ),
            "pending" => {
                if let Some(path) = self.configuration["signal"].as_str() {
                    std::fs::write(path, b"entered")?;
                }
                std::future::pending().await
            }
            "panic" => panic!("fixture consumer panic"),
            _ => Ok(()),
        }
    }
}
#[async_trait]
impl sdk::NativeComponent for Handler {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn configuration(&self) -> anyhow::Result<serde_json::Value> {
        Ok(self.configuration.clone())
    }
    async fn start(
        &mut self,
        _ctx: &mut drasi_computation_plugin_sdk::lifecycle::StartContext,
    ) -> drasi_computation_plugin_sdk::lifecycle::LifecycleResult {
        let result: anyhow::Result<()> = async {
            if let Some(path) = self.configuration["path"].as_str() {
                let connection = rusqlite::Connection::open(path)?;
                connection.execute_batch(
                    "CREATE TABLE IF NOT EXISTS effects(id TEXT PRIMARY KEY, operation INTEGER);
                 CREATE TABLE IF NOT EXISTS attempts(id TEXT, operation INTEGER);",
                )?;
                *self
                    .connection
                    .get_mut()
                    .map_err(|_| anyhow::anyhow!("fixture poisoned"))? = Some(connection);
            }
            Ok(())
        }
        .await;
        Ok(result?)
    }
    async fn stop(
        &mut self,
        _ctx: &drasi_computation_plugin_sdk::lifecycle::StopContext,
    ) -> drasi_computation_plugin_sdk::lifecycle::LifecycleResult {
        let result: anyhow::Result<()> = async {
            *self
                .connection
                .get_mut()
                .map_err(|_| anyhow::anyhow!("fixture poisoned"))? = None;
            Ok(())
        }
        .await;
        Ok(result?)
    }
    async fn initialize(
        &mut self,
        _: &drasi_computation_plugin_sdk::lifecycle::InitializeContext,
    ) -> drasi_computation_plugin_sdk::lifecycle::LifecycleResult {
        Ok(())
    }
    async fn pause(
        &mut self,
        _: &drasi_computation_plugin_sdk::lifecycle::PauseContext,
    ) -> drasi_computation_plugin_sdk::lifecycle::LifecycleResult {
        Ok(())
    }
    async fn resume(
        &mut self,
        _: &drasi_computation_plugin_sdk::lifecycle::ResumeContext,
    ) -> drasi_computation_plugin_sdk::lifecycle::LifecycleResult {
        Ok(())
    }
    async fn dispose(
        &mut self,
        _: &drasi_computation_plugin_sdk::lifecycle::DisposeContext,
    ) -> drasi_computation_plugin_sdk::lifecycle::LifecycleResult {
        Ok(())
    }
}
#[derive(Debug)]
struct Retry;
impl std::fmt::Display for Retry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("fixture retry")
    }
}
impl std::error::Error for Retry {}

#[async_trait]
impl DeliveryHandler for Handler {
    fn delivery_guarantee(&self) -> drasi_computation_contracts::DeliveryGuarantee {
        serde_json::from_value(self.configuration["deliveryGuarantee"].clone()).unwrap_or_default()
    }
    async fn handle(&mut self, item: DeliveryItem<'_>) -> anyhow::Result<()> {
        self.injected_failure(item.position.operation()).await?;
        let connection = self
            .connection
            .get_mut()
            .map_err(|_| anyhow::anyhow!("fixture poisoned"))?
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("fixture destination not open"))?;
        let transaction = connection.transaction()?;
        transaction.execute(
            "INSERT INTO attempts VALUES (?1, ?2)",
            rusqlite::params![item.id.as_str(), item.position.operation()],
        )?;
        transaction.execute(
            "INSERT OR IGNORE INTO effects VALUES (?1, ?2)",
            rusqlite::params![item.id.as_str(), item.position.operation()],
        )?;
        transaction.commit()?;
        self.attempts += 1;
        if self.mode() == "retry-once" && self.attempts == 1 {
            return Err(drasi_computation_contracts::DeliveryFailure::transient(Retry).into());
        }
        Ok(())
    }
}
#[async_trait]
impl sdk::NativeTransactionalConsumer for Handler {
    async fn handle(
        &self,
        item: DeliveryItem<'_>,
        context: &sdk::NativeTransactionContext<'_>,
    ) -> anyhow::Result<()> {
        let count = match context.get("count").await? {
            None => 0,
            Some(ElementValue::Integer(count)) => count,
            _ => anyhow::bail!("invalid fixture count"),
        };
        context
            .put("count", ElementValue::Integer(count + 1))
            .await?;
        if self.mode() == "ignore-state-error" {
            let _ = context.put("", ElementValue::Integer(0)).await;
        }
        self.injected_failure(item.position.operation()).await
    }
}
