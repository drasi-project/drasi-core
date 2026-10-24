// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use crate::models::{ElementReference, ElementTimestamp};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct FutureElementRef {
    pub element_ref: ElementReference,
    pub original_time: ElementTimestamp,
    pub due_time: ElementTimestamp,
    pub group_signature: u64,
}
