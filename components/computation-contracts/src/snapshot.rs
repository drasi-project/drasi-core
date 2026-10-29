// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use crate::{Record, RecordId, ResultNodeProjector};

/// Query result rows at one output sequence and generation.
///
/// This contains results, not the query text or component configuration.
#[derive(Clone)]
pub struct QueryResultSnapshot {
    /// Output scope, including for an empty snapshot.
    pub projector: ResultNodeProjector,
    /// Last output sequence represented by the rows.
    pub as_of_sequence: u64,
    /// Output generation associated with these rows.
    pub generation: u64,
    /// Rows keyed by their stable result identities.
    pub rows: im::HashMap<RecordId, Record>,
}
