// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Transient query instrumentation, separate from output/recovery observations.

use super::{ExecutionContext, Result, TelemetryError, TelemetryRecord, TraceContextV1};
use serde::{Deserialize, Serialize};

/// Collection phase, not a component lifecycle or durable processing marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum QueryObservationPhase {
    Steady,
    Bootstrap,
}

/// Allowlisted authoritative query boundaries. Plugins cannot choose metric names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum QueryObservationKind {
    /// Attribution scope for the host-owned transaction; not a timed phase/span.
    Transaction,
    /// Marks real transaction-owned work without timing the plugin's whole call.
    CommitWork,
    Evaluate,
    PrepareInput,
    PrepareOutput,
    Finalize,
    Bootstrap,
    FutureQueue,
    OutputReplay,
    InputReplay,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueryObservationRequest {
    pub kind: QueryObservationKind,
    pub phase: QueryObservationPhase,
    pub parent: Option<TraceContextV1>,
    pub change_count: Option<u64>,
}

impl TelemetryRecord for QueryObservationRequest {
    fn validate(&self) -> Result<()> {
        if self
            .change_count
            .is_some_and(|count| count > i64::MAX as u64)
        {
            return Err(TelemetryError::Invalid(
                "query observation count exceeds the supported range".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum QueryObservationOutcome {
    Success,
    Error,
    Cancelled,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueryObservationResult {
    pub outcome: QueryObservationOutcome,
    pub result_count: Option<u64>,
}

impl TelemetryRecord for QueryObservationResult {
    fn validate(&self) -> Result<()> {
        if self
            .result_count
            .is_some_and(|count| count > i64::MAX as u64)
        {
            return Err(TelemetryError::Invalid(
                "query result count exceeds the supported range".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum QueryObservationEvent {
    Input {
        phase: QueryObservationPhase,
    },
    Results {
        added: u64,
        updated: u64,
        deleted: u64,
    },
    State {
        outbox: u64,
        live: u64,
    },
}

impl TelemetryRecord for QueryObservationEvent {
    fn validate(&self) -> Result<()> {
        let valid = match *self {
            Self::Input { .. } => true,
            Self::Results {
                added,
                updated,
                deleted,
            } => added
                .checked_add(updated)
                .and_then(|value| value.checked_add(deleted))
                .is_some_and(|value| value <= i64::MAX as u64),
            Self::State { outbox, live } => outbox <= i64::MAX as u64 && live <= i64::MAX as u64,
        };
        if !valid {
            return Err(TelemetryError::Invalid(
                "query observation values exceed the supported range".into(),
            ));
        }
        Ok(())
    }
}

/// Local Rust integration only. Native SDK wrappers use opaque C handles and
/// synchronous callbacks; no trait object crosses the shared-library boundary.
pub trait QueryObservation: Send + Sync {
    fn context(&self) -> ExecutionContext;
    fn in_scope(&self, work: &mut dyn FnMut()) -> Result<()>;
    fn finish(&mut self, result: QueryObservationResult) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_counts_reject_overflow_before_crossing_the_native_boundary() {
        let mut request = QueryObservationRequest {
            kind: QueryObservationKind::Evaluate,
            phase: QueryObservationPhase::Steady,
            parent: None,
            change_count: Some(i64::MAX as u64),
        };
        assert!(request.validate().is_ok());
        request.change_count = Some(u64::MAX);
        assert!(request.validate().is_err());
        assert!(QueryObservationEvent::Results {
            added: u64::MAX,
            updated: 1,
            deleted: 0,
        }
        .validate()
        .is_err());
        assert!(QueryObservationEvent::State {
            outbox: 0,
            live: u64::MAX
        }
        .validate()
        .is_err());
        assert!(QueryObservationResult {
            outcome: QueryObservationOutcome::Success,
            result_count: Some(u64::MAX),
        }
        .validate()
        .is_err());
    }

    #[test]
    fn query_requests_have_no_identity_or_arbitrary_metric_name_override() -> Result<()> {
        let request = QueryObservationRequest {
            kind: QueryObservationKind::PrepareOutput,
            phase: QueryObservationPhase::Steady,
            parent: None,
            change_count: None,
        };
        let mut encoded = serde_json::to_value(request)
            .map_err(|error| TelemetryError::Invalid(error.to_string()))?;
        encoded["metric_name"] = "drasi.component.up".into();
        assert!(serde_json::from_value::<QueryObservationRequest>(encoded.clone()).is_err());
        encoded
            .as_object_mut()
            .expect("query request object")
            .remove("metric_name");
        encoded["component_id"] = "another-component".into();
        assert!(serde_json::from_value::<QueryObservationRequest>(encoded).is_err());
        Ok(())
    }
}
