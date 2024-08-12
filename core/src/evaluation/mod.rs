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
    InvalidArgumentCount(String), 
    IndexError(IndexError),
    MiddlewareError(MiddlewareError),
    ParseError,
    InvalidContext,
    OutOfRange {
        kind: OutOfRangeType,
    },  
    OverflowError,
    FunctionError(FunctionError),
    InvalidState,
    InvalidArgument,
    InvalidExpression,
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
    InvalidDateFormat,
    InvalidLocalTimeFormat,
    InvalidTimeFormat,
    InvalidDateTimeFormat,
    InvalidLocalDateTimeFormat,
    InvalidDurationFormat,
    InvalidAccumulator,
    InvalidState,
    InvalidClock,
    InvalidAssignmentExpression,
    InvalidReduceExpression,
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
            (FunctionEvaluationError::InvalidDateFormat, FunctionEvaluationError::InvalidDateFormat) => true,
            (FunctionEvaluationError::InvalidLocalTimeFormat, FunctionEvaluationError::InvalidLocalTimeFormat) => true,
            (FunctionEvaluationError::InvalidTimeFormat, FunctionEvaluationError::InvalidTimeFormat) => true,
            (FunctionEvaluationError::InvalidDateTimeFormat, FunctionEvaluationError::InvalidDateTimeFormat) => true,
            (FunctionEvaluationError::InvalidLocalDateTimeFormat, FunctionEvaluationError::InvalidLocalDateTimeFormat) => true,
            (FunctionEvaluationError::InvalidDurationFormat, FunctionEvaluationError::InvalidDurationFormat) => true,
            (FunctionEvaluationError::InvalidAccumulator, FunctionEvaluationError::InvalidAccumulator) => true,
            (FunctionEvaluationError::InvalidState, FunctionEvaluationError::InvalidState) => true,
            (FunctionEvaluationError::InvalidClock, FunctionEvaluationError::InvalidClock) => true,
            (FunctionEvaluationError::InvalidAssignmentExpression, FunctionEvaluationError::InvalidAssignmentExpression) => true,
            (FunctionEvaluationError::InvalidReduceExpression, FunctionEvaluationError::InvalidReduceExpression) => true,
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
            FunctionEvaluationError::InvalidDateFormat => write!(f, "Invalid date format"),
            FunctionEvaluationError::InvalidLocalTimeFormat => write!(f, "Invalid local time format"),
            FunctionEvaluationError::InvalidTimeFormat => write!(f, "Invalid time format"),
            FunctionEvaluationError::InvalidDateTimeFormat => write!(f, "Invalid date-time format"),
            FunctionEvaluationError::InvalidLocalDateTimeFormat => write!(f, "Invalid local date-time format"),
            FunctionEvaluationError::InvalidDurationFormat => write!(f, "Invalid duration format"),
            FunctionEvaluationError::InvalidAccumulator => write!(f, "Invalid accumulator"),
            FunctionEvaluationError::InvalidState => write!(f, "Invalid state"),
            FunctionEvaluationError::InvalidClock => write!(f, "Invalid clock"),
            FunctionEvaluationError::InvalidAssignmentExpression => write!(f, "Invalid assignment expression"),
            FunctionEvaluationError::InvalidReduceExpression => write!(f, "Invalid reduce expression"),
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
