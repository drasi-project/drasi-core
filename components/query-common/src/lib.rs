// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

#![deny(unused)]

//! Existing query-component behavior shared by linked and native hosting adapters.

mod configuration;
mod definition;
mod execution;
mod publication;
pub mod query_output;
pub mod query_state;
mod recovery;
mod scheduling;

pub use configuration::{
    parser, query_builder, query_input_scope, query_input_scopes, validate_middleware_configs,
    QueryExecutionSettingsExt, QueryInputScopes,
};
pub use definition::{query_descriptor, ContinuousQueryDefinition};
pub use execution::{
    prepare_query_input, project_due_query_output, PreparedQueryInput, QueryOperation,
};
pub use publication::{
    query_reset_key, QueryPublicationCheckpoint, QueryResetMarker, PENDING_QUERY_OUTPUT,
};
pub use query_output::{QueryOutputProvider, QueryOutputView, QUERY_OUTPUT_PORT};
pub use query_state::{
    QueryHistoryError, QueryOutputHead, QueryOutputOwner, QueryOutputState, QueryPublicationGuard,
    QueryRecoveryView, QueryResultSnapshot, QueryResults,
};
pub use recovery::{
    pending_query_outputs, should_reset_query_error, validate_query_recovery,
    validate_query_reset_owner,
};
pub use scheduling::{FutureWakeup, QuerySchedule};
