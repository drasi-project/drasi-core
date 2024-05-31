mod expressions;

pub mod context;
pub mod functions;
pub mod instant_query_clock;
pub mod phases;
pub mod temporal_constants;
pub mod variable_value;

use std::{error::Error, fmt::Display};

pub use context::ExpressionEvaluationContext;
pub use expressions::*;
pub use instant_query_clock::InstantQueryClock;
pub use phases::*;

use crate::interface::{IndexError, MiddlewareError};

#[derive(Debug)]
pub enum EvaluationError {
    DivideByZero,
    InvalidType,
    UnknownIdentifier(String),
    UnknownFunction(String),
    InvalidArgumentCount(String),
    IndexError(IndexError),
    MiddlewareError(MiddlewareError),
    ParseError,
    InvalidContext,
    OutOfRange,
    FunctionError {
        function_name: String,
        error: Box<EvaluationError>,
    },
    InvalidState,
}

impl From<IndexError> for EvaluationError {
    fn from(e: IndexError) -> Self {
        EvaluationError::IndexError(e)
    }
}

impl From<MiddlewareError> for EvaluationError {
    fn from(e: MiddlewareError) -> Self {
        EvaluationError::MiddlewareError(e)
    }
}

impl Display for EvaluationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Ok(format!("{:?}", self).fmt(f)?)
    }
}

impl Error for EvaluationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        None
    }
}
