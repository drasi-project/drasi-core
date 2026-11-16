// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Shared logical output identity and content comparison for replay and delivery.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    AdmissionRejection, ChangeEnvelope, ComponentId, EnvelopeCodec, GraphProducerIdentity,
    GraphProducerProgress, PortId, QueryChangeCodec, QueryRecoveryIdentity, StreamId,
};

/// Stable namespace for records owned by one output endpoint. Epochs and
/// transport sequences are deliberately separate from record identity.
pub fn dataset_namespace(
    construction_scope: &str,
    graph_id: &str,
    component: &ComponentId,
    port: &PortId,
) -> super::Result<String> {
    super::data::validate_identifier("output construction scope", construction_scope)?;
    super::data::validate_identifier("output graph", graph_id)?;
    let mut hash = Sha256::new();
    hash.update(b"drasi.output-dataset.v1");
    for part in [
        construction_scope,
        graph_id,
        component.as_str(),
        port.as_str(),
    ] {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part.as_bytes());
    }
    Ok(format!("drasi.output/{:x}", hash.finalize()))
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReplayRejection {
    #[error("output replay requires explicit persistent immediate-producer progress")]
    MissingProgress,
    #[error("output belongs to a different producer or query generation")]
    ProducerChanged,
    #[error("logical output {0} is outside the retained receipt window")]
    ReceiptExpired(u64),
    #[error("logical output {0} was already accepted with different content")]
    PayloadConflict(u64),
    #[error(
        "new logical output must advance transport sequence beyond {previous}, received {received}"
    )]
    TransportSequence { previous: u64, received: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Producer {
    Graph(GraphProducerIdentity),
    Query {
        identity: QueryRecoveryIdentity,
        generation: u64,
    },
}

impl Producer {
    pub fn validate(&self, stream: &StreamId) -> anyhow::Result<()> {
        let (scope, graph, component) = match self {
            Self::Graph(identity) => {
                identity.validate()?;
                if !identity.persistent() || identity.stream() != stream {
                    return Err(ReplayRejection::MissingProgress.into());
                }
                (
                    identity.construction_scope(),
                    identity.graph_id(),
                    identity.component_id().as_str(),
                )
            }
            Self::Query { identity, .. } => {
                identity.validate()?;
                if identity.incarnation().is_some() {
                    return Err(ReplayRejection::MissingProgress.into());
                }
                (
                    identity.construction_scope(),
                    identity.graph_id(),
                    identity.query_id().as_str(),
                )
            }
        };
        for value in [scope, graph, component] {
            if value.len() > 256 {
                return Err(AdmissionRejection::IdentifierTooLong.into());
            }
        }
        Ok(())
    }
}

pub struct Candidate {
    pub producer: Producer,
    pub sequence: u64,
    pub digest: [u8; 32],
    /// Present only for an at-least-once candidate derived from a volatile
    /// (in-memory) query: identifies the producer incarnation and generation
    /// whose sequence space this candidate belongs to. Strict candidates never
    /// carry an epoch.
    pub epoch: Option<String>,
}

impl Candidate {
    pub fn new(envelope: &ChangeEnvelope, codec: &EnvelopeCodec) -> anyhow::Result<Self> {
        let query = QueryChangeCodec::is_query_output(envelope);
        let progress = if query {
            GraphProducerProgress::from_query_envelope(envelope)
        } else {
            GraphProducerProgress::from_envelope(envelope)
        }?;
        let (producer, sequence) = match progress {
            Some(progress) => (
                Producer::Graph(progress.identity().clone()),
                progress.sequence(),
            ),
            None if query => {
                let (identity, generation, sequence) = query_output(envelope)?;
                (
                    Producer::Query {
                        identity,
                        generation,
                    },
                    sequence,
                )
            }
            None => return Err(ReplayRejection::MissingProgress.into()),
        };
        producer.validate(envelope.system().stream())?;
        Ok(Self {
            producer,
            sequence,
            digest: content_digest(envelope, codec, query)?,
            epoch: None,
        })
    }

    /// Explicit, weaker derivation for an immediate in-memory query output.
    ///
    /// Accepts only a live (non-snapshot, non-progress-only) query output whose
    /// recovery identity carries a volatile incarnation. The candidate's
    /// `epoch` identifies that incarnation and generation; the delivery stream
    /// key derived from it excludes both, so one destination cursor serves
    /// every run of the producer. This never replaces [`Candidate::new`]:
    /// persistent producers and every non-query producer are rejected here.
    pub fn new_volatile(envelope: &ChangeEnvelope, codec: &EnvelopeCodec) -> anyhow::Result<Self> {
        if !QueryChangeCodec::is_query_output(envelope) {
            return Err(ReplayRejection::MissingProgress.into());
        }
        if GraphProducerProgress::from_query_envelope(envelope)?
            .is_some_and(|progress| progress.identity().persistent())
        {
            anyhow::bail!("query output has persistent progress; use exactly-once delivery");
        }
        let (identity, generation, sequence) = query_output(envelope)?;
        let incarnation = identity
            .incarnation()
            .ok_or(ReplayRejection::MissingProgress)?;
        identity.validate()?;
        for value in [
            identity.construction_scope(),
            identity.graph_id(),
            identity.query_id().as_str(),
        ] {
            if value.len() > 256 {
                return Err(AdmissionRejection::IdentifierTooLong.into());
            }
        }
        anyhow::ensure!(
            sequence > 0,
            "volatile query output sequence must be positive"
        );
        let mut epoch = Sha256::new();
        epoch.update(b"drasi.delivery-epoch.v1");
        epoch.update(incarnation.as_bytes());
        epoch.update(generation.to_be_bytes());
        epoch.update(identity.configuration_hash().to_be_bytes());
        Ok(Self {
            producer: Producer::Query {
                identity,
                generation,
            },
            sequence,
            digest: content_digest(envelope, codec, true)?,
            epoch: Some(format!("{:x}", epoch.finalize())),
        })
    }
}

fn query_output(envelope: &ChangeEnvelope) -> anyhow::Result<(QueryRecoveryIdentity, u64, u64)> {
    let identity = QueryRecoveryIdentity::from_query_output(envelope)?;
    if QueryChangeCodec::is_snapshot(envelope)
        || QueryChangeCodec::is_progress_only(envelope)
        || &QueryChangeCodec::metadata(envelope)?.query_id != identity.query_id()
    {
        return Err(ReplayRejection::MissingProgress.into());
    }
    let sequence = QueryChangeCodec::output_sequence(envelope)?;
    let generation = QueryChangeCodec::query_generation(envelope)?;
    Ok((identity, generation, sequence))
}

fn content_digest(
    envelope: &ChangeEnvelope,
    codec: &EnvelopeCodec,
    query: bool,
) -> anyhow::Result<[u8; 32]> {
    // Immediate-query completion timings are added after its outbox commit.
    // Inherited timings remain immutable history.
    let mut normalized = envelope.reemit(1)?;
    if query {
        normalized = QueryChangeCodec::without_post_commit_profiling(&normalized)?;
    }
    Ok(Sha256::digest(codec.encode(&normalized)?).into())
}

/// True when `error` is the strict derivation's missing-progress rejection,
/// the only failure for which an at-least-once derivation may be attempted.
pub fn is_missing_progress(error: &anyhow::Error) -> bool {
    error.downcast_ref::<ReplayRejection>() == Some(&ReplayRejection::MissingProgress)
}

impl From<ReplayRejection> for crate::PipeError {
    fn from(error: ReplayRejection) -> Self {
        crate::PipeError::Backend(error.into())
    }
}

#[cfg(test)]
mod dataset_tests {
    use super::*;

    const QUERY: &str = "00112233-4455-4677-8899-aabbccddeeff";
    const OTHER_QUERY: &str = "11223344-5566-4788-99aa-bbccddeeff00";

    #[test]
    fn dataset_identity_is_stable_and_scoped_to_every_output_dimension() {
        let namespace = |scope: &str, graph: &str, component: &str, port: &str| {
            dataset_namespace(
                scope,
                graph,
                &ComponentId::try_new(component).unwrap(),
                &PortId::try_new(port).unwrap(),
            )
            .unwrap()
        };
        let first = namespace("instance", "graph", QUERY, "out");
        assert_eq!(first, namespace("instance", "graph", QUERY, "out"));
        for other in [
            namespace("another-instance", "graph", QUERY, "out"),
            namespace("instance", "another-graph", QUERY, "out"),
            namespace("instance", "graph", OTHER_QUERY, "out"),
            namespace("instance", "graph", QUERY, "another-out"),
        ] {
            assert_ne!(first, other);
        }
        assert_ne!(
            namespace("a/b", "c", QUERY, "out"),
            namespace("a", "b/c", QUERY, "out")
        );
        assert_eq!(first.len(), "drasi.output/".len() + 64);
    }

    #[test]
    fn missing_output_scope_is_not_replaced_with_a_default() {
        let component = ComponentId::try_new(QUERY).unwrap();
        let port = PortId::try_new("out").unwrap();
        assert!(dataset_namespace("", "graph", &component, &port).is_err());
        assert!(dataset_namespace("instance", "", &component, &port).is_err());
    }
}
