mod expressions;

pub mod context;
pub mod functions;
pub mod instant_query_clock;
pub mod parts;
pub mod temporal_constants;
pub mod variable_value;

use std::{error::Error, fmt::{self, Display}};

pub use context::ExpressionEvaluationContext;
pub use expressions::*;
pub use instant_query_clock::InstantQueryClock;
pub use parts::*;

use crate::interface::{IndexError, MiddlewareError};

#[derive(Debug)]
pub enum EvaluationError {
    DivideByZero,  
    InvalidType {
        expected: String,
    },
    UnknownIdentifier(String),
    UnknownFunction(String), // Unknown Cypher function 
    IndexError(IndexError),
    MiddlewareError(MiddlewareError),
    ParseError,
    InvalidContext,
    OutOfRange {
        kind: OutOfRangeType,
    },  
    OverflowError,
    FunctionError(FunctionError),
    CorruptData,
    InvalidArgument,
    UnknownProperty {
        property_name: String,
    },
}


#[derive(Debug)]
pub struct FunctionError {
    function_name: String,
    error: FunctionEvaluationError,
}

impl PartialEq for FunctionError {
    fn eq(&self, other: &Self) -> bool {
        self.error == other.error
    }
}


#[derive(Debug)]
pub enum FunctionEvaluationError {
    InvalidArgument(usize),
    InvalidArgumentCount,
    IndexError(IndexError),
    OverflowError,
    OutofRange,
    InvalidFormat {
        expected: String,
    },
    CorruptData,
    InvalidType {
        expected: String,
    },
}

impl PartialEq for FunctionEvaluationError {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (FunctionEvaluationError::InvalidArgument(a), FunctionEvaluationError::InvalidArgument(b)) => a == b,
            (FunctionEvaluationError::InvalidArgumentCount, FunctionEvaluationError::InvalidArgumentCount) => true,
            (FunctionEvaluationError::IndexError(a), FunctionEvaluationError::IndexError(b)) => a == b,
            (FunctionEvaluationError::OverflowError, FunctionEvaluationError::OverflowError) => true,
            (FunctionEvaluationError::OutofRange, FunctionEvaluationError::OutofRange) => true,
            (FunctionEvaluationError::InvalidFormat { .. }, FunctionEvaluationError::InvalidFormat { .. }) => true,
            (FunctionEvaluationError::CorruptData, FunctionEvaluationError::CorruptData) => true,
            (FunctionEvaluationError::InvalidType { .. }, FunctionEvaluationError::InvalidType { .. }) => true,
            _ => false,
        }
    }
}


impl fmt::Display for FunctionEvaluationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FunctionEvaluationError::InvalidArgument(arg) => write!(f, "Invalid argument: {}", arg),
            FunctionEvaluationError::InvalidArgumentCount => write!(f, "Invalid argument count"),
            FunctionEvaluationError::IndexError(err) => write!(f, "Index error: {}", err),
            FunctionEvaluationError::OverflowError => write!(f, "Overflow error"),
            FunctionEvaluationError::OutofRange => write!(f, "Out of range"),
            FunctionEvaluationError::InvalidFormat { expected } => write!(f, "Invalid format, expected: {}", expected),
            FunctionEvaluationError::CorruptData => write!(f, "Invalid accumulator"),
            FunctionEvaluationError::InvalidType { expected } => write!(f, "Invalid type, expected: {}", expected),
        }
    }
}


#[derive(Debug)]
pub enum OutOfRangeType {
    IndexOutOfRange,
    TemporalDurationOutOfRange,
    TemporalInstantOutOfRange,
}

impl From<IndexError> for FunctionEvaluationError {
    fn from(e: IndexError) -> Self {
        FunctionEvaluationError::IndexError(e)
    }
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
    // Match the variant and handle the display differently for each variant
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        format!("{:?}", self).fmt(f)
    }
}

impl Error for EvaluationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        None
    }
}
