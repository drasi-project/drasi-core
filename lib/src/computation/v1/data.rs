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

use std::{collections::HashSet, fmt, sync::Arc};

use bytes::Bytes;

use super::{ContractError, Result};
use crate::computation::internal::change::computation_bridge;

pub(super) fn validate_identifier(kind: &'static str, value: &str) -> Result<()> {
    if value.is_empty() || value.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(ContractError::InvalidIdentifier {
            kind,
            value: value.to_owned(),
        });
    }
    Ok(())
}

macro_rules! name_type {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(Arc<str>);

        impl $name {
            /// Reject empty identifiers, whitespace, and control characters.
            pub fn try_new(value: impl Into<Arc<str>>) -> Result<Self> {
                let value = value.into();
                validate_identifier(stringify!($name), &value)?;
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

name_type!(
    SchemaId,
    "Schema identity, scoped by its provider's naming convention."
);
name_type!(ComponentId, "Component identity within a graph.");
name_type!(PortId, "Port identity within a component.");
name_type!(ResourceId, "Resource identity within a computation graph.");
name_type!(
    StreamId,
    "Logical producer stream identity, unique to a producer/output port."
);

macro_rules! identity_type {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Debug, Clone, PartialEq, Eq, Hash)]
        pub struct $name {
            namespace: Arc<str>,
            value: Bytes,
        }

        impl $name {
            /// Preserve opaque identity bytes exactly; empty bytes are invalid.
            /// The caller owns uniqueness and stability within the namespace.
            pub fn try_new(namespace: impl Into<Arc<str>>, value: Bytes) -> Result<Self> {
                let namespace = namespace.into();
                validate_identifier("identity namespace", &namespace)?;
                if value.is_empty() {
                    return Err(ContractError::EmptyIdentity {
                        kind: stringify!($name),
                    });
                }
                Ok(Self { namespace, value })
            }

            pub fn namespace(&self) -> &str {
                &self.namespace
            }

            pub fn value(&self) -> &Bytes {
                &self.value
            }
        }
    };
}

identity_type!(
    RecordId,
    "Stable record identity; not a content fingerprint."
);
identity_type!(
    ChangeSetId,
    "Stable change-set identity supplied by its producer."
);
identity_type!(
    EnvelopeId,
    "Logical event identity, unchanged by branch-local context appends."
);

/// Positive schema version. No implicit upgrade or compatibility is assumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SchemaVersion(u32);

impl SchemaVersion {
    pub fn try_new(value: u32) -> Result<Self> {
        if value == 0 {
            return Err(ContractError::InvalidSchemaVersion { value });
        }
        Ok(Self(value))
    }

    pub const fn value(self) -> u32 {
        self.0
    }
}

/// Advisory noncryptographic fingerprint. Collisions are possible; never use
/// this instead of full descriptor equality or as a security/integrity guarantee.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SchemaFingerprint(u64);

impl SchemaFingerprint {
    pub const fn value(self) -> u64 {
        self.0
    }
}

/// Immutable schema identity and exact encoding/definition contract.
/// Definition bytes must be canonical according to the schema provider.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SchemaDescriptor {
    id: SchemaId,
    version: SchemaVersion,
    encoding: Arc<str>,
    definition: Bytes,
    fingerprint: SchemaFingerprint,
}

impl SchemaDescriptor {
    pub fn try_new(
        id: SchemaId,
        version: SchemaVersion,
        encoding: impl Into<Arc<str>>,
        definition: Bytes,
    ) -> Result<Self> {
        let encoding = encoding.into();
        validate_identifier("schema encoding", &encoding)?;
        if definition.is_empty() {
            return Err(ContractError::EmptySchemaDefinition);
        }
        let fingerprint = SchemaFingerprint(computation_bridge::schema_fingerprint(
            id.as_str(),
            version.value(),
            &encoding,
            &definition,
        ));
        Ok(Self {
            id,
            version,
            encoding,
            definition,
            fingerprint,
        })
    }

    pub fn id(&self) -> &SchemaId {
        &self.id
    }

    pub const fn version(&self) -> SchemaVersion {
        self.version
    }

    pub fn encoding(&self) -> &str {
        &self.encoding
    }

    pub fn definition(&self) -> &Bytes {
        &self.definition
    }

    pub const fn fingerprint(&self) -> SchemaFingerprint {
        self.fingerprint
    }
}

/// Completeness/meaning of a schema-defined record image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordImage {
    /// Complete snapshot of the record.
    Full,
    /// Update delta; omitted fields are unchanged, per the schema's patch rules.
    Patch,
    /// Known before-image projection, not a full snapshot or an update delta.
    Partial,
}

/// Domain-specific validator error, retained as the source of ContractError.
#[derive(Debug, thiserror::Error)]
#[error("{code}: {message}")]
pub struct RecordValidationError {
    code: String,
    message: String,
}

impl RecordValidationError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }

    pub fn code(&self) -> &str {
        &self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

/// Executable schema validation. Implementors must verify encoding, image-specific
/// structure, domain constraints, and embedded record identity where applicable.
/// Validation must be deterministic for the same descriptor/identity/image/bytes.
/// Different providers of the same descriptor must implement identical semantics.
pub trait RecordValidator: Send + Sync {
    /// Validate schema-specific identity namespace/encoding, including references
    /// without an image (identity-only deletes). No permissive default is supplied.
    fn validate_identity(
        &self,
        schema: &SchemaDescriptor,
        identity: &RecordId,
    ) -> std::result::Result<(), RecordValidationError>;

    fn validate(
        &self,
        schema: &SchemaDescriptor,
        identity: &RecordId,
        image: RecordImage,
        payload: &[u8],
    ) -> std::result::Result<(), RecordValidationError>;
}

/// Schema-validated record key, including when no before-image is available.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordReference {
    schema: SchemaDescriptor,
    identity: RecordId,
}

impl RecordReference {
    pub fn try_new(schema: &Schema, identity: RecordId) -> Result<Self> {
        schema
            .validator
            .validate_identity(&schema.descriptor, &identity)
            .map_err(|source| ContractError::RecordValidation {
                identity: identity.clone(),
                source,
            })?;
        Ok(Self {
            schema: schema.descriptor.clone(),
            identity,
        })
    }

    pub fn schema(&self) -> &SchemaDescriptor {
        &self.schema
    }

    pub fn identity(&self) -> &RecordId {
        &self.identity
    }
}

/// A descriptor paired with its mandatory in-process validator.
/// It is not a serializable schema registry entry.
pub struct Schema {
    descriptor: SchemaDescriptor,
    validator: Arc<dyn RecordValidator>,
}

impl Schema {
    pub fn new(descriptor: SchemaDescriptor, validator: Arc<dyn RecordValidator>) -> Self {
        Self {
            descriptor,
            validator,
        }
    }

    pub fn descriptor(&self) -> &SchemaDescriptor {
        &self.descriptor
    }
}

/// A validated immutable image. Cloning shares payload and descriptor allocations.
/// Deserialization cannot bypass identity/image validation.
///
/// ```compile_fail
/// use drasi_lib::computation::v1::Record;
/// let _: Record = serde_json::from_str("{}").unwrap();
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    reference: RecordReference,
    image: RecordImage,
    payload: Bytes,
}

impl Record {
    pub fn try_new(
        schema: &Schema,
        identity: RecordId,
        image: RecordImage,
        payload: Bytes,
    ) -> Result<Self> {
        let reference = RecordReference::try_new(schema, identity)?;
        schema
            .validator
            .validate(&schema.descriptor, reference.identity(), image, &payload)
            .map_err(|source| ContractError::RecordValidation {
                identity: reference.identity.clone(),
                source,
            })?;
        Ok(Self {
            reference,
            image,
            payload,
        })
    }

    pub fn schema(&self) -> &SchemaDescriptor {
        self.reference.schema()
    }

    pub fn identity(&self) -> &RecordId {
        self.reference.identity()
    }

    /// Reuse this record's already validated key without decoding its image again.
    pub fn reference(&self) -> &RecordReference {
        &self.reference
    }

    pub const fn image(&self) -> RecordImage {
        self.image
    }

    pub fn payload(&self) -> &Bytes {
        &self.payload
    }
}

/// Update semantics are explicit; there is no default interpretation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateSemantics {
    Patch,
    Replace,
}

/// An operation candidate. Cross-image and batch invariants are checked when
/// constructing a ChangeSet, not when assembling this enum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeOperation {
    Added {
        ordinal: u64,
        after: Record,
    },
    Updated {
        ordinal: u64,
        before: Option<Record>,
        after: Record,
        semantics: UpdateSemantics,
    },
    Deleted {
        ordinal: u64,
        identity: RecordReference,
        before: Option<Record>,
    },
}

impl ChangeOperation {
    pub const fn ordinal(&self) -> u64 {
        match self {
            Self::Added { ordinal, .. }
            | Self::Updated { ordinal, .. }
            | Self::Deleted { ordinal, .. } => *ordinal,
        }
    }

    pub fn identity(&self) -> &RecordId {
        match self {
            Self::Added { after, .. } | Self::Updated { after, .. } => after.identity(),
            Self::Deleted { identity, .. } => identity.identity(),
        }
    }
}

/// Shared immutable change-set reference.
pub type ChangeSetRef = Arc<ChangeSet>;

/// An ordered, schema-consistent batch. Stable operation identity is the pair
/// `(change_set.id(), operation.ordinal())`; ordinals need not be contiguous.
#[derive(Debug)]
pub struct ChangeSet {
    id: ChangeSetId,
    schema: SchemaDescriptor,
    operations: Arc<[ChangeOperation]>,
}

impl ChangeSet {
    /// Reject invalid batches rather than sorting or repairing their contents.
    pub fn try_new(
        id: ChangeSetId,
        schema: SchemaDescriptor,
        operations: Vec<ChangeOperation>,
    ) -> Result<ChangeSetRef> {
        let mut ordinals = HashSet::new();
        let mut previous = None;
        for operation in &operations {
            let ordinal = operation.ordinal();
            if !ordinals.insert(ordinal) {
                return Err(ContractError::DuplicateOrdinal { ordinal });
            }
            if let Some(previous) = previous {
                if ordinal < previous {
                    return Err(ContractError::OutOfOrderOrdinal { previous, ordinal });
                }
            }
            previous = Some(ordinal);
            let before = match operation {
                ChangeOperation::Added { after, .. } => {
                    validate_record(&schema, operation, after)?;
                    require_image(ordinal, after, RecordImage::Full)?;
                    None
                }
                ChangeOperation::Updated {
                    before,
                    after,
                    semantics,
                    ..
                } => {
                    validate_record(&schema, operation, after)?;
                    let image = match semantics {
                        UpdateSemantics::Patch => RecordImage::Patch,
                        UpdateSemantics::Replace => RecordImage::Full,
                    };
                    require_image(ordinal, after, image)?;
                    before.as_ref()
                }
                ChangeOperation::Deleted {
                    identity, before, ..
                } => {
                    validate_schema(&schema, identity.schema())?;
                    before.as_ref()
                }
            };
            if let Some(before) = before {
                validate_record(&schema, operation, before)?;
                if before.image == RecordImage::Patch {
                    return Err(ContractError::InvalidImage {
                        ordinal,
                        expected: "full or partial before-image",
                        actual: before.image,
                    });
                }
            }
        }
        Ok(Arc::new(Self {
            id,
            schema,
            operations: operations.into(),
        }))
    }

    pub fn id(&self) -> &ChangeSetId {
        &self.id
    }

    pub fn schema(&self) -> &SchemaDescriptor {
        &self.schema
    }

    pub fn operations(&self) -> &[ChangeOperation] {
        &self.operations
    }

    pub fn is_empty(&self) -> bool {
        self.operations.is_empty()
    }

    /// Empty batches are append-only vacuously. Append-only data uses Adds,
    /// rather than a separate operation with ambiguous mutation semantics.
    pub fn is_append_only(&self) -> bool {
        self.operations
            .iter()
            .all(|operation| matches!(operation, ChangeOperation::Added { .. }))
    }
}

pub(super) fn validate_schema(
    expected: &SchemaDescriptor,
    actual: &SchemaDescriptor,
) -> Result<()> {
    if expected != actual {
        return Err(ContractError::SchemaMismatch {
            expected: Box::new(expected.clone()),
            actual: Box::new(actual.clone()),
        });
    }
    Ok(())
}

fn validate_record(
    schema: &SchemaDescriptor,
    operation: &ChangeOperation,
    record: &Record,
) -> Result<()> {
    validate_schema(schema, record.schema())?;
    if operation.identity() != record.identity() {
        return Err(ContractError::IdentityMismatch {
            ordinal: operation.ordinal(),
            expected: operation.identity().clone(),
            actual: record.identity().clone(),
        });
    }
    Ok(())
}

fn require_image(ordinal: u64, record: &Record, expected: RecordImage) -> Result<()> {
    if record.image != expected {
        return Err(ContractError::InvalidImage {
            ordinal,
            expected: match expected {
                RecordImage::Full => "full after-image",
                RecordImage::Patch => "patch after-image",
                RecordImage::Partial => "partial image",
            },
            actual: record.image,
        });
    }
    Ok(())
}
