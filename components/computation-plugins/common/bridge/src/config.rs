// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{collections::BTreeMap, num::NonZeroUsize, sync::Arc, time::Duration};

use drasi_core::interface::FailureMode;
use drasi_lib::computation::v1::{
    ComponentDescriptor, ComponentId, ComponentRole, ComponentSpecification, ConfigurationValue,
    GraphChangeCodec, ImplementationIdentity, PipeCapability, PipeRequirements, PortDescriptor,
    PortDirection, PortId, QueryChangeCodec, Schema, SchemaDescriptor, SinkCompletion,
    SourceTransactionCodec,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{BridgeError, ErrorCode, Result, SINK_FACTORY, SOURCE_FACTORY};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PortableSchema {
    GraphChange,
    QueryChange,
    SourceTransaction,
}

impl PortableSchema {
    pub fn schema(self) -> Arc<Schema> {
        match self {
            Self::GraphChange => GraphChangeCodec::schema(),
            Self::QueryChange => QueryChangeCodec::schema(),
            Self::SourceTransaction => SourceTransactionCodec::schema(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Profile {
    Volatile,
    Durable,
}

/// Version one deliberately uses an identity mapping. Prefixes are explicit
/// authority declarations, not instructions to modify a source's business IDs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Namespace {
    pub version: u32,
    pub source_prefixes: Vec<String>,
    pub query_prefixes: Vec<String>,
}

impl Namespace {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.version != 1 || self.source_prefixes.len() > 16 || self.query_prefixes.len() > 16 {
            return Err(BridgeError::invalid(
                "unsupported namespace version or prefix count",
            ));
        }
        for prefix in self.source_prefixes.iter().chain(&self.query_prefixes) {
            identifier(prefix)?;
            if !prefix.ends_with('/') {
                return Err(BridgeError::invalid("namespace prefixes must end in '/'"));
            }
        }
        Ok(())
    }

    pub(crate) fn source(&self, id: &str) -> Result<()> {
        self.allowed(id, &self.source_prefixes, "source")
    }

    pub(crate) fn query(&self, id: &str) -> Result<()> {
        self.allowed(id, &self.query_prefixes, "query")
    }

    fn allowed(&self, id: &str, prefixes: &[String], kind: &str) -> Result<()> {
        identifier(id)?;
        if !prefixes
            .iter()
            .any(|prefix| id.starts_with(prefix) && id.len() > prefix.len())
        {
            return Err(BridgeError::invalid(format!(
                "{kind} identity is outside the declared namespace"
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Transport {
    Unix {
        data_path: String,
        control_path: String,
    },
    NamedPipe {
        data_pipe: String,
        control_pipe: String,
    },
    /// Mutual TLS plus a configured peer certificate pin; no plaintext variant.
    Tls {
        data_address: String,
        control_address: String,
        server_name: String,
    },
}

impl Transport {
    pub fn is_tls(&self) -> bool {
        matches!(self, Self::Tls { .. })
    }

    fn validate(&self) -> Result<()> {
        let (data, control) = match self {
            Self::Unix {
                data_path,
                control_path,
            } => {
                if !cfg!(unix) {
                    return Err(BridgeError::invalid("Unix-domain sockets are unavailable"));
                }
                // sockaddr_un is 104 bytes on macOS. Do not truncate addresses.
                if data_path.len() > 100 || control_path.len() > 100 {
                    return Err(BridgeError::invalid("Unix socket paths exceed 100 bytes"));
                }
                (data_path, control_path)
            }
            Self::NamedPipe {
                data_pipe,
                control_pipe,
            } => {
                if !cfg!(windows) {
                    return Err(BridgeError::invalid("Windows named pipes are unavailable"));
                }
                if !data_pipe.starts_with(r"\\.\pipe\") || !control_pipe.starts_with(r"\\.\pipe\") {
                    return Err(BridgeError::invalid(
                        "only local Windows named pipes are supported",
                    ));
                }
                (data_pipe, control_pipe)
            }
            Self::Tls {
                data_address,
                control_address,
                server_name,
            } => {
                identifier(server_name)?;
                for address in [data_address, control_address] {
                    let address = address.parse::<std::net::SocketAddr>().map_err(|_| {
                        BridgeError::invalid("TLS addresses must be IP:port socket addresses")
                    })?;
                    if address.port() == 0 {
                        return Err(BridgeError::invalid("TLS requires explicit nonzero ports"));
                    }
                }
                (data_address, control_address)
            }
        };
        for address in [data, control] {
            if address.is_empty() || address.len() > 256 || address.contains('\0') {
                return Err(BridgeError::invalid("invalid transport address"));
            }
        }
        if data == control {
            return Err(BridgeError::invalid(
                "data and control addresses must be distinct",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    pub max_envelope_bytes: usize,
    pub max_operations: usize,
    pub max_values: usize,
    pub max_depth: usize,
    pub max_lineage: usize,
    pub max_annotations: usize,
    pub queue_capacity: usize,
    pub timeout_ms: u64,
    pub retry_attempts: usize,
    pub retry_delay_ms: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_envelope_bytes: 1024 * 1024,
            max_operations: 1024,
            max_values: 65_536,
            max_depth: 32,
            max_lineage: 32,
            max_annotations: 64,
            queue_capacity: 16,
            timeout_ms: 5_000,
            retry_attempts: 3,
            retry_delay_ms: 50,
        }
    }
}

impl Limits {
    pub fn validate(&self) -> Result<()> {
        if !(1024..=4 * 1024 * 1024).contains(&self.max_envelope_bytes)
            || !(1..=4096).contains(&self.max_operations)
            || !(1..=131_072).contains(&self.max_values)
            || !(1..=32).contains(&self.max_depth)
            || !(1..=64).contains(&self.max_lineage)
            || !(1..=128).contains(&self.max_annotations)
            || !(1..=128).contains(&self.queue_capacity)
            || self.queue_capacity * self.max_envelope_bytes > 64 * 1024 * 1024
            || !(1..=60_000).contains(&self.timeout_ms)
            || !(1..=16).contains(&self.retry_attempts)
            || self.retry_delay_ms > 5_000
        {
            return Err(BridgeError::invalid(
                "bridge limits exceed supported bounds",
            ));
        }
        Ok(())
    }

    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.timeout_ms)
    }

    pub(crate) fn envelope_limit(&self) -> NonZeroUsize {
        NonZeroUsize::new(self.max_envelope_bytes).expect("validated bridge byte limit")
    }
}

fn failure_scope() -> FailureMode {
    FailureMode::ProcessRestart
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BridgeConfig {
    pub link_id: String,
    pub sender_instance: String,
    pub receiver_instance: String,
    pub sender_endpoint: String,
    pub receiver_endpoint: String,
    /// This component's local output/input binding.
    pub stream: String,
    pub peer_stream: String,
    pub schema: PortableSchema,
    pub profile: Profile,
    #[serde(default = "failure_scope")]
    pub failure_scope: FailureMode,
    pub namespace: Namespace,
    pub transport: Transport,
    #[serde(default)]
    pub limits: Limits,
}

impl BridgeConfig {
    pub fn validate(&self) -> Result<()> {
        for id in [
            &self.link_id,
            &self.sender_instance,
            &self.receiver_instance,
            &self.sender_endpoint,
            &self.receiver_endpoint,
            &self.stream,
            &self.peer_stream,
        ] {
            identifier(id)?;
        }
        self.namespace.validate()?;
        self.transport.validate()?;
        self.limits.validate()?;
        if self.schema == PortableSchema::QueryChange && self.namespace.query_prefixes.is_empty()
            || self.schema != PortableSchema::QueryChange
                && self.namespace.source_prefixes.is_empty()
        {
            return Err(BridgeError::invalid(
                "the selected schema has no namespace authority",
            ));
        }
        Ok(())
    }

    pub fn completion(&self) -> SinkCompletion {
        match self.profile {
            Profile::Volatile => SinkCompletion::Accepted,
            Profile::Durable => SinkCompletion::Handled,
        }
    }

    pub fn descriptor(&self, role: ComponentRole) -> anyhow::Result<ComponentDescriptor> {
        self.validate()?;
        let source = match role {
            ComponentRole::Source => true,
            ComponentRole::Sink => false,
            _ => anyhow::bail!("a bridge endpoint must be a source or a sink"),
        };
        use PipeCapability::*;
        let mut requirements = vec![FifoPerStream, Backpressure];
        if self.profile == Profile::Durable {
            requirements.extend([DurableAcceptance, ExplicitAcknowledgement, Replay]);
        }
        Ok(ComponentDescriptor::try_new(
            ComponentId::try_new(if source {
                self.receiver_endpoint.as_str()
            } else {
                self.sender_endpoint.as_str()
            })?,
            vec![PortDescriptor::new(
                PortId::try_new(if source { "out" } else { "in" })?,
                if source {
                    PortDirection::Output
                } else {
                    PortDirection::Input
                },
                self.schema.schema().descriptor().clone(),
                PipeRequirements::new(requirements),
            )],
        )?)
    }

    /// Builds a normal graph specification. Add the actual resource dependencies
    /// before submitting it; no socket or storage is opened here.
    pub fn specification(&self, role: ComponentRole) -> anyhow::Result<ComponentSpecification> {
        let value = serde_json::to_value(self)?;
        Ok(ComponentSpecification {
            descriptor: self.descriptor(role)?,
            role,
            completion: (role == ComponentRole::Sink).then(|| self.completion()),
            implementation: ImplementationIdentity::try_new(
                if role == ComponentRole::Source {
                    SOURCE_FACTORY
                } else {
                    SINK_FACTORY
                },
                "1",
            )?,
            configuration_version: 1,
            configuration: value
                .as_object()
                .expect("configuration object")
                .iter()
                .map(|(key, value)| {
                    (
                        Arc::from(key.as_str()),
                        ConfigurationValue::Literal(value.clone()),
                    )
                })
                .collect(),
            dependencies: BTreeMap::new(),
        })
    }

    pub fn contract(&self, source: bool) -> LinkContract {
        LinkContract {
            version: 1,
            envelope_version: 2,
            link_id: self.link_id.clone(),
            sender_instance: self.sender_instance.clone(),
            receiver_instance: self.receiver_instance.clone(),
            sender_endpoint: self.sender_endpoint.clone(),
            receiver_endpoint: self.receiver_endpoint.clone(),
            sender_stream: if source {
                &self.peer_stream
            } else {
                &self.stream
            }
            .clone(),
            receiver_stream: if source {
                &self.stream
            } else {
                &self.peer_stream
            }
            .clone(),
            schema: self.schema,
            schema_descriptor: self.schema.schema().descriptor().clone(),
            profile: self.profile,
            failure_scope: self.failure_scope,
            namespace: self.namespace.clone(),
            limits: self.limits.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkContract {
    pub version: u32,
    pub envelope_version: u32,
    pub link_id: String,
    pub sender_instance: String,
    pub receiver_instance: String,
    pub sender_endpoint: String,
    pub receiver_endpoint: String,
    pub sender_stream: String,
    pub receiver_stream: String,
    pub schema: PortableSchema,
    pub schema_descriptor: SchemaDescriptor,
    pub profile: Profile,
    pub failure_scope: FailureMode,
    pub namespace: Namespace,
    pub limits: Limits,
}

impl LinkContract {
    pub fn fingerprint(&self) -> [u8; 32] {
        Sha256::digest(serde_json::to_vec(self).expect("serializable bridge contract")).into()
    }

    pub(crate) fn check(&self, other: &Self) -> Result<()> {
        if self != other {
            return Err(BridgeError::new(
                ErrorCode::Incompatible,
                "bridge protocol, identities, schema, namespace or delivery contract differs",
            ));
        }
        Ok(())
    }
}

pub fn identifier(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 256
        || value.chars().any(char::is_control)
        || value.trim() != value
    {
        return Err(BridgeError::invalid(
            "identifiers require 1..=256 non-control UTF-8 bytes",
        ));
    }
    Ok(())
}
