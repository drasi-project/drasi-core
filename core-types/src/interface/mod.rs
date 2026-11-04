// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Data and errors exchanged with providers; no storage implementation.

mod checkpoint;
mod checkpoint_store;
pub mod durability;
mod element_index;
mod future_queue;
mod future_reference;
mod index_set;
mod live_results_writer;
mod middleware_error;
mod outbox_writer;
mod result_index;
mod session_control;
mod source_middleware;
pub use checkpoint::SourceCheckpoint;
pub use checkpoint_store::CheckpointStore;
pub use durability::*;
pub use element_index::{
    index_joins_by_label, ElementArchiveIndex, ElementIndex, ElementResult, ElementStream,
    IndexJoin, IndexJoinsByLabel,
};
pub use future_queue::{FutureQueue, FutureQueueConsumer, PushType};
pub use future_reference::FutureElementRef;
pub use index_set::IndexSet;
pub use live_results_writer::{LiveResultsWriter, RowMutation};
pub use middleware_error::{MiddlewareError, MiddlewareSetupError};
pub use outbox_writer::{OutboxPageLimits, OutboxWriter};
pub use result_index::{
    AccumulatorIndex, LazySortedSetStore, ResultIndex, ResultKey, ResultOwner, ResultSequence,
    ResultSequenceCounter, ValueAccumulator,
};
pub use session_control::{NoOpSessionControl, SessionControl, SessionGuard};
pub use source_middleware::{SourceMiddleware, SourceMiddlewareFactory};
use std::{error::Error, fmt::Display};

#[derive(Debug)]
pub enum IndexError {
    IOError,
    NotSupported,
    /// A temporal read was attempted on an index whose archive is not enabled.
    ArchiveNotEnabled,
    CorruptedData,
    ConnectionFailed(Box<dyn std::error::Error + Send + Sync>),
    UnknownStore(String),
    Other(Box<dyn std::error::Error + Send + Sync>),
}

impl PartialEq for IndexError {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (IndexError::IOError, IndexError::IOError) => true,
            (IndexError::NotSupported, IndexError::NotSupported) => true,
            (IndexError::ArchiveNotEnabled, IndexError::ArchiveNotEnabled) => true,
            (IndexError::CorruptedData, IndexError::CorruptedData) => true,
            (IndexError::ConnectionFailed(a), IndexError::ConnectionFailed(b)) => {
                a.to_string() == b.to_string()
            }
            (IndexError::UnknownStore(a), IndexError::UnknownStore(b)) => a == b,
            (IndexError::Other(a), IndexError::Other(b)) => a.to_string() == b.to_string(),
            _ => false,
        }
    }
}
// impl<E: std::error::Error + 'static> From<E> for IndexError {
//   fn from(e: E) -> Self {
//     IndexError::Other(Box::new(e))
//   }
// }

impl Display for IndexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IndexError::ArchiveNotEnabled => write!(
                f,
                "archive index not enabled for this query; set enable_archive to use past()"
            ),
            _ => format!("{self:?}").fmt(f),
        }
    }
}

impl Error for IndexError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            IndexError::Other(e) => Some(e.as_ref()),
            _ => None,
        }
    }
}

impl IndexError {
    pub fn other<E: std::error::Error + Send + Sync + 'static>(e: E) -> Self {
        IndexError::Other(Box::new(e))
    }

    pub fn connection_failed<E: std::error::Error + Send + Sync + 'static>(e: E) -> Self {
        IndexError::ConnectionFailed(Box::new(e))
    }
}
