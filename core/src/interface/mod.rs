mod element_index;
mod future_queue;
mod query_clock;
mod result_index;
mod source_middleware;

use std::error::Error;
use std::fmt::Display;

use drasi_query_ast::api::QueryParseError;
pub use element_index::ElementArchiveIndex;
pub use element_index::ElementIndex;
pub use element_index::ElementResult;
pub use element_index::ElementStream;
pub use future_queue::FutureElementRef;
pub use future_queue::FutureQueue;
pub use future_queue::FutureQueueConsumer;
pub use future_queue::PushType;
pub use query_clock::QueryClock;
pub use result_index::AccumulatorIndex;
pub use result_index::LazySortedSetStore;
pub use result_index::ResultIndex;
pub use result_index::ResultKey;
pub use result_index::ResultOwner;
pub use result_index::ResultSequence;
pub use result_index::ResultSequenceCounter;
pub use source_middleware::MiddlewareError;
pub use source_middleware::MiddlewareSetupError;
pub use source_middleware::SourceMiddleware;
pub use source_middleware::SourceMiddlewareFactory;
use thiserror::Error;

use crate::evaluation::EvaluationError;

#[derive(Debug)]
pub enum IndexError {
    IOError,
    NotSupported,
    CorruptedData,
    ConnectionFailed(Box<dyn std::error::Error + Send + Sync>),
    UnknownStore(String),
    Other(Box<dyn std::error::Error + Send + Sync>),
}

// impl<E: std::error::Error + 'static> From<E> for IndexError {
//   fn from(e: E) -> Self {
//     IndexError::Other(Box::new(e))
//   }
// }

impl Display for IndexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        format!("{:?}", self).fmt(f)
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

#[derive(Error, Debug)]
pub enum QueryBuilderError {
    #[error("Middleware setup error: {0}")]
    MiddlewareSetupError(MiddlewareSetupError),

    #[error("Parser error: {0}")]
    ParserError(QueryParseError),

    #[error("Evaluation error: {0}")]
    EvaluationError(EvaluationError),
}

impl From<MiddlewareSetupError> for QueryBuilderError {
    fn from(e: MiddlewareSetupError) -> Self {
        QueryBuilderError::MiddlewareSetupError(e)
    }
}

impl From<QueryParseError> for QueryBuilderError {
    fn from(e: QueryParseError) -> Self {
        QueryBuilderError::ParserError(e)
    }
}

impl From<EvaluationError> for QueryBuilderError {
    fn from(e: EvaluationError) -> Self {
        QueryBuilderError::EvaluationError(e)
    }
}
