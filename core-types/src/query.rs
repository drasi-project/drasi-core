// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use crate::evaluation::variable_value::VariableValue;
use std::collections::BTreeMap;

pub type QueryVariables = BTreeMap<Box<str>, VariableValue>;

/// Projected changes carry MATCH identities until aggregation replaces them
/// with grouping identities. Removes address before; additions/updates address after.
#[derive(Debug, Clone, PartialEq)]
pub enum QueryPartEvaluationContext {
    Adding {
        after: QueryVariables,
        row_signature: u64,
    },
    Updating {
        before: QueryVariables,
        after: QueryVariables,
        row_signature: u64,
    },
    Removing {
        before: QueryVariables,
        row_signature: u64,
    },
    Aggregation {
        before: Option<QueryVariables>,
        after: QueryVariables,
        grouping_keys: Vec<String>,
        /// The before side is a snapshot/default candidate, not necessarily an
        /// already-applied contribution. Downstream PartCurrent decides whether
        /// to revert it; PartDefault remembers the baseline for later removal.
        /// A group-crossing destination snapshot may already be populated.
        default_before: bool,
        /// Reconcile the after side against downstream PartDefault before applying
        /// it. This does not by itself prove that the group has no contributors.
        /// Internal transitions survive even when terminal values are unchanged.
        default_after: bool,
        row_signature: u64,
    },
    Noop,
}

impl QueryPartEvaluationContext {
    pub fn row_signature(&self) -> u64 {
        match self {
            Self::Adding { row_signature, .. }
            | Self::Updating { row_signature, .. }
            | Self::Removing { row_signature, .. }
            | Self::Aggregation { row_signature, .. } => *row_signature,
            Self::Noop => 0,
        }
    }

    /// Compares all fields except `row_signature`. Used by test helpers to assert
    /// on result data without needing to predict internal hash values.
    pub fn data_eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Adding { after: a, .. }, Self::Adding { after: b, .. }) => a == b,
            (
                Self::Updating {
                    before: ab,
                    after: aa,
                    ..
                },
                Self::Updating {
                    before: bb,
                    after: ba,
                    ..
                },
            ) => ab == bb && aa == ba,
            (Self::Removing { before: a, .. }, Self::Removing { before: b, .. }) => a == b,
            (
                Self::Aggregation {
                    before: ab,
                    after: aa,
                    grouping_keys: ag,
                    default_before: adb,
                    default_after: ada,
                    ..
                },
                Self::Aggregation {
                    before: bb,
                    after: ba,
                    grouping_keys: bg,
                    default_before: bdb,
                    default_after: bda,
                    ..
                },
            ) => ab == bb && aa == ba && ag == bg && adb == bdb && ada == bda,
            (Self::Noop, Self::Noop) => true,
            _ => false,
        }
    }
}
