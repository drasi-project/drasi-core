// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Typed permanent failures of change-data-capture sources.
//!
//! Each is permanent for the current activation: reconnecting cannot cure it,
//! so retry loops stop on it and the host reports it. A source family wraps
//! its own detail text; hosts and operators classify failures by variant.

use std::fmt;

/// A permanent change-data-capture source failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceFailure {
    /// A selected table, its key, its identity settings or the change feed
    /// configuration changed incompatibly with the committed cursor. Removing
    /// the source with erase and adding it again rebuilds from a new snapshot.
    SchemaChanged(String),
    /// The committed cursor is no longer retained by the database: the change
    /// history was purged, a replication slot or capture instance is gone, or
    /// a log position expired.
    PositionUnavailable(String),
    /// The server or its configuration cannot be used by this source.
    Unsupported(String),
    /// State the source owns (for example a replication slot) is in use by
    /// another connection, so it cannot be removed now.
    StateInUse(String),
}

impl fmt::Display for SourceFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SchemaChanged(detail) => write!(
                f,
                "source schema changed incompatibly ({detail}); remove the source with erase and add it again to rebuild"
            ),
            Self::PositionUnavailable(detail) => write!(
                f,
                "source position is no longer available ({detail}); explicit recovery is required"
            ),
            Self::Unsupported(detail) => write!(f, "unsupported source configuration: {detail}"),
            Self::StateInUse(detail) => write!(f, "source-owned state is in use: {detail}"),
        }
    }
}

impl std::error::Error for SourceFailure {}

/// Whether `error`'s chain contains a [`SourceFailure`].
pub fn is_permanent_source_failure(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| cause.is::<SourceFailure>())
}

/// The first [`SourceFailure`] in `error`'s chain.
pub fn source_failure(error: &anyhow::Error) -> Option<&SourceFailure> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<SourceFailure>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failures_are_found_anywhere_in_the_chain() {
        let error =
            anyhow::Error::new(SourceFailure::StateInUse("slot".into())).context("dispose failed");
        assert!(is_permanent_source_failure(&error));
        assert_eq!(
            source_failure(&error),
            Some(&SourceFailure::StateInUse("slot".into()))
        );
        assert!(!is_permanent_source_failure(&anyhow::anyhow!("io")));
    }
}
