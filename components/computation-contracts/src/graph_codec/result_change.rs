// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use drasi_core_types::evaluation::context::{QueryPartEvaluationContext, QueryVariables};

use crate::QueryRowKind;

/// One non-`Noop` query result, borrowed from the evaluator's ordered output.
///
/// This is the only interpretation of `QueryPartEvaluationContext` used by the
/// graph result projector and the legacy typed query-result conversion. It
/// performs no encoding or I/O; `ordinal` is the result's position in the
/// original slice, so skipped `Noop` entries leave gaps.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ResultChange<'a> {
    Add {
        ordinal: usize,
        row_signature: u64,
        after: &'a QueryVariables,
    },
    Update {
        ordinal: usize,
        row_signature: u64,
        before: &'a QueryVariables,
        after: &'a QueryVariables,
    },
    Remove {
        ordinal: usize,
        row_signature: u64,
        before: &'a QueryVariables,
    },
    Aggregate {
        ordinal: usize,
        row_signature: u64,
        before: Option<&'a QueryVariables>,
        after: &'a QueryVariables,
        grouping_keys: &'a [String],
        default_before: bool,
        default_after: bool,
    },
}

impl ResultChange<'_> {
    pub(crate) fn ordinal(&self) -> usize {
        match self {
            Self::Add { ordinal, .. }
            | Self::Update { ordinal, .. }
            | Self::Remove { ordinal, .. }
            | Self::Aggregate { ordinal, .. } => *ordinal,
        }
    }

    pub(crate) fn row_signature(&self) -> u64 {
        match self {
            Self::Add { row_signature, .. }
            | Self::Update { row_signature, .. }
            | Self::Remove { row_signature, .. }
            | Self::Aggregate { row_signature, .. } => *row_signature,
        }
    }

    pub(crate) fn row_kind(&self) -> QueryRowKind {
        match self {
            Self::Aggregate {
                grouping_keys,
                default_before,
                default_after,
                ..
            } => QueryRowKind::Aggregation {
                grouping_keys: grouping_keys.to_vec(),
                default_before: *default_before,
                default_after: *default_after,
            },
            _ => QueryRowKind::Row,
        }
    }
}

/// Visit every non-`Noop` result in evaluator order.
pub(crate) fn result_changes(
    results: &[QueryPartEvaluationContext],
) -> impl Iterator<Item = ResultChange<'_>> + Clone {
    results
        .iter()
        .enumerate()
        .filter_map(|(ordinal, result)| match result {
            QueryPartEvaluationContext::Adding {
                after,
                row_signature,
            } => Some(ResultChange::Add {
                ordinal,
                row_signature: *row_signature,
                after,
            }),
            QueryPartEvaluationContext::Updating {
                before,
                after,
                row_signature,
            } => Some(ResultChange::Update {
                ordinal,
                row_signature: *row_signature,
                before,
                after,
            }),
            QueryPartEvaluationContext::Removing {
                before,
                row_signature,
            } => Some(ResultChange::Remove {
                ordinal,
                row_signature: *row_signature,
                before,
            }),
            QueryPartEvaluationContext::Aggregation {
                before,
                after,
                grouping_keys,
                default_before,
                default_after,
                row_signature,
            } => Some(ResultChange::Aggregate {
                ordinal,
                row_signature: *row_signature,
                before: before.as_ref(),
                after,
                grouping_keys,
                default_before: *default_before,
                default_after: *default_after,
            }),
            QueryPartEvaluationContext::Noop => None,
        })
}
