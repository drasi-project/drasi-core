// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{collections::VecDeque, num::NonZeroUsize};

use super::{BinaryEnvelopeCodec, ChangeEnvelope, PipeError};

pub(super) struct JournalBudget {
    maximum: NonZeroUsize,
    sizes: VecDeque<usize>,
    used: usize,
}

pub(super) struct JournalAppend {
    pub remove: usize,
    size: usize,
    used: usize,
}

impl JournalBudget {
    pub fn new(maximum: NonZeroUsize) -> Self {
        Self {
            maximum,
            sizes: VecDeque::new(),
            used: 0,
        }
    }

    pub fn restore(&mut self, envelope: &ChangeEnvelope) -> Result<(), PipeError> {
        let size = BinaryEnvelopeCodec::encoded_size(envelope)
            .map_err(|error| PipeError::Backend(error.into()))?;
        self.used = self.used.checked_add(size).ok_or_else(|| {
            PipeError::Backend(anyhow::anyhow!("journal byte accounting overflow"))
        })?;
        self.sizes.push_back(size);
        Ok(())
    }

    /// Plan without discarding anything; the caller must first prove that every
    /// removed position is acknowledged, or explicitly authorize lossy pruning.
    pub fn prepare(
        &self,
        envelope: &ChangeEnvelope,
        minimum_remove: usize,
    ) -> Result<JournalAppend, PipeError> {
        let size = BinaryEnvelopeCodec::encoded_size(envelope)
            .map_err(|error| PipeError::Backend(error.into()))?;
        let mut used = self.used;
        let mut remove = 0;
        for old in &self.sizes {
            if remove >= minimum_remove
                && used <= self.maximum.get()
                && size <= self.maximum.get() - used
            {
                break;
            }
            used -= old;
            remove += 1;
        }
        // Exhausting the retained window permits one oversized envelope alone.
        let used = used.checked_add(size).ok_or_else(|| {
            PipeError::Backend(anyhow::anyhow!("journal byte accounting overflow"))
        })?;
        Ok(JournalAppend { remove, size, used })
    }

    pub fn retain_last(&mut self, count: usize) {
        while self.sizes.len() > count {
            self.used -= self.sizes.pop_front().expect("nonempty journal accounting");
        }
    }

    /// Apply only after acceptance/commit, under the same journal-state lock
    /// used to prepare the append.
    pub fn apply(&mut self, append: JournalAppend) {
        for _ in 0..append.remove {
            self.sizes.pop_front();
        }
        self.sizes.push_back(append.size);
        self.used = append.used;
    }
}
