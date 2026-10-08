// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Shared logical output identity and content comparison for replay and delivery.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    AdmissionRejection, ChangeEnvelope, EnvelopeCodec, GraphProducerIdentity,
    GraphProducerProgress, QueryChangeCodec, QueryRecoveryIdentity, StreamId,
};

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
pub(super) enum Producer {
    Graph(GraphProducerIdentity),
    Query {
        identity: QueryRecoveryIdentity,
        generation: u64,
    },
}

impl Producer {
    pub(super) fn validate(&self, stream: &StreamId) -> anyhow::Result<()> {
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

pub(super) struct Candidate {
    pub(super) producer: Producer,
    pub(super) sequence: u64,
    pub(super) digest: [u8; 32],
}

impl Candidate {
    pub(super) fn new(envelope: &ChangeEnvelope, codec: &EnvelopeCodec) -> anyhow::Result<Self> {
        let query = envelope.changes().schema() == QueryChangeCodec::schema().descriptor();
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
        if matches!(producer, Producer::Query { .. }) {
            normalized = QueryChangeCodec::without_post_commit_profiling(&normalized)?;
        }
        Ok(Self {
            producer,
            sequence,
            digest: Sha256::digest(codec.encode(&normalized)?).into(),
        })
    }
}
