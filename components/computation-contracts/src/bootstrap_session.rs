// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Addressed bootstrap sessions between graph components.
//!
//! A consumer asks one producer for a snapshot with a [`BootstrapRequest`]
//! sent upstream over the control plane. The producer answers on the bootstrap
//! edge to that consumer only: a `bootstrap/begin` marker, the snapshot records
//! tagged `bootstrap/record`, then exactly one `bootstrap/complete` (with its
//! handover watermarks and optional completion state) or `bootstrap/failed`
//! (with a reason). Every marker and record carries the session identifier, so a
//! consumer ignores data from sessions it no longer waits for.

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::{
    emission_id, BootstrapWatermark, ChangeEnvelope, ChangeSet, ChangeSetId, ComponentId,
    ControlMessage, PortId, SchemaDescriptor, StreamId, SystemMetadata, MAX_BOOTSTRAP_STATE_BYTES,
    MAX_CONTROL_PAYLOAD_BYTES,
};

/// Control kind used where a bootstrap request crosses a boundary without a
/// typed notification variant.
pub const BOOTSTRAP_REQUEST: &str = "bootstrap/request";
pub const BOOTSTRAP_BEGIN: &str = "bootstrap/begin";
pub const BOOTSTRAP_RECORD: &str = "bootstrap/record";
pub const BOOTSTRAP_STATE: &str = "bootstrap/state";

/// The conventional bootstrap input port of a source or bootstrapping
/// transformer: where its upstream provider (or a source relay) connects.
pub const BOOTSTRAP_INPUT_PORT: &str = "bootstrap";

/// The conventional bootstrap output port of a source, query or provider.
pub const BOOTSTRAP_OUTPUT_PORT: &str = "snapshot";
pub const BOOTSTRAP_COMPLETE: &str = "bootstrap/complete";
pub const BOOTSTRAP_FAILED: &str = "bootstrap/failed";

/// Maximum handover watermarks reported by one session.
pub const MAX_BOOTSTRAP_SESSION_WATERMARKS: usize = 256;

/// Maximum UTF-8 length of a session failure reason.
pub const MAX_BOOTSTRAP_FAILURE_REASON_BYTES: usize = MAX_CONTROL_PAYLOAD_BYTES;

/// Maximum encoded completion payload: the state, every watermark with its
/// position and producer identity, and framing.
pub const MAX_BOOTSTRAP_COMPLETION_BYTES: usize = MAX_BOOTSTRAP_STATE_BYTES
    + MAX_BOOTSTRAP_SESSION_WATERMARKS * (crate::MAX_SOURCE_POSITION_BYTES / 16 + 4096);

/// Identity of one bootstrap request and its reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BootstrapSessionId(uuid::Uuid);

impl BootstrapSessionId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4())
    }

    pub const fn from_uuid(value: uuid::Uuid) -> Self {
        Self(value)
    }

    pub const fn as_uuid(&self) -> &uuid::Uuid {
        &self.0
    }
}

impl Default for BootstrapSessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for BootstrapSessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// An upstream request for one snapshot, addressed to an immediate producer.
///
/// `consumer`/`input_port` select the bootstrap edge the reply travels on;
/// `output_port` names the producer's bootstrap output. `state` is the
/// completion state this producer returned to the consumer by an earlier
/// completed session, if any. `persistent` reports whether the consumer
/// retains its bootstrap outcome durably, so the producer can choose a
/// resumable (persistent) or a volatile handover. `origin` is the component
/// whose bootstrap the request serves when an intermediate component, such as
/// a source relaying to its provider, forwards it; `None` means `consumer`.
///
/// A `resume` request comes from a persistent consumer that already completed
/// its bootstrap and is restarting: the producer restores what its saved
/// `state` describes, such as a paired source's live handover, and completes
/// without records. It never produces a snapshot.
///
/// `labels` are the labels the origin declares it reads (a query's node and
/// relation labels). A relay forwards its origin's labels unchanged. `None`
/// means the origin does not restrict the snapshot (for example a transformer
/// that maps labels); providers then select every configured element.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootstrapRequest {
    pub session: BootstrapSessionId,
    pub consumer: ComponentId,
    pub input_port: PortId,
    pub output_port: PortId,
    #[serde(default)]
    pub persistent: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<Bytes>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<ComponentId>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub resume: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<BootstrapLabels>,
}

impl BootstrapRequest {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.resume || (self.persistent && self.state.is_some()),
            "a resumed bootstrap session requires a persistent consumer and its saved state"
        );
        if let Some(labels) = &self.labels {
            labels.validate()?;
        }
        validate_state(self.state.as_ref())
    }

    /// The component whose bootstrap this request ultimately serves.
    pub fn origin(&self) -> &ComponentId {
        self.origin.as_ref().unwrap_or(&self.consumer)
    }
}

/// Maximum number of labels of each kind a bootstrap request may carry.
pub const MAX_BOOTSTRAP_LABELS: usize = 4096;

/// Maximum UTF-8 length of one requested label.
pub const MAX_BOOTSTRAP_LABEL_BYTES: usize = 1024;

/// The element labels a bootstrapping component declares it reads.
///
/// A provider selects only the tables, endpoints or collections that produce
/// one of these labels. Its own configured selection is an upper bound: it
/// never reads more than it is configured for, and a requested label outside
/// that selection yields no data. Empty sets are a real restriction (the
/// origin reads no labels of that kind), not "everything".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BootstrapLabels {
    #[serde(default)]
    pub nodes: std::collections::BTreeSet<String>,
    #[serde(default)]
    pub relations: std::collections::BTreeSet<String>,
}

impl BootstrapLabels {
    pub fn new(
        nodes: impl IntoIterator<Item = impl Into<String>>,
        relations: impl IntoIterator<Item = impl Into<String>>,
    ) -> anyhow::Result<Self> {
        let labels = Self {
            nodes: nodes.into_iter().map(Into::into).collect(),
            relations: relations.into_iter().map(Into::into).collect(),
        };
        labels.validate()?;
        Ok(labels)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.nodes.len() <= MAX_BOOTSTRAP_LABELS
                && self.relations.len() <= MAX_BOOTSTRAP_LABELS,
            "bootstrap request exceeds {MAX_BOOTSTRAP_LABELS} labels of one kind"
        );
        for label in self.nodes.iter().chain(&self.relations) {
            anyhow::ensure!(
                !label.is_empty() && label.len() <= MAX_BOOTSTRAP_LABEL_BYTES,
                "bootstrap labels must contain 1..={MAX_BOOTSTRAP_LABEL_BYTES} bytes"
            );
        }
        Ok(())
    }

    /// Whether an element with `label` is requested, as a node or relation.
    pub fn requests(&self, label: &str) -> bool {
        self.nodes.contains(label) || self.relations.contains(label)
    }

    /// Whether a node label is requested.
    pub fn requests_node(&self, label: &str) -> bool {
        self.nodes.contains(label)
    }

    /// Whether a relation label is requested.
    pub fn requests_relation(&self, label: &str) -> bool {
        self.relations.contains(label)
    }
}

/// Whether `labels` (the origin's request, `None` = unrestricted) selects an
/// element carrying `label`.
pub fn labels_select(labels: Option<&BootstrapLabels>, label: &str) -> bool {
    labels.is_none_or(|labels| labels.requests(label))
}

fn validate_state(state: Option<&Bytes>) -> anyhow::Result<()> {
    anyhow::ensure!(
        state.is_none_or(|state| !state.is_empty() && state.len() <= MAX_BOOTSTRAP_STATE_BYTES),
        "bootstrap session state must contain 1..={MAX_BOOTSTRAP_STATE_BYTES} bytes"
    );
    Ok(())
}

/// One event of a bootstrap session reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootstrapSessionEvent {
    Begin,
    Record,
    /// Provider state the consumer must retain durably before it continues,
    /// such as a handover a restarted session resumes or releases. The
    /// consumer returns the latest state with its next request.
    State {
        state: Bytes,
    },
    Complete {
        watermarks: Vec<BootstrapWatermark>,
        state: Option<Bytes>,
    },
    Failed {
        reason: String,
    },
}

/// A session event as attached to an envelope's event-local control.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapMarker {
    pub session: BootstrapSessionId,
    pub event: BootstrapSessionEvent,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Completion {
    session: BootstrapSessionId,
    watermarks: Vec<BootstrapWatermark>,
    state: Option<Bytes>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionState {
    session: BootstrapSessionId,
    state: Bytes,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Failure {
    session: BootstrapSessionId,
    reason: String,
}

impl BootstrapMarker {
    pub fn new(session: BootstrapSessionId, event: BootstrapSessionEvent) -> Self {
        Self { session, event }
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        match &self.event {
            BootstrapSessionEvent::Begin | BootstrapSessionEvent::Record => Ok(()),
            BootstrapSessionEvent::State { state } => validate_state(Some(state)),
            BootstrapSessionEvent::Complete { watermarks, state } => {
                anyhow::ensure!(
                    watermarks.len() <= MAX_BOOTSTRAP_SESSION_WATERMARKS,
                    "bootstrap session reports more than {MAX_BOOTSTRAP_SESSION_WATERMARKS} watermarks"
                );
                for watermark in watermarks {
                    watermark.validate()?;
                }
                validate_state(state.as_ref())
            }
            BootstrapSessionEvent::Failed { reason } => {
                anyhow::ensure!(
                    reason.len() <= MAX_BOOTSTRAP_FAILURE_REASON_BYTES,
                    "bootstrap failure reason exceeds {MAX_BOOTSTRAP_FAILURE_REASON_BYTES} bytes"
                );
                Ok(())
            }
        }
    }

    /// Encode as an event-local control message.
    pub fn to_control(&self) -> anyhow::Result<ControlMessage> {
        self.validate()?;
        let session = Bytes::copy_from_slice(self.session.0.as_bytes());
        let (kind, payload) = match &self.event {
            BootstrapSessionEvent::Begin => (BOOTSTRAP_BEGIN, session),
            BootstrapSessionEvent::Record => (BOOTSTRAP_RECORD, session),
            BootstrapSessionEvent::State { state } => (
                BOOTSTRAP_STATE,
                Bytes::from(rmp_serde::to_vec_named(&SessionState {
                    session: self.session,
                    state: state.clone(),
                })?),
            ),
            BootstrapSessionEvent::Complete { watermarks, state } => (
                BOOTSTRAP_COMPLETE,
                Bytes::from(rmp_serde::to_vec_named(&Completion {
                    session: self.session,
                    watermarks: watermarks.clone(),
                    state: state.clone(),
                })?),
            ),
            BootstrapSessionEvent::Failed { reason } => (
                BOOTSTRAP_FAILED,
                Bytes::from(rmp_serde::to_vec_named(&Failure {
                    session: self.session,
                    reason: reason.clone(),
                })?),
            ),
        };
        Ok(ControlMessage::try_new(kind, payload)?)
    }

    /// Decode a bootstrap control message; `None` for any other control kind.
    pub fn from_control(control: &ControlMessage) -> anyhow::Result<Option<Self>> {
        let session = |payload: &Bytes| -> anyhow::Result<BootstrapSessionId> {
            Ok(BootstrapSessionId(uuid::Uuid::from_slice(payload)?))
        };
        let marker = match control.kind() {
            BOOTSTRAP_BEGIN => Self::new(session(control.payload())?, BootstrapSessionEvent::Begin),
            BOOTSTRAP_RECORD => {
                Self::new(session(control.payload())?, BootstrapSessionEvent::Record)
            }
            BOOTSTRAP_STATE => {
                anyhow::ensure!(
                    control.payload().len() <= MAX_BOOTSTRAP_STATE_BYTES + 64,
                    "bootstrap state exceeds its size limit"
                );
                let value: SessionState = rmp_serde::from_slice(control.payload())?;
                Self::new(
                    value.session,
                    BootstrapSessionEvent::State { state: value.state },
                )
            }
            BOOTSTRAP_COMPLETE => {
                anyhow::ensure!(
                    control.payload().len() <= MAX_BOOTSTRAP_COMPLETION_BYTES,
                    "bootstrap completion exceeds {MAX_BOOTSTRAP_COMPLETION_BYTES} bytes"
                );
                let value: Completion = rmp_serde::from_slice(control.payload())?;
                Self::new(
                    value.session,
                    BootstrapSessionEvent::Complete {
                        watermarks: value.watermarks,
                        state: value.state,
                    },
                )
            }
            BOOTSTRAP_FAILED => {
                anyhow::ensure!(
                    control.payload().len() <= MAX_BOOTSTRAP_FAILURE_REASON_BYTES + 64,
                    "bootstrap failure exceeds its size limit"
                );
                let value: Failure = rmp_serde::from_slice(control.payload())?;
                Self::new(
                    value.session,
                    BootstrapSessionEvent::Failed {
                        reason: value.reason,
                    },
                )
            }
            _ => return Ok(None),
        };
        marker.validate()?;
        Ok(Some(marker))
    }

    /// Read the bootstrap marker carried by an envelope, if any.
    pub fn of(envelope: &ChangeEnvelope) -> anyhow::Result<Option<Self>> {
        envelope
            .system()
            .control()
            .map_or(Ok(None), Self::from_control)
    }

    /// A record-free envelope carrying a `begin`, `complete` or `failed` marker.
    pub fn envelope(
        &self,
        stream: StreamId,
        sequence: u64,
        schema: SchemaDescriptor,
    ) -> anyhow::Result<ChangeEnvelope> {
        anyhow::ensure!(
            self.event != BootstrapSessionEvent::Record,
            "a bootstrap record marker must tag a snapshot record"
        );
        let changes = ChangeSet::try_new(
            ChangeSetId::try_new(
                stream.as_str(),
                Bytes::copy_from_slice(&sequence.to_be_bytes()),
            )?,
            schema,
            Vec::new(),
        )?;
        Ok(ChangeEnvelope::new(
            emission_id(&stream, sequence)?,
            changes,
            SystemMetadata::new(stream, sequence).with_control(self.to_control()?),
        ))
    }

    /// Tag one snapshot record for delivery in `session`.
    pub fn tag_record(
        session: BootstrapSessionId,
        record: &ChangeEnvelope,
    ) -> anyhow::Result<ChangeEnvelope> {
        anyhow::ensure!(
            record.system().control().is_none(),
            "bootstrap snapshot records cannot carry event-local control"
        );
        Ok(ChangeEnvelope::new(
            record.id().clone(),
            record.changes().clone(),
            record
                .system()
                .as_ref()
                .clone()
                .with_control(Self::new(session, BootstrapSessionEvent::Record).to_control()?),
        ))
    }

    /// Remove the session tag from a delivered snapshot record.
    pub fn untag_record(record: &ChangeEnvelope) -> ChangeEnvelope {
        ChangeEnvelope::new(
            record.id().clone(),
            record.changes().clone(),
            record.system().as_ref().clone().without_control(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream() -> StreamId {
        StreamId::try_new("bootstrap/test").unwrap()
    }

    fn schema() -> SchemaDescriptor {
        crate::GraphChangeCodec::schema().descriptor().clone()
    }

    #[test]
    fn markers_round_trip_through_control() -> anyhow::Result<()> {
        let session = BootstrapSessionId::new();
        let watermark = BootstrapWatermark {
            stream: stream(),
            source_id: Some(ComponentId::try_new(
                "00000000-0000-4000-8000-000000000001",
            )?),
            sequence: 7,
            position: Some(Bytes::from_static(b"cursor")),
            producer: None,
        };
        for event in [
            BootstrapSessionEvent::Begin,
            BootstrapSessionEvent::State {
                state: Bytes::from_static(b"handover"),
            },
            BootstrapSessionEvent::Complete {
                watermarks: vec![watermark],
                state: Some(Bytes::from_static(b"state")),
            },
            BootstrapSessionEvent::Failed {
                reason: "source unavailable".into(),
            },
        ] {
            let marker = BootstrapMarker::new(session, event);
            let envelope = marker.envelope(stream(), 1, schema())?;
            assert!(envelope.changes().is_empty());
            assert_eq!(BootstrapMarker::of(&envelope)?, Some(marker));
        }
        Ok(())
    }

    #[test]
    fn records_are_tagged_and_untagged() -> anyhow::Result<()> {
        let session = BootstrapSessionId::new();
        let begin = BootstrapMarker::new(session, BootstrapSessionEvent::Begin).envelope(
            stream(),
            1,
            schema(),
        )?;
        let record = BootstrapMarker::untag_record(&begin);
        let tagged = BootstrapMarker::tag_record(session, &record)?;
        assert_eq!(
            BootstrapMarker::of(&tagged)?,
            Some(BootstrapMarker::new(session, BootstrapSessionEvent::Record))
        );
        assert_eq!(
            BootstrapMarker::untag_record(&tagged).system(),
            record.system()
        );
        assert!(BootstrapMarker::tag_record(session, &tagged).is_err());
        assert!(BootstrapMarker::new(session, BootstrapSessionEvent::Record)
            .envelope(stream(), 2, schema())
            .is_err());
        Ok(())
    }

    #[test]
    fn limits_are_enforced() -> anyhow::Result<()> {
        let session = BootstrapSessionId::new();
        let oversized = BootstrapMarker::new(
            session,
            BootstrapSessionEvent::Failed {
                reason: "x".repeat(MAX_BOOTSTRAP_FAILURE_REASON_BYTES + 1),
            },
        );
        assert!(oversized.to_control().is_err());
        let state = BootstrapMarker::new(
            session,
            BootstrapSessionEvent::Complete {
                watermarks: Vec::new(),
                state: Some(Bytes::from(vec![0; MAX_BOOTSTRAP_STATE_BYTES + 1])),
            },
        );
        assert!(state.to_control().is_err());
        let request = BootstrapRequest {
            session,
            consumer: ComponentId::try_new("00000000-0000-4000-8000-000000000002")?,
            input_port: PortId::try_new("bootstrap")?,
            output_port: PortId::try_new("bootstrap")?,
            persistent: false,
            state: Some(Bytes::new()),
            origin: None,
            resume: false,
            labels: None,
        };
        assert!(request.validate().is_err());
        let unrelated = ControlMessage::try_new("checkpoint", Bytes::new())?;
        assert_eq!(BootstrapMarker::from_control(&unrelated)?, None);
        Ok(())
    }

    #[test]
    fn request_round_trips_through_serde() -> anyhow::Result<()> {
        let request = BootstrapRequest {
            session: BootstrapSessionId::new(),
            consumer: ComponentId::try_new("00000000-0000-4000-8000-000000000002")?,
            input_port: PortId::try_new("bootstrap")?,
            output_port: PortId::try_new("snapshot")?,
            persistent: true,
            state: Some(Bytes::from_static(b"resume")),
            origin: Some(ComponentId::try_new(
                "00000000-0000-4000-8000-000000000003",
            )?),
            resume: true,
            labels: Some(BootstrapLabels::new(["Person"], ["KNOWS"])?),
        };
        request.validate()?;
        let decoded: BootstrapRequest = serde_json::from_slice(&serde_json::to_vec(&request)?)?;
        assert_eq!(decoded, request);
        Ok(())
    }

    #[test]
    fn labels_restrict_selection_and_are_bounded() -> anyhow::Result<()> {
        let labels = BootstrapLabels::new(["Person"], ["KNOWS"])?;
        assert!(labels_select(Some(&labels), "Person"));
        assert!(labels_select(Some(&labels), "KNOWS"));
        assert!(!labels_select(Some(&labels), "Order"));
        assert!(
            labels_select(None, "Order"),
            "no request selects everything"
        );
        let empty = BootstrapLabels::default();
        assert!(
            !labels_select(Some(&empty), "Person"),
            "an empty request is a real restriction"
        );
        assert!(BootstrapLabels::new([""], Vec::<String>::new()).is_err());
        assert!(BootstrapLabels::new(
            ["x".repeat(MAX_BOOTSTRAP_LABEL_BYTES + 1)],
            Vec::<String>::new()
        )
        .is_err());
        Ok(())
    }
}
