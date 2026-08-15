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

//! The `workgraph.event/v1` envelope and its four exact payloads.
//!
//! # Canonical envelope
//!
//! Serialization always emits exactly these keys in exactly this order:
//!
//! ```json
//! {
//!   "schemaVersion": "workgraph.event/v1",
//!   "eventId": "event:...",
//!   "eventType": "ResponsibilityAssigned",
//!   "runId": "run:...",
//!   "projectItemNodeId": "PVTI_...",
//!   "subjectNodeId": "I_...",
//!   "payload": {}
//! }
//! ```
//!
//! The envelope deliberately carries **no** actor, repository, issue number,
//! subject type, timestamp, route ID, responsibility ID, causation ID, or human
//! summary. Those are either derived from authoritative GitHub Source metadata
//! (actor, time, repository, number, subject type) or from graph relations. A
//! JSON document that claims any of them is rejected, because trusting a
//! self-asserted actor or timestamp would let a comment author forge identity.
//!
//! # Strictness
//!
//! Parsing is fail-closed at every level:
//!
//! * unknown or missing envelope keys are rejected;
//! * `schemaVersion` must be exactly `workgraph.event/v1`;
//! * each payload is parsed into its own `deny_unknown_fields` struct chosen by
//!   `eventType`, so a payload valid for one event type is rejected under
//!   another;
//! * enumerated fields accept only their exact tokens; and
//! * semantically inconsistent (but structurally valid) payloads — a `passed`
//!   outcome carrying `required-marker-missing`, or a `NeedsMoreInformation`
//!   routing decision carrying `issue-risk-profiling` — are rejected.

use std::fmt;

use serde::{Deserialize, Serialize};

/// The only schema version this crate accepts or emits.
pub const SCHEMA_VERSION: &str = "workgraph.event/v1";

/// Errors produced while validating a WorkGraph event or one of its scalars.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EventError {
    /// `schemaVersion` was not exactly [`SCHEMA_VERSION`].
    #[error("schemaVersion must be '{SCHEMA_VERSION}', got '{0}'")]
    SchemaVersion(String),
    /// A scalar did not match its required grammar.
    #[error("{field} '{value}' is invalid: {reason}")]
    Scalar {
        /// The field that failed validation.
        field: &'static str,
        /// The rejected value.
        value: String,
        /// Why it was rejected.
        reason: &'static str,
    },
    /// The payload did not match the payload schema selected by `eventType`.
    #[error("payload for eventType '{event_type}' is invalid: {reason}")]
    Payload {
        /// The event type whose payload schema was applied.
        event_type: &'static str,
        /// The underlying serde error message.
        reason: String,
    },
    /// The envelope itself was not a valid `workgraph.event/v1` document.
    #[error("event envelope is invalid: {0}")]
    Envelope(String),
    /// A structurally valid payload carried a self-contradicting combination.
    #[error("payload for eventType '{event_type}' is inconsistent: {reason}")]
    Inconsistent {
        /// The event type whose payload was inconsistent.
        event_type: &'static str,
        /// The contradiction that was detected.
        reason: String,
    },
}

impl EventError {
    fn scalar(field: &'static str, value: impl Into<String>, reason: &'static str) -> Self {
        Self::Scalar {
            field,
            value: value.into(),
            reason,
        }
    }
}

macro_rules! prefixed_hex_id {
    (
        $(#[$meta:meta])*
        $name:ident, $field:literal, $prefix:literal, $len:literal
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            /// The declared textual prefix, including the trailing colon.
            pub const PREFIX: &'static str = $prefix;
            /// The number of lowercase hex characters after the prefix.
            pub const HEX_LEN: usize = $len;

            /// Build the identifier from bare lowercase hex (no prefix).
            pub fn from_hex(hex: &str) -> Result<Self, EventError> {
                Self::try_from(format!("{}{hex}", Self::PREFIX))
            }

            /// The full identifier, including its prefix.
            pub fn as_str(&self) -> &str {
                &self.0
            }

            /// The lowercase hex portion, without the prefix.
            pub fn hex(&self) -> &str {
                &self.0[Self::PREFIX.len()..]
            }
        }

        impl TryFrom<String> for $name {
            type Error = EventError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                let Some(hex) = value.strip_prefix(Self::PREFIX) else {
                    return Err(EventError::scalar($field, value, concat!("must start with '", $prefix, "'")));
                };
                if hex.len() != Self::HEX_LEN {
                    return Err(EventError::scalar($field, value, concat!("must carry exactly ", stringify!($len), " hex characters")));
                }
                if !hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
                    return Err(EventError::scalar($field, value, "must be lowercase hex"));
                }
                Ok(Self(value))
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

prefixed_hex_id!(
    /// A SHA-256 content digest rendered as `sha256:<64-hex>`.
    Sha256Digest,
    "contentDigest",
    "sha256:",
    64
);

prefixed_hex_id!(
    /// A deterministic run identifier rendered as `run:<64-hex>`.
    ///
    /// See [`crate::ids::run_id`] for the derivation.
    RunId,
    "runId",
    "run:",
    64
);

prefixed_hex_id!(
    /// A deterministic event identifier rendered as `event:<64-hex>`.
    ///
    /// See [`crate::ids::event_id`] for the derivation.
    EventId,
    "eventId",
    "event:",
    64
);

/// An agent-task execution identifier, rendered as `execution:<opaque>`.
///
/// The suffix is opaque to the event contract: it is minted by whichever
/// component reserves the execution, and only its stability matters.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ExecutionId(String);

impl ExecutionId {
    /// The declared textual prefix, including the trailing colon.
    pub const PREFIX: &'static str = "execution:";

    /// Build an execution ID from its opaque suffix.
    pub fn from_suffix(suffix: &str) -> Result<Self, EventError> {
        Self::try_from(format!("{}{suffix}", Self::PREFIX))
    }

    /// The full identifier, including its prefix.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ExecutionId {
    type Error = EventError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let Some(suffix) = value.strip_prefix(Self::PREFIX) else {
            return Err(EventError::scalar(
                "executionId",
                value,
                "must start with 'execution:'",
            ));
        };
        if suffix.is_empty() {
            return Err(EventError::scalar(
                "executionId",
                value,
                "must carry a non-empty suffix",
            ));
        }
        if suffix
            .bytes()
            .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
        {
            return Err(EventError::scalar(
                "executionId",
                value,
                "must not contain whitespace or control characters",
            ));
        }
        Ok(Self(value))
    }
}

impl From<ExecutionId> for String {
    fn from(value: ExecutionId) -> Self {
        value.0
    }
}

impl fmt::Display for ExecutionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A blob-pinned agent profile reference, rendered as `<profile>@<40-hex>`.
///
/// The 40-hex suffix is the Git blob SHA-1 of the profile file, so a profile
/// reference names an exact immutable file revision rather than a mutable path.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ProfileRef(String);

impl ProfileRef {
    /// Build a profile reference from a profile name and blob SHA.
    pub fn new(profile: &str, blob_sha: &str) -> Result<Self, EventError> {
        Self::try_from(format!("{profile}@{blob_sha}"))
    }

    /// The full reference, including the blob SHA.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The profile name portion (before `@`).
    pub fn profile(&self) -> &str {
        self.0.split_once('@').map(|(name, _)| name).unwrap_or("")
    }

    /// The pinned Git blob SHA portion (after `@`).
    pub fn blob_sha(&self) -> &str {
        self.0.split_once('@').map(|(_, sha)| sha).unwrap_or("")
    }
}

impl TryFrom<String> for ProfileRef {
    type Error = EventError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let Some((profile, sha)) = value.split_once('@') else {
            return Err(EventError::scalar(
                "profileRef",
                value,
                "must be '<profile>@<40-hex blob sha>'",
            ));
        };
        if profile.is_empty()
            || !profile
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err(EventError::scalar(
                "profileRef",
                value,
                "profile name must be non-empty lowercase alphanumeric or '-'",
            ));
        }
        if sha.len() != 40
            || !sha
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(EventError::scalar(
                "profileRef",
                value,
                "blob sha must be exactly 40 lowercase hex characters",
            ));
        }
        Ok(Self(value))
    }
}

impl From<ProfileRef> for String {
    fn from(value: ProfileRef) -> Self {
        value.0
    }
}

impl fmt::Display for ProfileRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The four event types carried by `workgraph.event/v1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum WorkGraphEventType {
    /// A responsibility was assigned for a Project Item at an exact body digest.
    ResponsibilityAssigned,
    /// An agent-task execution was created (or adopted) for that responsibility.
    ExecutionStarted,
    /// The issue-validation execution reported its outcome.
    CompletedIssueValidation,
    /// The router chose the next status and responsibility.
    RoutingDecided,
}

impl WorkGraphEventType {
    /// The exact serialized token for this event type.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ResponsibilityAssigned => "ResponsibilityAssigned",
            Self::ExecutionStarted => "ExecutionStarted",
            Self::CompletedIssueValidation => "CompletedIssueValidation",
            Self::RoutingDecided => "RoutingDecided",
        }
    }

    /// Every event type, in declaration order.
    pub const ALL: [Self; 4] = [
        Self::ResponsibilityAssigned,
        Self::ExecutionStarted,
        Self::CompletedIssueValidation,
        Self::RoutingDecided,
    ];
}

impl fmt::Display for WorkGraphEventType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The only responsibility type that can be assigned in v1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AssignedResponsibilityType {
    /// Validate the issue body against the required marker.
    #[serde(rename = "issue-validation")]
    IssueValidation,
}

/// The responsibility types the router can name as the next step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NextResponsibilityType {
    /// Profile the risk of a validated issue.
    #[serde(rename = "issue-risk-profiling")]
    IssueRiskProfiling,
    /// Ask the submitter to correct the issue.
    #[serde(rename = "issue-correction")]
    IssueCorrection,
}

impl NextResponsibilityType {
    /// The exact serialized token.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::IssueRiskProfiling => "issue-risk-profiling",
            Self::IssueCorrection => "issue-correction",
        }
    }
}

/// The outcome of one issue validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ValidationOutcome {
    /// The required marker was present.
    Passed,
    /// The required marker was missing.
    Failed,
}

impl ValidationOutcome {
    /// The exact serialized token.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
        }
    }
}

/// The machine-readable reason behind a [`ValidationOutcome`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ValidationReasonCode {
    /// The required marker was found in the authoritative issue body.
    #[serde(rename = "required-marker-present")]
    RequiredMarkerPresent,
    /// The required marker was absent from the authoritative issue body.
    #[serde(rename = "required-marker-missing")]
    RequiredMarkerMissing,
}

impl ValidationReasonCode {
    /// The exact serialized token.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::RequiredMarkerPresent => "required-marker-present",
            Self::RequiredMarkerMissing => "required-marker-missing",
        }
    }
}

/// The only Project status a routing decision can move away from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RoutingFromStatus {
    /// The Project Item is awaiting issue validation.
    AwaitingValidation,
}

impl RoutingFromStatus {
    /// The exact serialized token.
    pub fn as_str(&self) -> &'static str {
        "AwaitingValidation"
    }
}

/// The Project statuses a routing decision can move to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RoutingToStatus {
    /// Validation passed; risk profiling is next.
    AwaitingIssueRiskProfiling,
    /// Validation failed; the submitter must correct the issue.
    NeedsMoreInformation,
}

impl RoutingToStatus {
    /// The exact serialized token.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::AwaitingIssueRiskProfiling => "AwaitingIssueRiskProfiling",
            Self::NeedsMoreInformation => "NeedsMoreInformation",
        }
    }
}

/// Payload of [`WorkGraphEventType::ResponsibilityAssigned`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ResponsibilityAssignedPayload {
    /// Always `issue-validation` in v1.
    pub responsibility_type: AssignedResponsibilityType,
    /// The blob-pinned agent profile that must perform the responsibility.
    pub profile_ref: ProfileRef,
    /// The digest of the exact issue body this responsibility covers.
    pub content_digest: Sha256Digest,
}

/// Payload of [`WorkGraphEventType::ExecutionStarted`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ExecutionStartedPayload {
    /// The execution reserved for this run.
    pub execution_id: ExecutionId,
    /// The GitHub agent-task identifier that was created or adopted.
    pub task_id: String,
}

/// Payload of [`WorkGraphEventType::CompletedIssueValidation`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CompletedIssueValidationPayload {
    /// The execution that produced this outcome.
    pub execution_id: ExecutionId,
    /// Whether validation passed.
    pub outcome: ValidationOutcome,
    /// The machine-readable reason for [`Self::outcome`].
    pub reason_code: ValidationReasonCode,
}

/// Payload of [`WorkGraphEventType::RoutingDecided`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RoutingDecidedPayload {
    /// The status the item is being routed away from.
    pub from_status: RoutingFromStatus,
    /// The status the item is being routed to.
    pub to_status: RoutingToStatus,
    /// The responsibility that becomes current after the move.
    pub next_responsibility_type: NextResponsibilityType,
}

impl CompletedIssueValidationPayload {
    /// Reject outcome/reason pairs that contradict each other.
    fn validate_consistency(&self) -> Result<(), EventError> {
        let coherent = matches!(
            (self.outcome, self.reason_code),
            (
                ValidationOutcome::Passed,
                ValidationReasonCode::RequiredMarkerPresent
            ) | (
                ValidationOutcome::Failed,
                ValidationReasonCode::RequiredMarkerMissing
            )
        );
        if coherent {
            return Ok(());
        }
        Err(EventError::Inconsistent {
            event_type: WorkGraphEventType::CompletedIssueValidation.as_str(),
            reason: format!(
                "outcome '{}' cannot carry reasonCode '{}'",
                self.outcome.as_str(),
                self.reason_code.as_str()
            ),
        })
    }
}

impl RoutingDecidedPayload {
    /// The routing decision implied by a validation outcome.
    pub fn for_outcome(outcome: ValidationOutcome) -> Self {
        match outcome {
            ValidationOutcome::Passed => Self {
                from_status: RoutingFromStatus::AwaitingValidation,
                to_status: RoutingToStatus::AwaitingIssueRiskProfiling,
                next_responsibility_type: NextResponsibilityType::IssueRiskProfiling,
            },
            ValidationOutcome::Failed => Self {
                from_status: RoutingFromStatus::AwaitingValidation,
                to_status: RoutingToStatus::NeedsMoreInformation,
                next_responsibility_type: NextResponsibilityType::IssueCorrection,
            },
        }
    }

    /// Reject destination/responsibility pairs that contradict each other.
    fn validate_consistency(&self) -> Result<(), EventError> {
        let coherent = matches!(
            (self.to_status, self.next_responsibility_type),
            (
                RoutingToStatus::AwaitingIssueRiskProfiling,
                NextResponsibilityType::IssueRiskProfiling
            ) | (
                RoutingToStatus::NeedsMoreInformation,
                NextResponsibilityType::IssueCorrection
            )
        );
        if coherent {
            return Ok(());
        }
        Err(EventError::Inconsistent {
            event_type: WorkGraphEventType::RoutingDecided.as_str(),
            reason: format!(
                "toStatus '{}' cannot carry nextResponsibilityType '{}'",
                self.to_status.as_str(),
                self.next_responsibility_type.as_str()
            ),
        })
    }
}

/// The typed payload of a WorkGraph event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkGraphEventPayload {
    /// See [`ResponsibilityAssignedPayload`].
    ResponsibilityAssigned(ResponsibilityAssignedPayload),
    /// See [`ExecutionStartedPayload`].
    ExecutionStarted(ExecutionStartedPayload),
    /// See [`CompletedIssueValidationPayload`].
    CompletedIssueValidation(CompletedIssueValidationPayload),
    /// See [`RoutingDecidedPayload`].
    RoutingDecided(RoutingDecidedPayload),
}

impl WorkGraphEventPayload {
    /// The event type this payload belongs to.
    pub fn event_type(&self) -> WorkGraphEventType {
        match self {
            Self::ResponsibilityAssigned(_) => WorkGraphEventType::ResponsibilityAssigned,
            Self::ExecutionStarted(_) => WorkGraphEventType::ExecutionStarted,
            Self::CompletedIssueValidation(_) => WorkGraphEventType::CompletedIssueValidation,
            Self::RoutingDecided(_) => WorkGraphEventType::RoutingDecided,
        }
    }

    fn validate_consistency(&self) -> Result<(), EventError> {
        match self {
            Self::CompletedIssueValidation(payload) => payload.validate_consistency(),
            Self::RoutingDecided(payload) => payload.validate_consistency(),
            Self::ResponsibilityAssigned(_) | Self::ExecutionStarted(_) => Ok(()),
        }
    }

    fn from_value(
        event_type: WorkGraphEventType,
        value: serde_json::Value,
    ) -> Result<Self, EventError> {
        fn parse<T: serde::de::DeserializeOwned>(
            event_type: WorkGraphEventType,
            value: serde_json::Value,
        ) -> Result<T, EventError> {
            serde_json::from_value(value).map_err(|error| EventError::Payload {
                event_type: event_type.as_str(),
                reason: error.to_string(),
            })
        }

        let payload = match event_type {
            WorkGraphEventType::ResponsibilityAssigned => {
                Self::ResponsibilityAssigned(parse(event_type, value)?)
            }
            WorkGraphEventType::ExecutionStarted => {
                Self::ExecutionStarted(parse(event_type, value)?)
            }
            WorkGraphEventType::CompletedIssueValidation => {
                Self::CompletedIssueValidation(parse(event_type, value)?)
            }
            WorkGraphEventType::RoutingDecided => Self::RoutingDecided(parse(event_type, value)?),
        };
        payload.validate_consistency()?;
        Ok(payload)
    }
}

impl Serialize for WorkGraphEventPayload {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::ResponsibilityAssigned(payload) => payload.serialize(serializer),
            Self::ExecutionStarted(payload) => payload.serialize(serializer),
            Self::CompletedIssueValidation(payload) => payload.serialize(serializer),
            Self::RoutingDecided(payload) => payload.serialize(serializer),
        }
    }
}

/// A fully validated `workgraph.event/v1` document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkGraphEvent {
    /// Deterministic event identifier; see [`crate::ids::event_id`].
    pub event_id: EventId,
    /// Deterministic run identifier; see [`crate::ids::run_id`].
    pub run_id: RunId,
    /// The GitHub Projects (v2) item node ID (`PVTI_...`).
    pub project_item_node_id: String,
    /// The GitHub issue node ID (`I_...`) the run is about.
    pub subject_node_id: String,
    /// The typed payload; its variant fixes `eventType`.
    pub payload: WorkGraphEventPayload,
}

/// Wire representation used only to enforce key order and reject unknown keys.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct WireEvent<P> {
    schema_version: String,
    event_id: EventId,
    event_type: WorkGraphEventType,
    run_id: RunId,
    project_item_node_id: String,
    subject_node_id: String,
    payload: P,
}

impl WorkGraphEvent {
    /// Build an event, deriving `eventId` from `runId` and the payload variant.
    pub fn new(
        run_id: RunId,
        project_item_node_id: impl Into<String>,
        subject_node_id: impl Into<String>,
        payload: WorkGraphEventPayload,
    ) -> Result<Self, EventError> {
        let event_id = crate::ids::event_id(&run_id, payload.event_type());
        let event = Self {
            event_id,
            run_id,
            project_item_node_id: project_item_node_id.into(),
            subject_node_id: subject_node_id.into(),
            payload,
        };
        event.validate()?;
        Ok(event)
    }

    /// The event type implied by the payload variant.
    pub fn event_type(&self) -> WorkGraphEventType {
        self.payload.event_type()
    }

    /// Validate node-ID shapes, payload consistency, and `eventId` derivation.
    pub fn validate(&self) -> Result<(), EventError> {
        validate_node_id(
            "projectItemNodeId",
            &self.project_item_node_id,
            "PVTI_",
            "must be a GitHub Projects v2 item node ID starting with 'PVTI_'",
        )?;
        validate_node_id(
            "subjectNodeId",
            &self.subject_node_id,
            "I_",
            "must be a GitHub issue node ID starting with 'I_'",
        )?;
        self.payload.validate_consistency()?;
        let expected = crate::ids::event_id(&self.run_id, self.event_type());
        if expected != self.event_id {
            return Err(EventError::Envelope(format!(
                "eventId '{}' is not the deterministic ID for runId '{}' and eventType '{}' (expected '{}')",
                self.event_id,
                self.run_id,
                self.event_type(),
                expected
            )));
        }
        Ok(())
    }

    /// Serialize to canonical compact JSON with the frozen key order.
    pub fn to_canonical_json(&self) -> String {
        let wire = WireEvent {
            schema_version: SCHEMA_VERSION.to_string(),
            event_id: self.event_id.clone(),
            event_type: self.event_type(),
            run_id: self.run_id.clone(),
            project_item_node_id: self.project_item_node_id.clone(),
            subject_node_id: self.subject_node_id.clone(),
            payload: &self.payload,
        };
        serde_json::to_string(&wire).expect("workgraph event serialization is infallible")
    }

    /// Parse and fully validate a canonical JSON document.
    pub fn from_json(json: &str) -> Result<Self, EventError> {
        let value: serde_json::Value = serde_json::from_str(json)
            .map_err(|error| EventError::Envelope(format!("not valid JSON: {error}")))?;
        Self::from_value(value)
    }

    /// Parse and fully validate an already-decoded JSON value.
    pub fn from_value(value: serde_json::Value) -> Result<Self, EventError> {
        if !value.is_object() {
            return Err(EventError::Envelope(
                "must be a single JSON object".to_string(),
            ));
        }
        let wire: WireEvent<serde_json::Value> = serde_json::from_value(value)
            .map_err(|error| EventError::Envelope(error.to_string()))?;
        if wire.schema_version != SCHEMA_VERSION {
            return Err(EventError::SchemaVersion(wire.schema_version));
        }
        let payload = WorkGraphEventPayload::from_value(wire.event_type, wire.payload)?;
        let event = Self {
            event_id: wire.event_id,
            run_id: wire.run_id,
            project_item_node_id: wire.project_item_node_id,
            subject_node_id: wire.subject_node_id,
            payload,
        };
        event.validate()?;
        Ok(event)
    }
}

fn validate_node_id(
    field: &'static str,
    value: &str,
    prefix: &str,
    reason: &'static str,
) -> Result<(), EventError> {
    if !value.starts_with(prefix) || value.len() <= prefix.len() {
        return Err(EventError::scalar(field, value, reason));
    }
    if value
        .bytes()
        .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
    {
        return Err(EventError::scalar(
            field,
            value,
            "must not contain whitespace or control characters",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{body_digest, run_id};

    const ITEM: &str = "PVTI_lADOABCDEF4AbcDEzgXYZ123";
    const SUBJECT: &str = "I_kwDOABCDEF6ABCDE";
    const BLOB: &str = "0123456789abcdef0123456789abcdef01234567";

    fn sample_run() -> RunId {
        run_id(
            ITEM,
            SUBJECT,
            &body_digest(Some("Ready. workgraph:validate")),
        )
    }

    fn assigned() -> WorkGraphEvent {
        WorkGraphEvent::new(
            sample_run(),
            ITEM,
            SUBJECT,
            WorkGraphEventPayload::ResponsibilityAssigned(ResponsibilityAssignedPayload {
                responsibility_type: AssignedResponsibilityType::IssueValidation,
                profile_ref: ProfileRef::new("issue-validator", BLOB).expect("valid profile ref"),
                content_digest: body_digest(Some("Ready. workgraph:validate")),
            }),
        )
        .expect("valid event")
    }

    #[test]
    fn canonical_key_order_is_frozen() {
        let json = assigned().to_canonical_json();
        let keys: Vec<&str> = json
            .split("\":")
            .filter_map(|chunk| chunk.rsplit('"').next())
            .collect();
        assert!(
            json.starts_with(r#"{"schemaVersion":"workgraph.event/v1","eventId":"event:"#),
            "unexpected canonical prefix: {json}"
        );
        let envelope_order = [
            "schemaVersion",
            "eventId",
            "eventType",
            "runId",
            "projectItemNodeId",
            "subjectNodeId",
            "payload",
        ];
        let mut positions = envelope_order
            .iter()
            .map(|key| json.find(&format!("\"{key}\":")).expect("key present"));
        let mut previous = positions.next().expect("first key");
        for position in positions {
            assert!(
                position > previous,
                "envelope keys are out of order: {json}"
            );
            previous = position;
        }
        assert!(!keys.is_empty());
    }

    #[test]
    fn payload_key_order_is_frozen() {
        let json = assigned().to_canonical_json();
        let payload = json
            .split_once("\"payload\":")
            .expect("payload present")
            .1
            .to_string();
        assert!(
            payload.starts_with(r#"{"responsibilityType":"issue-validation","profileRef":"#),
            "unexpected payload order: {payload}"
        );
    }

    #[test]
    fn round_trips_through_canonical_json() {
        let event = assigned();
        let parsed = WorkGraphEvent::from_json(&event.to_canonical_json()).expect("round trip");
        assert_eq!(parsed, event);
    }

    #[test]
    fn rejects_unknown_envelope_fields() {
        let mut value: serde_json::Value =
            serde_json::from_str(&assigned().to_canonical_json()).expect("valid json");
        value["actor"] = serde_json::json!("mallory");
        let error = WorkGraphEvent::from_value(value).expect_err("unknown field must be rejected");
        assert!(
            matches!(error, EventError::Envelope(ref message) if message.contains("actor")),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn rejects_unknown_payload_fields() {
        let mut value: serde_json::Value =
            serde_json::from_str(&assigned().to_canonical_json()).expect("valid json");
        value["payload"]["timestamp"] = serde_json::json!("2026-01-01T00:00:00Z");
        let error = WorkGraphEvent::from_value(value).expect_err("unknown payload field");
        assert!(
            matches!(error, EventError::Payload { .. }),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn rejects_payload_from_a_different_event_type() {
        let mut value: serde_json::Value =
            serde_json::from_str(&assigned().to_canonical_json()).expect("valid json");
        value["eventType"] = serde_json::json!("ExecutionStarted");
        let error = WorkGraphEvent::from_value(value).expect_err("payload mismatch");
        assert!(
            matches!(error, EventError::Payload { event_type, .. } if event_type == "ExecutionStarted"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn rejects_tampered_event_id() {
        let mut value: serde_json::Value =
            serde_json::from_str(&assigned().to_canonical_json()).expect("valid json");
        value["eventId"] = serde_json::json!(format!("event:{}", "0".repeat(64)));
        let error = WorkGraphEvent::from_value(value).expect_err("event id must be derived");
        assert!(
            matches!(error, EventError::Envelope(ref message) if message.contains("deterministic")),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn rejects_wrong_schema_version() {
        let json = assigned()
            .to_canonical_json()
            .replace("workgraph.event/v1", "workgraph.event/v2");
        let error = WorkGraphEvent::from_json(&json).expect_err("schema version pinned");
        assert!(matches!(error, EventError::SchemaVersion(_)));
    }

    #[test]
    fn rejects_inconsistent_completion_payload() {
        let run = sample_run();
        let event_id = crate::ids::event_id(&run, WorkGraphEventType::CompletedIssueValidation);
        let value = serde_json::json!({
            "schemaVersion": SCHEMA_VERSION,
            "eventId": event_id.as_str(),
            "eventType": "CompletedIssueValidation",
            "runId": run.as_str(),
            "projectItemNodeId": ITEM,
            "subjectNodeId": SUBJECT,
            "payload": {
                "executionId": "execution:abc",
                "outcome": "passed",
                "reasonCode": "required-marker-missing"
            }
        });
        let error = WorkGraphEvent::from_value(value).expect_err("inconsistent payload");
        assert!(matches!(error, EventError::Inconsistent { .. }));
    }

    #[test]
    fn rejects_inconsistent_routing_payload() {
        let run = sample_run();
        let event_id = crate::ids::event_id(&run, WorkGraphEventType::RoutingDecided);
        let value = serde_json::json!({
            "schemaVersion": SCHEMA_VERSION,
            "eventId": event_id.as_str(),
            "eventType": "RoutingDecided",
            "runId": run.as_str(),
            "projectItemNodeId": ITEM,
            "subjectNodeId": SUBJECT,
            "payload": {
                "fromStatus": "AwaitingValidation",
                "toStatus": "NeedsMoreInformation",
                "nextResponsibilityType": "issue-risk-profiling"
            }
        });
        let error = WorkGraphEvent::from_value(value).expect_err("inconsistent routing");
        assert!(matches!(error, EventError::Inconsistent { .. }));
    }

    #[test]
    fn rejects_removed_legacy_envelope_fields() {
        for legacy in [
            "actor",
            "repository",
            "number",
            "subjectType",
            "timestamp",
            "routeId",
            "responsibilityId",
            "causationId",
            "summary",
        ] {
            let mut value: serde_json::Value =
                serde_json::from_str(&assigned().to_canonical_json()).expect("valid json");
            value[legacy] = serde_json::json!("x");
            let error =
                WorkGraphEvent::from_value(value).expect_err("legacy field must be rejected");
            assert!(
                matches!(error, EventError::Envelope(_)),
                "legacy field '{legacy}' produced unexpected error: {error}"
            );
        }
    }

    #[test]
    fn scalar_grammars_are_enforced() {
        assert!(Sha256Digest::try_from("sha256:abc".to_string()).is_err());
        assert!(Sha256Digest::try_from(format!("sha256:{}", "A".repeat(64))).is_err());
        assert!(Sha256Digest::try_from(format!("sha256:{}", "a".repeat(64))).is_ok());
        assert!(ExecutionId::try_from("exec:1".to_string()).is_err());
        assert!(ExecutionId::try_from("execution:".to_string()).is_err());
        assert!(ExecutionId::try_from("execution:a b".to_string()).is_err());
        assert!(ProfileRef::new("issue-validator", "zz").is_err());
        assert!(ProfileRef::new("Issue-Validator", BLOB).is_err());
        let profile = ProfileRef::new("issue-validator", BLOB).expect("valid");
        assert_eq!(profile.profile(), "issue-validator");
        assert_eq!(profile.blob_sha(), BLOB);
    }

    #[test]
    fn rejects_malformed_node_ids() {
        let run = sample_run();
        let payload = WorkGraphEventPayload::ExecutionStarted(ExecutionStartedPayload {
            execution_id: ExecutionId::from_suffix("abc").expect("valid"),
            task_id: "task-1".to_string(),
        });
        assert!(WorkGraphEvent::new(run.clone(), "PVT_wrong", SUBJECT, payload.clone()).is_err());
        assert!(WorkGraphEvent::new(run, ITEM, "PR_wrong", payload).is_err());
    }
}
