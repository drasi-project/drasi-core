// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use crate::observability::ExecutionContext;
use std::sync::Arc;

/// Transient queue metadata. Neither field changes the payload's wire format.
#[derive(Clone, Debug)]
pub struct ChangeMessage<T> {
    pub payload: Arc<T>,
    pub execution_context: ExecutionContext,
}

impl<T> ChangeMessage<T> {
    pub fn new(payload: Arc<T>, execution_context: ExecutionContext) -> Self {
        Self {
            payload,
            execution_context,
        }
    }

    pub fn capture(payload: Arc<T>) -> Self {
        Self::new(payload, ExecutionContext::current())
    }
}

impl<T: super::Timestamped> super::Timestamped for ChangeMessage<T> {
    fn timestamp(&self) -> chrono::DateTime<chrono::Utc> {
        self.payload.timestamp()
    }

    fn ordering_stream(&self) -> Option<(&str, u64)> {
        self.payload.ordering_stream()
    }

    fn ordering_tie_breaker(&self) -> Option<(usize, u64)> {
        self.payload.ordering_tie_breaker()
    }
}
