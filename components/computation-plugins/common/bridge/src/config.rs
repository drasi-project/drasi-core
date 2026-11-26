// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{num::NonZeroUsize, path::Component, sync::Arc, time::Duration};

use drasi_computation_contracts::{
    BinaryEnvelopeCodec, ComponentDescriptor, ComponentId, ComponentRole, GraphChangeCodec,
    PipeCapability, PipeRequirements, PortDescriptor, PortDirection, PortId, Schema,
    SchemaDescriptor, SinkCompletion, StreamId,
};
use drasi_core_types::interface::FailureMode;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{BridgeError, ErrorCode, Result, TlsResourceRecipe, TlsRole};

/// All bridge data is the standard graph schema. The delivery mode says whether
/// every frame must additionally carry a current complete-source-transaction
/// marker; the schema alone never selects atomic handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PortableDelivery {
    Events,
    CompleteTransactions,
}

impl PortableDelivery {
    pub fn schema(self) -> Arc<Schema> {
        GraphChangeCodec::schema()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Profile {
    Volatile,
    Durable,
}

/// Version one deliberately uses an identity mapping. The list is an explicit
/// authority declaration over graph element namespaces
/// (`ElementReference::namespace`), not an instruction to modify a business ID.
///
/// Source, query and transformer outputs are the same graph data, so one list
/// covers physical source namespaces and derived output dataset namespaces
/// (`drasi.output/<hash>`). An entry ending in `/` is a prefix that needs at
/// least one more character; any other entry is one exact namespace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Namespace {
    pub version: u32,
    pub source_prefixes: Vec<String>,
}

impl Namespace {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.version != 1 || self.source_prefixes.len() > 16 {
            return Err(BridgeError::invalid(
                "unsupported namespace version or entry count",
            ));
        }
        for entry in &self.source_prefixes {
            identifier(entry)?;
        }
        Ok(())
    }

    pub(crate) fn validate_namespace(&self, id: &str) -> Result<()> {
        identifier(id)?;
        if !self.source_prefixes.iter().any(|entry| {
            if entry.ends_with('/') {
                id.starts_with(entry.as_str()) && id.len() > entry.len()
            } else {
                id == entry
            }
        }) {
            return Err(BridgeError::invalid(
                "element namespace is outside the declared namespace",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
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
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
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

fn is_stall(policy: &drasi_computation_contracts::PermanentFailurePolicy) -> bool {
    *policy == drasi_computation_contracts::PermanentFailurePolicy::Stall
}

fn failure_scope() -> FailureMode {
    FailureMode::ProcessRestart
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BridgeConfig {
    pub link_id: String,
    pub sender_instance: String,
    pub receiver_instance: String,
    pub sender_endpoint: ComponentId,
    pub receiver_endpoint: ComponentId,
    /// This component's local output/input binding.
    pub stream: StreamId,
    pub peer_stream: StreamId,
    pub delivery: PortableDelivery,
    pub profile: Profile,
    #[serde(default = "failure_scope")]
    pub failure_scope: FailureMode,
    pub namespace: Namespace,
    pub transport: Transport,
    #[serde(default)]
    pub limits: Limits,
    /// What the graph does with an input the sender can never deliver; the
    /// same `onPermanentFailure` setting every sink has. Only senders publish
    /// it in their configuration schema.
    #[serde(default, skip_serializing_if = "is_stall")]
    pub on_permanent_failure: drasi_computation_contracts::PermanentFailurePolicy,
    /// This endpoint's mutual-TLS identity; required exactly for the `tls`
    /// transport. A local setting, not part of the link contract.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls: Option<TlsResourceRecipe>,
    /// Directory of a durable sender's mapping file; required exactly for a
    /// durable sink. A local setting, not part of the link contract.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mapping_directory: Option<String>,
}

impl BridgeConfig {
    pub fn validate(&self) -> Result<()> {
        for id in [
            &self.link_id,
            &self.sender_instance,
            &self.receiver_instance,
        ] {
            identifier(id)?;
        }
        self.namespace.validate()?;
        self.transport.validate()?;
        self.limits.validate()?;
        if self.namespace.source_prefixes.is_empty() {
            return Err(BridgeError::invalid(
                "the selected schema has no namespace authority",
            ));
        }
        if let Some(tls) = &self.tls {
            tls.validate()
                .map_err(|error| BridgeError::invalid(error.to_string()))?;
        }
        if self.transport.is_tls() != self.tls.is_some() {
            return Err(BridgeError::invalid(
                "the TLS transport requires exactly its TLS identity",
            ));
        }
        if let Some(directory) = &self.mapping_directory {
            local_path(directory)?;
        }
        Ok(())
    }

    /// Validates the configuration for one endpoint role: the TLS identity
    /// role must match it, and only a durable sender has a mapping directory.
    pub fn validate_role(&self, role: ComponentRole) -> Result<()> {
        self.validate()?;
        let source = match role {
            ComponentRole::Source => true,
            ComponentRole::Sink => false,
            _ => {
                return Err(BridgeError::invalid(
                    "a bridge endpoint must be a source or a sink",
                ))
            }
        };
        if let Some(tls) = &self.tls {
            if tls.role
                != if source {
                    TlsRole::Server
                } else {
                    TlsRole::Client
                }
            {
                return Err(BridgeError::invalid(
                    "bridge TLS client/server role does not match its endpoint",
                ));
            }
        }
        if source && self.mapping_directory.is_some() {
            return Err(BridgeError::invalid(
                "a bridge source has no sender mapping directory",
            ));
        }
        if !source && (self.profile == Profile::Durable) != self.mapping_directory.is_some() {
            return Err(BridgeError::invalid(
                "a durable sender requires its mapping directory; a volatile sender forbids it",
            ));
        }
        Ok(())
    }

    /// Both profiles complete an input only once the receiver has admitted
    /// it: into its bounded queue (volatile) or its admission journal
    /// (durable). A volatile receiver can still lose admitted work.
    pub fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }

    pub fn descriptor(&self, role: ComponentRole) -> anyhow::Result<ComponentDescriptor> {
        self.validate()?;
        let source = match role {
            ComponentRole::Source => true,
            ComponentRole::Sink => false,
            _ => anyhow::bail!("a bridge endpoint must be a source or a sink"),
        };
        Ok(ComponentDescriptor::try_new(
            if source {
                self.receiver_endpoint.clone()
            } else {
                self.sender_endpoint.clone()
            },
            vec![PortDescriptor::new(
                PortId::try_new(if source { "out" } else { "in" })?,
                if source {
                    PortDirection::Output
                } else {
                    PortDirection::Input
                },
                self.delivery.schema().descriptor().clone(),
                port_requirements(),
            )],
        )?)
    }

    pub fn contract(&self, source: bool) -> LinkContract {
        LinkContract {
            version: u32::from(crate::protocol::PROTOCOL_VERSION),
            envelope_version: BinaryEnvelopeCodec::VERSION,
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
            delivery: self.delivery,
            schema_descriptor: self.delivery.schema().descriptor().clone(),
            profile: self.profile,
            failure_scope: self.failure_scope,
            namespace: self.namespace.clone(),
            limits: self.limits.clone(),
        }
    }
}

/// The pipe requirements of either endpoint's port. Plugin factory ports are
/// fixed, so they do not vary by profile: a durable link is enforced by its
/// admission journal (receiver) and exactly-once input (sender).
pub fn port_requirements() -> PipeRequirements {
    PipeRequirements::new(vec![
        PipeCapability::FifoPerStream,
        PipeCapability::Backpressure,
    ])
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkContract {
    pub version: u32,
    pub envelope_version: u32,
    pub link_id: String,
    pub sender_instance: String,
    pub receiver_instance: String,
    pub sender_endpoint: ComponentId,
    pub receiver_endpoint: ComponentId,
    pub sender_stream: StreamId,
    pub receiver_stream: StreamId,
    pub delivery: PortableDelivery,
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

/// A bounded local path without parent traversal. Relative paths resolve
/// against the process working directory.
pub(crate) fn local_path(value: &str) -> Result<()> {
    let path = std::path::Path::new(value);
    if value.is_empty()
        || value.len() > 4096
        || value.chars().any(char::is_control)
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
    {
        return Err(BridgeError::invalid(
            "local paths must be bounded and cannot contain parent traversal",
        ));
    }
    Ok(())
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

/// The description of a bridge receiver (a source): a durable profile
/// publishes through its (required durable) Host admission channel under a
/// restart-stable producer identity; a volatile profile publishes under a new
/// volatile identity on every construction.
pub fn receiver_description(
    descriptor: &drasi_computation_contracts::ComponentDescriptor,
    profile: Profile,
) -> drasi_computation_contracts::ComponentDescription {
    use drasi_computation_contracts::{
        ComponentDescription, DependencyRequirement, DurabilityRequirement, OutputDescription,
        ProgressPersistence,
    };
    match profile {
        Profile::Durable => ComponentDescription::new(ProgressPersistence::Durable)
            .with_outputs(descriptor, OutputDescription::DURABLE)
            .with_dependency(
                "admission",
                DependencyRequirement {
                    durability: DurabilityRequirement::Durable,
                },
            ),
        Profile::Volatile => ComponentDescription::new(ProgressPersistence::Volatile)
            .with_outputs(descriptor, OutputDescription::VOLATILE),
    }
}

/// The description of a bridge sender (a sink). A durable sender keys its
/// persisted sender mapping on the producer's restart-stable identity and
/// sequence, so it requires exactly-once input from a durable producer; a
/// volatile or unidentified producer is refused when the edge is bound. The
/// mapping itself is the sender's own synced file. A volatile sender does not
/// key on producer identity.
pub fn sender_description(
    descriptor: &drasi_computation_contracts::ComponentDescriptor,
    config: &BridgeConfig,
) -> drasi_computation_contracts::ComponentDescription {
    use drasi_computation_contracts::{ComponentDescription, InputDelivery, ProgressPersistence};
    match config.profile {
        Profile::Durable => ComponentDescription::new(ProgressPersistence::Durable)
            .with_inputs(descriptor, InputDelivery::ExactlyOnce),
        Profile::Volatile => ComponentDescription::new(ProgressPersistence::Stateless)
            .with_inputs(descriptor, InputDelivery::Unkeyed),
    }
    .with_permanent_failure(config.on_permanent_failure)
}
