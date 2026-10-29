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
                    identity.query_id(),
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
                let identity = QueryRecoveryIdentity::from_query_output(envelope)?;
                if QueryChangeCodec::is_snapshot(envelope)
                    || QueryChangeCodec::is_progress_only(envelope)
                    || QueryChangeCodec::metadata(envelope)?.query_id != identity.query_id()
                {
                    return Err(ReplayRejection::MissingProgress.into());
                }
                let sequence = QueryChangeCodec::output_sequence(envelope)?;
                let generation = QueryChangeCodec::query_generation(envelope)?;
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
        // Immediate-query completion timings are added after its outbox commit.
        // Inherited timings remain immutable history.
        let mut normalized = envelope.reemit(1)?;
        if query {
            normalized = QueryChangeCodec::without_post_commit_profiling(&normalized)?;
        }
        Ok(Self {
            producer,
            sequence,
            digest: Sha256::digest(codec.encode(&normalized)?).into(),
        })
    }
}

impl From<ReplayRejection> for crate::PipeError {
    fn from(error: ReplayRejection) -> Self {
        crate::PipeError::Backend(error.into())
    }
}

#[cfg(test)]
mod dataset_tests {
    use super::*;

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
        let first = namespace("instance", "graph", "query", "out");
        assert_eq!(first, namespace("instance", "graph", "query", "out"));
        for other in [
            namespace("another-instance", "graph", "query", "out"),
            namespace("instance", "another-graph", "query", "out"),
            namespace("instance", "graph", "another-query", "out"),
            namespace("instance", "graph", "query", "another-out"),
        ] {
            assert_ne!(first, other);
        }
        assert_ne!(
            namespace("a/b", "c", "query", "out"),
            namespace("a", "b/c", "query", "out")
        );
        assert_eq!(first.len(), "drasi.output/".len() + 64);
    }

    #[test]
    fn missing_output_scope_is_not_replaced_with_a_default() {
        let component = ComponentId::try_new("query").unwrap();
        let port = PortId::try_new("out").unwrap();
        assert!(dataset_namespace("", "graph", &component, &port).is_err());
        assert!(dataset_namespace("instance", "", &component, &port).is_err());
    }
}
