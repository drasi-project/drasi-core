// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use crate::computation::v1::{
    ChangeEnvelope, Delivery, InputEnvelope, PipeError, PortId, SinkBatching,
};
use std::{collections::VecDeque, time::Duration};
use tokio::time::Instant;

/// The owning receiver retains these leases even when receive_batch is dropped.
pub(crate) struct BatchBuffer {
    pub deliveries: VecDeque<Delivery>,
    pub after: u64,
    pub deadline: Instant,
    pub finished: bool,
    limits: SinkBatching,
    port: PortId,
    bytes: usize,
    operations: usize,
    barrier: bool,
}

impl BatchBuffer {
    pub fn new(limits: SinkBatching, port: PortId) -> Result<Self, PipeError> {
        limits.validate().map_err(PipeError::Backend)?;
        Ok(Self {
            deliveries: VecDeque::new(),
            after: 0,
            deadline: Instant::now() + Duration::from_millis(limits.max_delay_ms),
            finished: false,
            limits,
            port,
            bytes: 5,
            operations: 0,
            barrier: false,
        })
    }

    pub fn size(&self, envelope: &ChangeEnvelope) -> Result<usize, PipeError> {
        SinkBatching::input_size(&InputEnvelope {
            port: self.port.clone(),
            envelope: envelope.clone(),
        })
        .map_err(PipeError::Backend)
    }

    pub fn can_push(&self, envelope: &ChangeEnvelope, bytes: usize) -> bool {
        if self.deliveries.is_empty() {
            return true;
        }
        !self.full()
            && !self.is_barrier(envelope)
            && self.bytes.saturating_add(bytes) <= self.limits.max_bytes
            && self
                .operations
                .saturating_add(envelope.changes().operations().len())
                <= self.limits.max_operations
    }

    fn is_barrier(&self, envelope: &ChangeEnvelope) -> bool {
        SinkBatching::is_barrier(&InputEnvelope {
            port: self.port.clone(),
            envelope: envelope.clone(),
        })
    }

    pub fn push(&mut self, position: u64, delivery: Delivery, bytes: usize) {
        self.after = position;
        self.barrier |= self.is_barrier(delivery.envelope());
        self.bytes = self.bytes.saturating_add(bytes);
        self.operations = self
            .operations
            .saturating_add(delivery.envelope().changes().operations().len());
        self.deliveries.push_back(delivery);
    }

    pub fn full(&self) -> bool {
        self.finished
            || self.barrier
            || self.deliveries.len() >= self.limits.max_inputs
            || self.operations >= self.limits.max_operations
            || self.bytes >= self.limits.max_bytes
            || Instant::now() >= self.deadline
    }
}
