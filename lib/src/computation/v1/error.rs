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
}

/// Result of validated construction or compatibility negotiation.
pub type Result<T> = std::result::Result<T, ContractError>;
