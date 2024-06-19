use std::sync::Arc;

use crate::evaluation::variable_value::VariableValue;
use async_trait::async_trait;
use chrono::NaiveTime;
use drasi_query_ast::ast;
use futures::StreamExt;

use crate::{
    evaluation::{EvaluationError, ExpressionEvaluationContext},
    interface::ElementArchiveIndex,
};

use super::{Function, FunctionRegistry, ScalarFunction};
use crate::models::{Element, TimestampBound, TimestampRange};

pub trait RegisterPastFunctions {
    fn register_past_functions(&self, archive_index: Arc<dyn ElementArchiveIndex>);
}

impl RegisterPastFunctions for FunctionRegistry {
    fn register_past_functions(&self, archive_index: Arc<dyn ElementArchiveIndex>) {
        self.register_function(
            "drasi.getVersionByTimestamp",
            Function::Scalar(Arc::new(GetVersionByTimestamp {
                archive_index: archive_index.clone(),
            })),
        );

        self.register_function(
            "drasi.getVersionsByTimeRange",
            Function::Scalar(Arc::new(GetVersionsByTimeRange {
                archive_index: archive_index.clone(),
            })),
        );
    }
}

pub struct GetVersionByTimestamp {
    archive_index: Arc<dyn ElementArchiveIndex>,
}

#[async_trait]
impl ScalarFunction for GetVersionByTimestamp {
    async fn call(
        &self,
        _context: &ExpressionEvaluationContext,
        _expression: &ast::FunctionExpression,
        args: Vec<VariableValue>,
    ) -> Result<VariableValue, EvaluationError> {
        if args.len() != 2 {
            return Err(EvaluationError::InvalidArgumentCount(
                "getVersionByTimestamp".to_string(),
            ));
        }

        let metadata = match &args[0] {
            VariableValue::Element(e) => e.get_metadata(),
            _ => return Err(EvaluationError::InvalidType),
        };

        let timestamp = match &args[1] {
            VariableValue::Date(d) => d.and_time(NaiveTime::MIN).timestamp_millis() as u64,
            VariableValue::LocalDateTime(d) => d.timestamp_millis() as u64,
            VariableValue::ZonedDateTime(d) => d.datetime().timestamp_millis() as u64,
            VariableValue::Integer(n) => match n.as_u64() {
                Some(u) => u,
                None => return Err(EvaluationError::InvalidType),
            },
            _ => return Err(EvaluationError::InvalidType),
        };

        let element = self
            .archive_index
            .get_element_as_at(&metadata.reference, timestamp)
            .await?;

        match element {
            Some(e) => Ok(e.to_expression_variable()),
            None => Ok(VariableValue::Null),
        }
    }
}

pub struct GetVersionsByTimeRange {
    archive_index: Arc<dyn ElementArchiveIndex>,
}

#[async_trait]
impl ScalarFunction for GetVersionsByTimeRange {
    async fn call(
        &self,
        _context: &ExpressionEvaluationContext,
        _expression: &ast::FunctionExpression,
        args: Vec<VariableValue>,
    ) -> Result<VariableValue, EvaluationError> {
        if args.len() < 3 || args.len() > 4 {
            return Err(EvaluationError::InvalidArgumentCount(
                "getVersionsByTimeRange".to_string(),
            ));
        }
        let metadata = match &args[0] {
            VariableValue::Element(e) => e.get_metadata(),
            _ => return Err(EvaluationError::InvalidType),
        };

        let from = match &args[1] {
            VariableValue::Date(d) => d.and_time(NaiveTime::MIN).timestamp_millis() as u64,
            VariableValue::LocalDateTime(d) => d.timestamp_millis() as u64,
            VariableValue::ZonedDateTime(d) => d.datetime().timestamp_millis() as u64,
            VariableValue::Integer(n) => match n.as_u64() {
                Some(u) => u,
                None => return Err(EvaluationError::InvalidType),
            },
            _ => return Err(EvaluationError::InvalidType),
        };

        let to = match &args[2] {
            VariableValue::Date(d) => d.and_time(NaiveTime::MIN).timestamp_millis() as u64,
            VariableValue::LocalDateTime(d) => d.timestamp_millis() as u64,
            VariableValue::ZonedDateTime(d) => d.datetime().timestamp_millis() as u64,
            VariableValue::Integer(n) => match n.as_u64() {
                Some(u) => u,
                None => return Err(EvaluationError::InvalidType),
            },
            _ => return Err(EvaluationError::InvalidType),
        };

        let retrieve_initial_value = match args.get(3) {
            Some(VariableValue::Bool(b)) => *b,
            None => false,
            _ => return Err(EvaluationError::InvalidType),
        };

        let range = match retrieve_initial_value {
            true => TimestampRange {
                from: TimestampBound::StartFromPrevious(from),
                to,
            },
            false => TimestampRange {
                from: TimestampBound::Included(from),
                to,
            },
        };

        let mut stream = self
            .archive_index
            .get_element_versions(&metadata.reference, range)
            .await?;

        let mut result = Vec::new();
        while let Some(item) = stream.next().await {
            match item {
                Ok(e) => {
                    // if effective time is less than from and if retrieve_initial_value is true
                    //  then we need to update the effective time to be from
                    if retrieve_initial_value && e.get_effective_from() < from {
                        let mut metadata = e.get_metadata().clone();
                        metadata.effective_from = from;
                        let _result_element = Element::Node {
                            metadata,
                            properties: e.get_properties().clone(),
                        };
                        result.push(e.to_expression_variable())
                    } else {
                        result.push(e.to_expression_variable())
                    }
                }
                Err(e) => return Err(EvaluationError::IndexError(e)),
            }
        }

        Ok(VariableValue::List(result))
    }
}
