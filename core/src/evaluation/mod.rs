mod expressions;

pub mod context;
pub mod functions;
pub mod instant_query_clock;
pub mod parts;
pub mod temporal_constants;
pub mod variable_value;

use std::{error::Error, fmt::Display};

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
    PropertyRetrievalError {
        property_name: String,
    },
}


#[derive(Debug)]
pub struct FunctionError {
    function_name: String,
    error: FunctionEvaluationError,
}

#[derive(Debug)]
pub enum FunctionEvaluationError {
    InvalidArgument(usize),
    InvalidArgumentCount(String),
    OverflowError,
    OutofRange,
    InvalidDateFormat,
    InvalidLocalTimeFormat,
    InvalidTimeFormat,
    InvalidDateTimeFormat,
    InvalidLocalDateTimeFormat,
    InvalidDurationFormat,
}

#[derive(Debug)]
pub enum OutOfRangeType {
    IndexOutOfRange,
    TemporalDurationOutOfRange,
    TemporalInstantOutOfRange,
}


// pub enum FunctionEvaluationError{
//     InvalidArgument
// }

// #[derive(Debug)]
// pub enum EvaluationError {
//     DivideByZero,  // never used lol
//     InvalidType,
//     UnknownIdentifier(String),
//     UnknownFunction(String), // Unknown Cypher function or internal helper function
//     InvalidArgumentCount(String),
//     IndexError(IndexError),
//     MiddlewareError(MiddlewareError),
//     ParseError,
//     InvalidContext,
//     OutOfRange,  // Is this for a index out of range error? or something else?
//     FunctionError {
//         function_name: String,
//         error: Box<EvaluationError>,
//     },
//     InvalidState,
//     InvalidArgument,
//     // InvalidType {
//     //     expected: String,
//     //     actual: String,
//     // }
//     // CreationError? TemporalCreationError
//     // RetrievalError? "failed to retrieve duration"
// }

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
