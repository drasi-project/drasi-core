// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Wire records for native output reads. Values reuse the shared graph/record
//! codecs: records carry their schema-encoded payload unchanged and replayed
//! envelopes are the binary computation envelope frames. Provider results,
//! including typed provider errors, are an [`Outcome`] in a successful reply;
//! lifecycle and transport failures use ABI status codes.

use std::{borrow::Cow, num::NonZeroUsize, time::Duration};

use drasi_computation_contracts::{
    GraphProducerIdentity, OutputCapability, OutputCursor, OutputEndpoint, OutputFollowLimits,
    OutputReadError, SchemaDescriptor,
};
use serde::{Deserialize, Serialize};

/// A snapshot or replay larger than the negotiated bounded representation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputReadLimit(pub String);

impl std::fmt::Display for OutputReadLimit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "native output read limit exceeded: {}", self.0)
    }
}
impl std::error::Error for OutputReadLimit {}

/// A committed logical position: the producer epoch and its logical sequence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireCursor {
    pub producer: GraphProducerIdentity,
    pub sequence: u64,
}

impl WireCursor {
    pub fn new(cursor: &OutputCursor) -> Self {
        Self {
            producer: cursor.producer().clone(),
            sequence: cursor.sequence(),
        }
    }

    pub fn into_cursor(self) -> Result<OutputCursor, OutputReadError> {
        OutputCursor::try_new(self.producer, self.sequence)
    }
}

/// One full-image record. `image` is 0 for a full image; any other value is
/// rejected for a snapshot.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireRecord<'a> {
    #[serde(borrow)]
    pub namespace: Cow<'a, str>,
    #[serde(borrow, with = "serde_bytes")]
    pub identity: Cow<'a, [u8]>,
    pub image: u32,
    #[serde(borrow, with = "serde_bytes")]
    pub payload: Cow<'a, [u8]>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireSnapshot<'a> {
    pub cursor: WireCursor,
    pub dataset: String,
    pub schema: SchemaDescriptor,
    #[serde(borrow)]
    pub records: Vec<WireRecord<'a>>,
}

/// One binary computation envelope, encoded by the plugin's registered codec.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireFrame<'a> {
    #[serde(borrow, with = "serde_bytes")]
    pub envelope: Cow<'a, [u8]>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireReplay<'a> {
    pub after: WireCursor,
    pub head: WireCursor,
    #[serde(borrow)]
    pub envelopes: Vec<WireFrame<'a>>,
}

/// Bounds of one non-renewing snapshot/live lease, as nonzero counts and a
/// positive lifetime. Converted to and validated as `OutputFollowLimits`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireFollowLimits {
    pub max_envelopes: u64,
    pub max_bytes: u64,
    pub lifetime_nanos: u64,
}

impl WireFollowLimits {
    pub fn new(limits: &OutputFollowLimits) -> Result<Self, OutputReadError> {
        let invalid = || OutputReadError::invalid("output follow limits exceed the wire range");
        Ok(Self {
            max_envelopes: u64::try_from(limits.max_envelopes.get()).map_err(|_| invalid())?,
            max_bytes: u64::try_from(limits.max_bytes.get()).map_err(|_| invalid())?,
            lifetime_nanos: u64::try_from(limits.lifetime.as_nanos()).map_err(|_| invalid())?,
        })
    }

    pub fn into_limits(self) -> Result<OutputFollowLimits, OutputReadError> {
        let invalid = |message: &str| OutputReadError::invalid(message.to_owned());
        let limits = OutputFollowLimits {
            max_envelopes: usize::try_from(self.max_envelopes)
                .ok()
                .and_then(NonZeroUsize::new)
                .ok_or_else(|| invalid("invalid output follow envelope bound"))?,
            max_bytes: usize::try_from(self.max_bytes)
                .ok()
                .and_then(NonZeroUsize::new)
                .ok_or_else(|| invalid("invalid output follow byte bound"))?,
            lifetime: Duration::from_nanos(self.lifetime_nanos),
        };
        limits.validate()?;
        Ok(limits)
    }
}

/// Request for the next lease item. `acknowledged` is the index of the last item
/// the holder observed; a later item is retained by the producer until it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireNext {
    pub acknowledged: u64,
}

/// One live item. `index` counts delivered items from 1 within the lease.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireFollowItem<'a> {
    pub index: u64,
    #[serde(borrow, with = "serde_bytes")]
    pub envelope: Cow<'a, [u8]>,
}

/// A provider's complete answer. Only `Ok` carries data; every other variant is
/// the corresponding typed [`OutputReadError`].
#[derive(Debug, Serialize, Deserialize)]
pub enum Outcome<T> {
    Ok(T),
    NotReady(String),
    Gap {
        requested: u64,
        oldest: Option<u64>,
        latest: u64,
    },
    EpochChanged {
        requested: GraphProducerIdentity,
        current: GraphProducerIdentity,
    },
    Retired,
    LeaseExpired,
    /// The provider reports its own output as unknown; the reader names it.
    UnknownOutput,
    UnknownComponent,
    /// The provider reports its own capability as absent; the reader names it.
    Absent,
    Invalid(String),
    Failed(String),
    Limit(String),
}

impl<T> Outcome<T> {
    pub fn from_result(result: Result<T, OutputReadError>) -> Self {
        match result {
            Ok(value) => Self::Ok(value),
            Err(OutputReadError::NotReady(message)) => Self::NotReady(message),
            Err(OutputReadError::Gap {
                requested,
                oldest,
                latest,
            }) => Self::Gap {
                requested,
                oldest,
                latest,
            },
            Err(OutputReadError::EpochChanged { requested, current }) => Self::EpochChanged {
                requested: *requested,
                current: *current,
            },
            Err(OutputReadError::Retired) => Self::Retired,
            Err(OutputReadError::LeaseExpired) => Self::LeaseExpired,
            Err(OutputReadError::UnknownOutput(_)) => Self::UnknownOutput,
            Err(OutputReadError::UnknownComponent(_)) => Self::UnknownComponent,
            Err(OutputReadError::Absent { .. }) => Self::Absent,
            Err(OutputReadError::Invalid(message)) => Self::Invalid(message),
            Err(OutputReadError::Failed(error)) => match error.downcast_ref::<OutputReadLimit>() {
                Some(limit) => Self::Limit(limit.0.clone()),
                None => Self::Failed(format!("{error:#}")),
            },
        }
    }

    /// `endpoint` and `capability` name the output that produced this outcome.
    pub fn into_result(
        self,
        endpoint: &OutputEndpoint,
        capability: OutputCapability,
    ) -> Result<T, OutputReadError> {
        match self {
            Self::Ok(value) => Ok(value),
            Self::NotReady(message) => Err(OutputReadError::NotReady(message)),
            Self::Gap {
                requested,
                oldest,
                latest,
            } => Err(OutputReadError::Gap {
                requested,
                oldest,
                latest,
            }),
            Self::EpochChanged { requested, current } => Err(OutputReadError::EpochChanged {
                requested: Box::new(requested),
                current: Box::new(current),
            }),
            Self::Retired => Err(OutputReadError::Retired),
            Self::LeaseExpired => Err(OutputReadError::LeaseExpired),
            Self::UnknownOutput => Err(OutputReadError::UnknownOutput(endpoint.clone())),
            Self::UnknownComponent => Err(OutputReadError::UnknownComponent(
                endpoint.component.clone(),
            )),
            Self::Absent => Err(OutputReadError::Absent {
                endpoint: endpoint.clone(),
                capability,
            }),
            Self::Invalid(message) => Err(OutputReadError::Invalid(message)),
            Self::Failed(message) => Err(OutputReadError::Failed(anyhow::anyhow!(message))),
            Self::Limit(message) => Err(OutputReadError::Failed(OutputReadLimit(message).into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_computation_contracts::{ComponentId, PortId, StreamId};

    fn producer() -> GraphProducerIdentity {
        GraphProducerIdentity::new(
            "scope".into(),
            "graph".into(),
            ComponentId::try_new("component").unwrap(),
            StreamId::try_new("component/out").unwrap(),
            true,
        )
        .unwrap()
    }

    fn endpoint() -> OutputEndpoint {
        OutputEndpoint::new(
            ComponentId::try_new("component").unwrap(),
            PortId::try_new("out").unwrap(),
        )
    }

    #[test]
    fn typed_errors_survive_the_wire_unchanged() {
        let requested = producer();
        let current = producer();
        let errors = [
            OutputReadError::NotReady("loading".into()),
            OutputReadError::Gap {
                requested: 3,
                oldest: Some(5),
                latest: 9,
            },
            OutputReadError::EpochChanged {
                requested: Box::new(requested.clone()),
                current: Box::new(current.clone()),
            },
            OutputReadError::Retired,
            OutputReadError::invalid("bad"),
        ];
        for error in errors {
            let bytes = crate::wire::encode(&Outcome::<u64>::from_result(Err(error))).unwrap();
            let outcome: Outcome<u64> = crate::wire::decode(&bytes).unwrap();
            let restored = outcome
                .into_result(&endpoint(), OutputCapability::Replay)
                .unwrap_err();
            match restored {
                OutputReadError::NotReady(message) => assert_eq!(message, "loading"),
                OutputReadError::Gap {
                    requested,
                    oldest,
                    latest,
                } => assert_eq!((requested, oldest, latest), (3, Some(5), 9)),
                OutputReadError::EpochChanged {
                    requested: got_requested,
                    current: got_current,
                } => assert_eq!(
                    (*got_requested, *got_current),
                    (requested.clone(), current.clone())
                ),
                OutputReadError::Retired => {}
                OutputReadError::Invalid(message) => assert_eq!(message, "bad"),
                other => panic!("unexpected error {other:?}"),
            }
        }
    }

    #[test]
    fn limits_and_failures_remain_distinct_and_never_become_success() {
        let limit = Outcome::<u64>::from_result(Err(OutputReadError::Failed(
            OutputReadLimit("too large".into()).into(),
        )));
        assert!(matches!(limit, Outcome::Limit(_)));
        let error = limit
            .into_result(&endpoint(), OutputCapability::Snapshot)
            .unwrap_err();
        let OutputReadError::Failed(error) = error else {
            panic!("limit must be an explicit failure")
        };
        assert!(error.downcast_ref::<OutputReadLimit>().is_some());
        assert!(matches!(
            Outcome::<u64>::from_result(Err(OutputReadError::Failed(anyhow::anyhow!("boom")))),
            Outcome::Failed(_)
        ));
    }

    #[test]
    fn provider_absence_is_attributed_to_the_reading_output() {
        let error = Outcome::<u64>::Absent
            .into_result(&endpoint(), OutputCapability::Replay)
            .unwrap_err();
        assert!(matches!(
            error,
            OutputReadError::Absent {
                capability: OutputCapability::Replay,
                ..
            }
        ));
    }
}
