// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::Context;
use async_trait::async_trait;
use drasi_computation_contracts::*;
use drasi_computation_plugin_sdk::{self as sdk, Factory as _};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

static COUNTERS: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];
pub const CONSTRUCTED: u32 = 0;
pub const LITERAL: u32 = 1;
pub const AUTHENTICATED: u32 = 2;
pub const STOPPED: u32 = 3;
pub const IN_FLIGHT: u32 = 4;
pub const COMPLETED: u32 = 5;
pub const LAST_SECRET: u32 = 6;
pub const LAST_KIND: u32 = 7;

#[no_mangle]
pub extern "C" fn drasi_activation_fixture_probe(counter: u32) -> u64 {
    COUNTERS
        .get(counter as usize)
        .map_or(0, |value| value.load(Ordering::SeqCst))
}

#[no_mangle]
pub extern "C" fn drasi_activation_fixture_reset() {
    for counter in &COUNTERS {
        counter.store(0, Ordering::SeqCst);
    }
}

fn increment(counter: u32) {
    COUNTERS[counter as usize].fetch_add(1, Ordering::SeqCst);
}

pub struct Factory;
impl sdk::Factory for Factory {
    fn metadata(&self) -> sdk::FactoryMetadata {
        sdk::FactoryMetadata {
            implementation: ImplementationIdentity::try_new("fixture/activation-credentials", "1")
                .expect("valid activation fixture implementation"),
            role: ComponentRole::Sink,
            configuration_version: 1,
            configuration: sdk::ConfigSchema {
                fields: [
                    ("hostname", sdk::ConfigType::String, true, false),
                    ("port", sdk::ConfigType::Integer, true, false),
                    ("user", sdk::ConfigType::String, false, false),
                    ("password", sdk::ConfigType::String, false, true),
                    ("mode", sdk::ConfigType::String, false, false),
                ]
                .into_iter()
                .map(|(name, value_type, required, secret)| {
                    (
                        name.into(),
                        sdk::ConfigField {
                            value_type,
                            required,
                            secret,
                        },
                    )
                })
                .collect(),
                allow_additional: false,
            },
            ports: vec![PortDescriptor::new(
                PortId::try_new("in").expect("valid activation fixture port"),
                PortDirection::Input,
                GraphChangeCodec::schema().descriptor().clone(),
                PipeRequirements::default(),
            )],
            completion: Some(SinkCompletion::Handled),
            capabilities: sdk::Capabilities::default(),
        }
    }

    fn supports_activation_credentials(&self) -> bool {
        true
    }

    fn activation_credential_context(
        &self,
        request: &sdk::CreateRequest,
    ) -> anyhow::Result<sdk::ActivationCredentialContext> {
        anyhow::ensure!(
            request.activation_credentials_bound,
            "context must inspect the actual bound request"
        );
        Ok(sdk::ActivationCredentialContext {
            hostname: request.configuration["hostname"]
                .as_str()
                .context("activation fixture requires a hostname")?
                .into(),
            port: request.configuration["port"]
                .as_u64()
                .context("activation fixture requires an unsigned port")?
                .try_into()?,
        })
    }

    fn create(
        &self,
        request: &sdk::CreateRequest,
        _: sdk::ControlSender,
    ) -> anyhow::Result<sdk::CreatedComponent> {
        anyhow::ensure!(
            request.activation_credentials_bound
                || request.configuration["user"]
                    .as_str()
                    .is_some_and(|user| !user.is_empty()),
            "literal authentication requires a user"
        );
        increment(CONSTRUCTED);
        Ok(sdk::Component::Sink(Box::new(Sink {
            descriptor: self.metadata().descriptor(request.id.clone())?,
            configuration: request.configuration.clone(),
            bound: request.activation_credentials_bound,
        }))
        .into())
    }
}

struct Sink {
    descriptor: ComponentDescriptor,
    configuration: Value,
    bound: bool,
}

struct InFlight;
impl InFlight {
    fn new() -> Self {
        increment(IN_FLIGHT);
        Self
    }
}
impl Drop for InFlight {
    fn drop(&mut self) {
        COUNTERS[IN_FLIGHT as usize].fetch_sub(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl sdk::NativeComponent for Sink {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn configuration(&self) -> anyhow::Result<Value> {
        Ok(self.configuration.clone())
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        increment(LITERAL);
        anyhow::ensure!(!self.bound, "bound component must not use literal startup");
        Ok(())
    }
    async fn start_with_credentials(
        &mut self,
        credentials: sdk::ActivationCredentials,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.bound,
            "credentials cannot be supplied to a literal component"
        );
        increment(AUTHENTICATED);
        let _in_flight = InFlight::new();
        COUNTERS[LAST_KIND as usize].store(
            if matches!(&credentials, sdk::ActivationCredentials::Token { .. }) {
                2
            } else {
                1
            },
            Ordering::SeqCst,
        );
        let (username, secret) = credentials.into_auth_pair();
        anyhow::ensure!(username == "provider-user", "unexpected provider username");
        COUNTERS[LAST_SECRET as usize].store(
            match secret.as_str() {
                "first-secret" => 1,
                "second-secret" => 2,
                _ => 3,
            },
            Ordering::SeqCst,
        );
        match self.configuration["mode"].as_str() {
            Some("pending") => std::future::pending::<()>().await,
            Some("panic") => panic!("fixture startup panic"),
            Some("failure") => anyhow::bail!("must-not-leak-{secret}"),
            _ => {}
        }
        increment(COMPLETED);
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        increment(STOPPED);
        Ok(())
    }
}

#[async_trait]
impl sdk::NativeSink for Sink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, _: InputEnvelope) -> anyhow::Result<()> {
        Ok(())
    }
}

pub fn definition() -> anyhow::Result<sdk::PluginDefinition> {
    sdk::PluginDefinition::new(
        "fixture/activation-library",
        "1",
        vec![Arc::new(Factory)],
        vec![GraphChangeCodec::schema()],
    )
}

pub fn configuration() -> Value {
    json!({"hostname": "exact.database.test", "port": 3307, "user": "", "password": ""})
}

#[cfg(not(test))]
sdk::export_computation_plugin!(definition());
