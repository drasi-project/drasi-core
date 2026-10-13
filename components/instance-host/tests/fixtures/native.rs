// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

#![allow(clippy::unwrap_used)]

use std::{collections::BTreeMap, io::Write, sync::Arc, time::Duration};

use anyhow::{ensure, Result};
use async_trait::async_trait;
use drasi_computation_plugin_sdk as sdk;
use drasi_lib::computation::v1::*;
use serde::Deserialize;

struct Factory;

impl sdk::Factory for Factory {
    fn metadata(&self) -> sdk::FactoryMetadata {
        sdk::FactoryMetadata {
            implementation: ImplementationIdentity::try_new("fixture/instance-host", "1").unwrap(),
            role: ComponentRole::Service,
            configuration_version: 1,
            configuration: sdk::ConfigSchema {
                fields: BTreeMap::from([
                    ("mode".into(), field(sdk::ConfigType::String)),
                    ("marker".into(), field(sdk::ConfigType::String)),
                    ("start_delay_ms".into(), field(sdk::ConfigType::Integer)),
                    ("stop_delay_ms".into(), field(sdk::ConfigType::Integer)),
                ]),
                allow_additional: false,
            },
            ports: Vec::new(),
            completion: None,
            capabilities: sdk::Capabilities::default(),
        }
    }

    fn create(
        &self,
        request: &sdk::CreateRequest,
        _: sdk::ControlSender,
    ) -> Result<sdk::CreatedComponent> {
        let configuration: Configuration = serde_json::from_value(request.configuration.clone())?;
        ensure!(
            matches!(
                configuration.mode.as_str(),
                "healthy" | "start-error" | "block" | "pending-stop"
            ),
            "unknown fixture mode"
        );
        Ok(sdk::Component::Service(Box::new(Service {
            descriptor: self.metadata().descriptor(request.id.clone())?,
            values: request.configuration.clone(),
            configuration,
        }))
        .into())
    }
}

fn field(value_type: sdk::ConfigType) -> sdk::ConfigField {
    sdk::ConfigField {
        value_type,
        required: false,
        secret: false,
    }
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Configuration {
    mode: String,
    marker: String,
    start_delay_ms: u64,
    stop_delay_ms: u64,
}

impl Default for Configuration {
    fn default() -> Self {
        Self {
            mode: "healthy".into(),
            marker: String::new(),
            start_delay_ms: 0,
            stop_delay_ms: 0,
        }
    }
}

struct Service {
    descriptor: ComponentDescriptor,
    values: serde_json::Value,
    configuration: Configuration,
}

#[async_trait]
impl ComputationComponent for Service {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn configuration(&self) -> Result<serde_json::Value> {
        Ok(self.values.clone())
    }
    async fn start(&mut self) -> Result<()> {
        if !self.configuration.marker.is_empty() {
            std::fs::write(&self.configuration.marker, std::process::id().to_string())?;
        }
        if self.configuration.start_delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(self.configuration.start_delay_ms)).await;
        }
        ensure!(
            self.configuration.mode != "start-error",
            "deliberate native initialization failure"
        );
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
        if self.configuration.mode == "pending-stop" {
            std::future::pending::<()>().await;
        }
        if self.configuration.stop_delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(self.configuration.stop_delay_ms)).await;
        }
        if !self.configuration.marker.is_empty() {
            std::fs::write(
                format!("{}.stopped", self.configuration.marker),
                std::process::id().to_string(),
            )?;
        }
        Ok(())
    }
}

#[async_trait]
impl ComputationService for Service {
    async fn run(&mut self) -> Result<()> {
        if self.configuration.mode == "block" {
            if !self.configuration.marker.is_empty() {
                std::fs::write(
                    format!("{}.blocked", self.configuration.marker),
                    std::process::id().to_string(),
                )?;
            }
            loop {
                std::hint::spin_loop();
            }
        }
        std::future::pending().await
    }
}

fn plugin() -> Result<sdk::PluginDefinition> {
    if let Ok(path) = std::env::var("DRASI_INSTANCE_FIXTURE_LOAD_LOG") {
        writeln!(
            std::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(path)?,
            "{}",
            std::process::id()
        )?;
    }
    sdk::PluginDefinition::new(
        "instance-host-fixture",
        env!("CARGO_PKG_VERSION"),
        vec![Arc::new(Factory)],
        Vec::new(),
    )
}

#[cfg(feature = "dynamic-plugin")]
sdk::export_computation_plugin!(plugin());
