// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Typed sink delivery failures and the declared permanent-failure policy.
//!
//! A sink classifies why it could not deliver one input:
//!
//! - [`DeliveryFailureClass::Transient`]: the destination may accept the same
//!   input later (an outage, a timeout, throttling). The graph retries the same
//!   input in place with bounded exponential backoff, honouring an optional
//!   retry-after hint, and never skips it.
//! - [`DeliveryFailureClass::Permanent`]: this input can never be delivered
//!   (the destination rejected its content). The graph applies the sink's
//!   declared [`PermanentFailurePolicy`].
//! - [`DeliveryFailureClass::Skipped`]: this operation could never be
//!   delivered and the sink, whose declared policy is
//!   [`PermanentFailurePolicy::Skip`], has already settled it as skipped: any
//!   progress the destination keeps records the skip together with the
//!   failure, atomically. A host-owned delivery runner advances past such an
//!   operation and reports the input as permanently failed, so the graph
//!   counts the skip; anywhere else it is a permanent failure.
//!
//! An error without a [`DeliveryFailure`] in its chain is transient.

use std::{fmt, time::Duration};

use serde::{Deserialize, Serialize};

/// Why a sink could not deliver one input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "class", rename_all = "camelCase", deny_unknown_fields)]
pub enum DeliveryFailureClass {
    /// The same input may be delivered later.
    #[serde(rename_all = "camelCase")]
    Transient {
        /// The destination's requested minimum delay before the next attempt.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retry_after_ms: Option<u64>,
    },
    /// This input can never be delivered.
    Permanent,
    /// This operation can never be delivered and the sink has durably
    /// recorded it as skipped under its declared skip policy.
    Skipped,
}

impl DeliveryFailureClass {
    /// The destination's requested minimum delay, if any.
    pub fn retry_after(self) -> Option<Duration> {
        match self {
            Self::Transient { retry_after_ms } => retry_after_ms.map(Duration::from_millis),
            Self::Permanent | Self::Skipped => None,
        }
    }

    /// Whether the input can never be delivered (permanent or skipped).
    pub fn is_permanent(self) -> bool {
        matches!(self, Self::Permanent | Self::Skipped)
    }
}

/// A classified sink delivery failure. Carry it in the error a sink's
/// `handle` returns; the graph finds it anywhere in the error chain.
#[derive(Debug)]
pub struct DeliveryFailure {
    class: DeliveryFailureClass,
    source: anyhow::Error,
}

impl DeliveryFailure {
    pub fn new(class: DeliveryFailureClass, source: impl Into<anyhow::Error>) -> Self {
        Self {
            class,
            source: source.into(),
        }
    }

    /// The same input may succeed later.
    pub fn transient(source: impl Into<anyhow::Error>) -> Self {
        Self::new(
            DeliveryFailureClass::Transient {
                retry_after_ms: None,
            },
            source,
        )
    }

    /// The same input may succeed later, but not before `delay`.
    pub fn transient_after(source: impl Into<anyhow::Error>, delay: Duration) -> Self {
        Self::new(
            DeliveryFailureClass::Transient {
                retry_after_ms: Some(u64::try_from(delay.as_millis()).unwrap_or(u64::MAX)),
            },
            source,
        )
    }

    /// This input can never be delivered.
    pub fn permanent(source: impl Into<anyhow::Error>) -> Self {
        Self::new(DeliveryFailureClass::Permanent, source)
    }

    /// This operation can never be delivered and the sink, declaring
    /// [`PermanentFailurePolicy::Skip`], has already recorded the skip with
    /// any destination-side progress in one atomic step.
    pub fn skipped(source: impl Into<anyhow::Error>) -> Self {
        Self::new(DeliveryFailureClass::Skipped, source)
    }

    pub fn class(&self) -> DeliveryFailureClass {
        self.class
    }
}

impl fmt::Display for DeliveryFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.class {
            DeliveryFailureClass::Transient { .. } => f.write_str("transient delivery failure"),
            DeliveryFailureClass::Permanent => f.write_str("permanent delivery failure"),
            DeliveryFailureClass::Skipped => f.write_str("skipped undeliverable operation"),
        }
    }
}

impl std::error::Error for DeliveryFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

/// The class of the first [`DeliveryFailure`] in `error`'s chain.
pub fn delivery_failure_class(error: &anyhow::Error) -> Option<DeliveryFailureClass> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<DeliveryFailure>())
        .map(DeliveryFailure::class)
}

/// What a sink's graph node does with an input that failed permanently.
/// Every sink exposes it as its `onPermanentFailure` setting and declares it
/// in its component description.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PermanentFailurePolicy {
    /// Record a terminal failure on the node and deliver nothing more until
    /// an operator acts.
    #[default]
    Stall,
    /// Advance past the input, log it and count it on the node.
    Skip,
}

impl PermanentFailurePolicy {
    /// The JSON values a sink configuration accepts, for schema publication.
    pub const VALUES: [&'static str; 2] = ["stall", "skip"];
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification_is_found_through_context_and_survives_the_wire() {
        let error = anyhow::Error::new(DeliveryFailure::transient_after(
            anyhow::anyhow!("throttled"),
            Duration::from_secs(3),
        ))
        .context("deliver output");
        let class = delivery_failure_class(&error).expect("classified");
        assert_eq!(class.retry_after(), Some(Duration::from_secs(3)));
        assert_eq!(
            format!("{error:#}"),
            "deliver output: transient delivery failure: throttled"
        );
        let wire = serde_json::to_value(class).unwrap();
        assert_eq!(
            wire,
            serde_json::json!({"class": "transient", "retryAfterMs": 3000})
        );
        assert_eq!(
            serde_json::from_value::<DeliveryFailureClass>(wire).unwrap(),
            class
        );
        assert_eq!(
            serde_json::to_value(DeliveryFailureClass::Permanent).unwrap(),
            serde_json::json!({"class": "permanent"})
        );
        assert_eq!(
            serde_json::to_value(DeliveryFailureClass::Skipped).unwrap(),
            serde_json::json!({"class": "skipped"})
        );
        assert!(DeliveryFailureClass::Skipped.is_permanent());
        assert_eq!(
            delivery_failure_class(&anyhow::anyhow!("unclassified")),
            None
        );
    }
}
