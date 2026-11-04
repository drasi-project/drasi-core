// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{fmt, sync::Arc};

use super::{ElementArchiveIndex, ElementIndex, FutureQueue, ResultIndex, SessionControl};

/// Indexes and session control sharing one underlying storage resource.
///
/// The bundle is a set of interfaces, not proof that its members participate
/// in the same transaction. The host must validate actual ownership separately.
pub struct IndexSet {
    /// Graph elements and their match-slot membership.
    pub element_index: Arc<dyn ElementIndex>,
    /// Historical elements for temporal reads.
    pub archive_index: Arc<dyn ElementArchiveIndex>,
    /// Accumulators and result sequences.
    pub result_index: Arc<dyn ResultIndex>,
    /// Scheduled future references.
    pub future_queue: Arc<dyn FutureQueue>,
    /// Session lifecycle for the underlying storage.
    pub session_control: Arc<dyn SessionControl>,
}

impl fmt::Debug for IndexSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IndexSet")
            .field("element_index", &"<trait object>")
            .field("archive_index", &"<trait object>")
            .field("result_index", &"<trait object>")
            .field("future_queue", &"<trait object>")
            .field("session_control", &"<trait object>")
            .finish()
    }
}
