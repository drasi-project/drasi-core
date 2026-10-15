// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use crate::{Record, RecordId};

#[derive(Clone)]
pub struct QuerySnapshot {
    pub as_of_sequence: u64,
    pub generation: u64,
    pub rows: im::HashMap<RecordId, Record>,
}
