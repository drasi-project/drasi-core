// Copyright 2026 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use super::{
    PipeCapability, PortDirection, PortId, RecordId, RecordImage, RecordValidationError,
    SchemaDescriptor, SinkCompletion,
};

/// Structured validation errors for v1 contracts, independent of legacy APIs.
#[derive(Debug, thiserror::Error)]
pub enum ContractError {
    #[error("invalid {kind} identifier {value:?}: expected nonempty text without whitespace or controls")]
    InvalidIdentifier { kind: &'static str, value: String },
    #[error("{kind} identity must contain nonempty bytes")]
    EmptyIdentity { kind: &'static str },
    #[error("schema version must be nonzero, got {value}")]
    InvalidSchemaVersion { value: u32 },
    #[error("schema definition must not be empty")]
    EmptySchemaDefinition,
    #[error("record {identity:?} failed schema validation: {source}")]
    RecordValidation {
        identity: RecordId,
        #[source]
        source: RecordValidationError,
    },
    #[error("operation ordinal {ordinal} occurs more than once")]
    DuplicateOrdinal { ordinal: u64 },
    #[error("operation ordinal {ordinal} follows larger ordinal {previous}")]
    OutOfOrderOrdinal { previous: u64, ordinal: u64 },
    #[error("schema mismatch: expected {expected:?}, got {actual:?}")]
    SchemaMismatch {
        expected: Box<SchemaDescriptor>,
        actual: Box<SchemaDescriptor>,
    },
    #[error("operation {ordinal} has mismatched record identities")]
    IdentityMismatch {
        ordinal: u64,
        expected: RecordId,
        actual: RecordId,
    },
    #[error("operation {ordinal} requires {expected}, got {actual:?}")]
    InvalidImage {
        ordinal: u64,
        expected: &'static str,
        actual: RecordImage,
    },
    #[error("port {port:?} occurs more than once")]
    DuplicatePort { port: PortId },
    #[error("port {port:?} must be {expected:?}, got {actual:?}")]
    WrongPortDirection {
        port: PortId,
        expected: PortDirection,
        actual: PortDirection,
    },
    #[error("pipe does not support required capability {capability:?}")]
    UnsupportedCapability { capability: PipeCapability },
    #[error("capability {capability:?} requires {requires:?}")]
    InvalidCapabilities {
        capability: PipeCapability,
        requires: PipeCapability,
    },
    #[error("sink completion {actual:?} cannot satisfy {required:?}")]
    InsufficientSinkCompletion {
        actual: SinkCompletion,
        required: PipeCapability,
    },
    #[error("processing context length overflow")]
    ContextOverflow,
    #[error("invalid port format declaration: {reason}")]
    InvalidPortFormats { reason: String },
    #[error("invalid port format override: {reason}")]
    InvalidPortFormatOverride { reason: String },
    #[error(
        "no {direction:?} port {port:?} is declared (declared {direction:?} ports: {known:?})"
    )]
    EndpointPortNotFound {
        port: PortId,
        direction: PortDirection,
        known: Vec<PortId>,
    },
    #[error("component declares no {direction:?} port")]
    NoEndpointPort { direction: PortDirection },
    #[error(
        "ambiguous {direction:?} port: name one explicitly or designate a primary (candidates: {candidates:?})"
    )]
    AmbiguousEndpointPort {
        direction: PortDirection,
        candidates: Vec<PortId>,
    },
    #[error("port {port:?} is not declared by this component (declared: {known:?})")]
    UnknownFormatPort { port: PortId, known: Vec<PortId> },
    #[error(
        "port {port:?} does not support format {requested:?}; supported formats: {supported:?}"
    )]
    UnsupportedPortFormat {
        port: PortId,
        requested: String,
        supported: Vec<String>,
    },
    #[error("port {port:?} is declared as {actual} but its configuration resolves to {expected}")]
    PortFormatMismatch {
        port: PortId,
        expected: String,
        actual: String,
    },
}

/// Result of validated construction or compatibility negotiation.
pub type Result<T> = std::result::Result<T, ContractError>;

/// Multiple operation failures, all retained in encounter order.
///
/// The standard source chain follows the first failure. Inspect [`Self::failures`]
/// to recover other causes or ownership-bearing cleanup errors.
#[derive(Debug)]
pub struct OperationFailures {
    message: String,
    failures: Vec<anyhow::Error>,
}

impl OperationFailures {
    /// Retains the individual causes of an operation and its required cleanup.
    pub fn new(message: impl Into<String>, failures: Vec<anyhow::Error>) -> Self {
        Self {
            message: message.into(),
            failures,
        }
    }

    pub fn failures(&self) -> &[anyhow::Error] {
        &self.failures
    }
}

impl std::fmt::Display for OperationFailures {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)?;
        for (index, error) in self.failures.iter().enumerate() {
            write!(f, "{}{error:#}", if index == 0 { ": " } else { "; " })?;
        }
        Ok(())
    }
}

impl std::error::Error for OperationFailures {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.failures.first().map(|error| error.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::OperationFailures;
    use std::{error::Error, io};

    #[test]
    fn operation_failures_preserve_all_typed_causes_and_display_order() {
        let failures = OperationFailures::new(
            "operation and cleanup failed",
            vec![
                io::Error::new(io::ErrorKind::InvalidData, "operation").into(),
                io::Error::new(io::ErrorKind::PermissionDenied, "cleanup").into(),
            ],
        );
        assert_eq!(
            failures.to_string(),
            "operation and cleanup failed: operation; cleanup"
        );
        assert_eq!(failures.failures().len(), 2);
        assert_eq!(
            failures
                .source()
                .and_then(|error| error.downcast_ref::<io::Error>())
                .expect("primary cause")
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(
            failures.failures()[1]
                .downcast_ref::<io::Error>()
                .expect("cleanup cause")
                .kind(),
            io::ErrorKind::PermissionDenied
        );
    }
}
