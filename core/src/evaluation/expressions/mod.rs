#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::sync::Arc;

use async_recursion::async_recursion;
use chrono::{
    Datelike, Duration, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime, Timelike, Weekday, LocalResult
};
use drasi_query_ast::ast;

use crate::evaluation::temporal_constants::{self, MIDNIGHT_NAIVE_TIME, EPOCH_MIDNIGHT_NAIVE_DATETIME, UTC_FIXED_OFFSET};
use crate::evaluation::variable_value::duration::Duration as DurationStruct;
use crate::evaluation::variable_value::float::Float;
use crate::evaluation::variable_value::integer::Integer;
use crate::evaluation::variable_value::zoned_datetime::ZonedDateTime;
use crate::evaluation::variable_value::zoned_time::ZonedTime;
use crate::evaluation::variable_value::{ListRange, RangeBound};
use crate::interface::{ResultKey, ResultOwner};
use crate::{evaluation::variable_value::VariableValue, interface::ResultIndex};

use super::{
    context::{ExpressionEvaluationContext, SideEffects},
    functions::{aggregation::Accumulator, Function, FunctionRegistry},
    EvaluationError,
};

pub struct ExpressionEvaluator {
    functions: Arc<FunctionRegistry>,
    result_index: Arc<dyn ResultIndex>,
}

impl ExpressionEvaluator {
    pub fn new(
        functions: Arc<FunctionRegistry>,
        result_index: Arc<dyn ResultIndex>,
    ) -> ExpressionEvaluator {
        ExpressionEvaluator {
            functions,
            result_index,
        }
    }

    #[async_recursion]
    pub async fn evaluate_expression(
        &self,
        context: &ExpressionEvaluationContext<'_>,
        expression: &ast::Expression,
    ) -> Result<VariableValue, EvaluationError> {
        match expression {
            ast::Expression::UnaryExpression(expression) => {
                self.evaluate_unary_expression(context, expression).await
            }
            ast::Expression::BinaryExpression(expression) => {
                self.evaluate_binary_expression(context, expression).await
            }
            ast::Expression::FunctionExpression(expression) => {
                self.evaluate_function_expression(context, expression).await
            }
            ast::Expression::CaseExpression(expression) => {
                self.evaluate_case_expression(context, expression).await
            }
            ast::Expression::ListExpression(expression) => {
                self.evaluate_list_expression(context, expression).await
            }
            ast::Expression::ObjectExpression(expression) => {
                self.evaluate_object_expression(context, expression).await
            }
            ast::Expression::IteratorExpression(expression) => {
                self.evaluate_iterator_expression(context, expression).await
            }
        }
    }

    pub async fn evaluate_predicate(
        &self,
        context: &ExpressionEvaluationContext<'_>,
        expression: &ast::Expression,
    ) -> Result<bool, EvaluationError> {
        let value = self.evaluate_expression(context, expression).await?;
        match value {
            // Value::Bool(b) => Ok(VariableValue::Bool(b)),
            VariableValue::Bool(b) => Ok(b),
            _ => Ok(false),
        }
    }

    pub async fn evaluate_projection_field(
        &self,
        context: &ExpressionEvaluationContext<'_>,
        expression: &ast::Expression,
    ) -> Result<(String, VariableValue), EvaluationError> {
        let value = self.evaluate_expression(context, expression).await?;
        let alias = match expression {
            ast::Expression::UnaryExpression(expression) => match expression {
                ast::UnaryExpression::Property { name: _, key } => key,
                ast::UnaryExpression::Parameter(p) => p,
                ast::UnaryExpression::Alias { source: _, alias } => alias,
                ast::UnaryExpression::Identifier(id) => id,
                _ => "expression",
            },
            ast::Expression::BinaryExpression(_) => "expression",
            ast::Expression::FunctionExpression(f) => &f.name,
            ast::Expression::CaseExpression(_) => "case",
            ast::Expression::ListExpression(_) => "list",
            ast::Expression::ObjectExpression(_) => "object",
            ast::Expression::IteratorExpression(_) => "iterator",
        };

        Ok((alias.to_string(), value))
    }

    #[async_recursion]
    async fn evaluate_unary_expression(
        &self,
        context: &ExpressionEvaluationContext<'_>,
        expression: &ast::UnaryExpression,
    ) -> Result<VariableValue, EvaluationError> {
        let result = match expression {
            ast::UnaryExpression::Not(expression) => {
                VariableValue::Bool(!self.evaluate_predicate(context, expression).await?)
            }
            ast::UnaryExpression::Exists(_) => todo!(),
            ast::UnaryExpression::IsNull(e) => {
                VariableValue::Bool(self.evaluate_expression(context, e).await?.is_null())
            }
            ast::UnaryExpression::IsNotNull(e) => {
                VariableValue::Bool(!self.evaluate_expression(context, e).await?.is_null())
            }
            ast::UnaryExpression::Literal(l) => match l {
                ast::Literal::Boolean(b) => VariableValue::Bool(*b),
                ast::Literal::Text(t) => VariableValue::String(t.to_string()),
                ast::Literal::Null => VariableValue::Null,
                ast::Literal::Date(_) => VariableValue::Date(*temporal_constants::EPOCH_NAIVE_DATE),
                ast::Literal::Integer(i) => VariableValue::Integer(Integer::from(
                    match serde_json::Number::from(*i).as_i64() {
                        Some(n) => n,
                        None => return Err(EvaluationError::ConversionError),
                    },
                )),
                ast::Literal::Real(r) => match serde_json::Number::from_f64(*r) {
                    Some(n) => VariableValue::Float(Float::from(match n.as_f64() {
                        Some(n) => n,
                        None => return Err(EvaluationError::ConversionError),
                    })),
                    None => VariableValue::Null,
                },
                ast::Literal::LocalTime(_t) => {
                    VariableValue::LocalTime(*MIDNIGHT_NAIVE_TIME)
                }
                ast::Literal::ZonedTime(_t) => VariableValue::ZonedTime(ZonedTime::new(
                    *MIDNIGHT_NAIVE_TIME,
                    *UTC_FIXED_OFFSET,
                )),
                ast::Literal::LocalDateTime(_dt) => VariableValue::LocalDateTime(
                    *EPOCH_MIDNIGHT_NAIVE_DATETIME,
                ),
                ast::Literal::ZonedDateTime(_dt) => {
                    let local_datetime = *EPOCH_MIDNIGHT_NAIVE_DATETIME;
                    VariableValue::ZonedDateTime(ZonedDateTime::new(
                        match local_datetime.and_local_timezone(*UTC_FIXED_OFFSET) {
                            LocalResult::Single(dt) => dt,
                            _ => return Err(EvaluationError::InvalidType),
                        },
                        None,
                    ))
                }
                ast::Literal::Duration(_d) => {
                    VariableValue::Duration(DurationStruct::new(chrono::Duration::zero(), 0, 0))
                }
                ast::Literal::Object(o) => {
                    let mut map = BTreeMap::new();
                    for (key, value) in o {
                        let val = match value {
                            ast::Literal::Integer(i) => VariableValue::Integer(Integer::from(
                                match serde_json::Number::from(*i).as_i64() {
                                    Some(n) => n,
                                    None => return Err(EvaluationError::ConversionError),
                                },
                            )),
                            ast::Literal::Text(s) => VariableValue::String(s.to_string()),
                            ast::Literal::Real(r) => match serde_json::Number::from_f64(*r) {
                                Some(n) => VariableValue::Float(Float::from( match n.as_f64() {
                                    Some(n) => n,
                                    None => return Err(EvaluationError::ConversionError),
                                })),
                                None => VariableValue::Null,
                            },
                            _ => VariableValue::Null,
                        };
                        map.insert(key.to_string(), val);
                    }
                    VariableValue::Object(map)
                }
                ast::Literal::Expression(expr) => {
                    let expression = *expr.clone();
                    VariableValue::Expression(expression)
                }
            },
            ast::UnaryExpression::Property { name, key } => {
                match context.get_variable(name.clone()) {
                    Some(v) => match v {
                        VariableValue::Element(e) => e.get_property(key).into(),

                        //type of object
                        VariableValue::Object(o) => match o.get(&key.to_string()) {
                            Some(v) => v.clone(),
                            None => VariableValue::Null,
                        },
                        VariableValue::Date(d) => match get_date_property(*d, (*key).to_string())
                            .await
                        {
                            Some(v) => VariableValue::Integer(v.into()),
                            None => {
                                return Err(EvaluationError::UnknownFunction((*key).to_string()))
                            }
                        },
                        VariableValue::LocalTime(t) => {
                            match get_local_time_property(*t, (*key).to_string()).await {
                                Some(v) => VariableValue::Integer(v.into()),
                                None => {
                                    return Err(EvaluationError::UnknownFunction(
                                        (*key).to_string(),
                                    ))
                                }
                            }
                        }
                        VariableValue::ZonedTime(t) => {
                            match get_time_property(*t, (*key).to_string()).await {
                                Some(v) => {
                                    if (*key).to_string() == "timezone"
                                        || (*key).to_string() == "offset"
                                    {
                                        VariableValue::String(v.to_string())
                                    } else {
                                        match v.parse::<i64>() {
                                            Ok(parsed_num) => {
                                                VariableValue::Integer(parsed_num.into())
                                            }
                                            Err(_) => {
                                                return Err(EvaluationError::ParseError);
                                            }
                                        }
                                    }
                                }
                                None => {
                                    return Err(EvaluationError::UnknownFunction(
                                        (*key).to_string(),
                                    ))
                                }
                            }
                        }
                        VariableValue::LocalDateTime(t) => {
                            match get_local_datetime_property(*t, (*key).to_string()).await {
                                Some(v) => VariableValue::Integer(v.into()),
                                None => {
                                    return Err(EvaluationError::UnknownFunction(
                                        (*key).to_string(),
                                    ))
                                }
                            }
                        }
                        VariableValue::ZonedDateTime(t) => {
                            match get_datetime_property(t.clone(), (*key).to_string()).await {
                                Some(v) => {
                                    if (*key).to_string() == "timezone"
                                        || (*key).to_string() == "offset"
                                    {
                                        VariableValue::String(v.to_string())
                                    } else {
                                        match v.parse::<i64>() {
                                            Ok(parsed_num) => {
                                                VariableValue::Integer(parsed_num.into())
                                            }
                                            Err(_) => {
                                                return Err(EvaluationError::ParseError);
                                            }
                                        }
                                    }
                                }
                                None => {
                                    return Err(EvaluationError::UnknownFunction(
                                        (*key).to_string(),
                                    ))
                                }
                            }
                        }
                        VariableValue::Duration(d) => {
                            match get_duration_property(d.clone(), (*key).to_string()).await {
                                Some(v) => VariableValue::Integer(v.into()),
                                None => {
                                    return Err(EvaluationError::UnknownFunction(
                                        (*key).to_string(),
                                    ))
                                }
                            }
                        }
                        _ => VariableValue::Null,
                    },
                    None => VariableValue::Null,
                }
            }
            ast::UnaryExpression::ExpressionProperty { exp, key } => match self
                .evaluate_expression(context, exp)
                .await?
            {
                v => match v {
                    //type of object
                    VariableValue::Object(o) => match o.get(&key.to_string()) {
                        Some(v) => v.clone(),
                        None => VariableValue::Null,
                    },
                    VariableValue::Date(d) => {
                        match get_date_property(d, (*key).to_string()).await {
                            Some(v) => VariableValue::Integer(v.into()),
                            None => {
                                return Err(EvaluationError::UnknownFunction((*key).to_string()))
                            }
                        }
                    }
                    VariableValue::LocalTime(t) => {
                        match get_local_time_property(t, (*key).to_string()).await {
                            Some(v) => VariableValue::Integer(v.into()),
                            None => {
                                return Err(EvaluationError::UnknownFunction((*key).to_string()))
                            }
                        }
                    }
                    VariableValue::ZonedTime(t) => match get_time_property(t, (*key).to_string())
                        .await
                    {
                        Some(v) => {
                            if (*key).to_string() == "timezone" || (*key).to_string() == "offset" {
                                VariableValue::String(v.to_string())
                            } else {
                                match v.parse::<i64>() {
                                    Ok(parsed_num) => VariableValue::Integer(parsed_num.into()),
                                    Err(_) => {
                                        return Err(EvaluationError::ParseError);
                                    }
                                }
                            }
                        }
                        None => return Err(EvaluationError::UnknownFunction((*key).to_string())),
                    },
                    VariableValue::LocalDateTime(t) => {
                        match get_local_datetime_property(t, (*key).to_string()).await {
                            Some(v) => VariableValue::Integer(v.into()),
                            None => {
                                return Err(EvaluationError::UnknownFunction((*key).to_string()))
                            }
                        }
                    }
                    VariableValue::ZonedDateTime(t) => {
                        match get_datetime_property(t.clone(), (*key).to_string()).await {
                            Some(v) => {
                                if (*key).to_string() == "timezone"
                                    || (*key).to_string() == "offset"
                                {
                                    VariableValue::String(v.to_string())
                                } else {
                                    match v.parse::<i64>() {
                                        Ok(parsed_num) => VariableValue::Integer(parsed_num.into()),
                                        Err(_) => {
                                            return Err(EvaluationError::ParseError);
                                        }
                                    }
                                }
                            }
                            None => {
                                return Err(EvaluationError::UnknownFunction((*key).to_string()))
                            }
                        }
                    }
                    VariableValue::Duration(d) => {
                        match get_duration_property(d.clone(), (*key).to_string()).await {
                            Some(v) => VariableValue::Integer(v.into()),
                            None => {
                                return Err(EvaluationError::UnknownFunction((*key).to_string()))
                            }
                        }
                    }
                    _ => VariableValue::Null,
                },
            },
            ast::UnaryExpression::Parameter(p) => match context.get_variable(p.clone()) {
                Some(v) => v.clone(),
                None => VariableValue::Null,
            },
            ast::UnaryExpression::Variable { name, value } => {
                //review
                // create an object with name being the key and val being the value
                let variable_value = self.evaluate_expression(context, value).await?;
                let mut map = BTreeMap::new();
                map.insert(name.to_string(), variable_value.clone());
                VariableValue::Object(map)
            }
            ast::UnaryExpression::Alias { source, alias: _ } => {
                self.evaluate_expression(context, source).await?
            }
            ast::UnaryExpression::Identifier(ident) => match context.get_variable(ident.clone()) {
                Some(value) => value.clone(),
                None => return Err(EvaluationError::UnknownIdentifier(ident.to_string())),
            },
            ast::UnaryExpression::ListRange {
                start_bound,
                end_bound,
            } => match (start_bound, end_bound) {
                (Some(start), Some(end)) => {
                    let start = self.evaluate_expression(context, start).await?;
                    if !start.is_i64() {
                        return Err(EvaluationError::InvalidType);
                    }
                    let end = self.evaluate_expression(context, end).await?;
                    if !end.is_i64() {
                        return Err(EvaluationError::InvalidType);
                    }
                    VariableValue::ListRange(ListRange {
                        start: RangeBound::Index(match start.as_i64() {
                            Some(n) => n,
                            None => return Err(EvaluationError::InvalidType),
                        }),
                        end: RangeBound::Index(match end.as_i64() {
                            Some(n) => n,
                            None => return Err(EvaluationError::InvalidType),
                        }),
                    })
                }
                (Some(start), None) => {
                    let start = self.evaluate_expression(context, start).await?;
                    if !start.is_i64() {
                        return Err(EvaluationError::InvalidType);
                    }
                    VariableValue::ListRange(ListRange {
                        start: RangeBound::Index(match start.as_i64() {
                            Some(n) => n,
                            None => return Err(EvaluationError::InvalidType),
                        }),
                        end: RangeBound::Unbounded,
                    })
                }
                (None, Some(end)) => {
                    let end = self.evaluate_expression(context, end).await?;
                    if !end.is_i64() {
                        return Err(EvaluationError::InvalidType);
                    }
                    VariableValue::ListRange(ListRange {
                        start: RangeBound::Unbounded,
                        end: RangeBound::Index(match end.as_i64() {
                            Some(n) => n,
                            None => return Err(EvaluationError::InvalidType),
                        }),
                    })
                }
                (None, None) => VariableValue::ListRange(ListRange {
                    start: RangeBound::Unbounded,
                    end: RangeBound::Unbounded,
                }),
            },
        };
        Ok(result)
    }

    #[async_recursion]
    async fn evaluate_binary_expression(
        &self,
        context: &ExpressionEvaluationContext<'_>,
        expression: &ast::BinaryExpression,
    ) -> Result<VariableValue, EvaluationError> {
        let result = match expression {
            ast::BinaryExpression::And(c1, c2) => VariableValue::Bool(
                self.evaluate_predicate(context, c1).await?
                    && self.evaluate_predicate(context, c2).await?,
            ),
            ast::BinaryExpression::Or(c1, c2) => VariableValue::Bool(
                self.evaluate_predicate(context, c1).await?
                    || self.evaluate_predicate(context, c2).await?,
            ),
            ast::BinaryExpression::Eq(e1, e2) => match (
                self.evaluate_expression(context, e1).await?,
                self.evaluate_expression(context, e2).await?,
            ) {
                (VariableValue::Integer(n1), VariableValue::Integer(n2)) => {
                    VariableValue::Bool(n1 == n2)
                }
                (VariableValue::Float(n1), VariableValue::Float(n2)) => {
                    VariableValue::Bool(n1 == n2)
                }
                (VariableValue::String(s1), VariableValue::String(s2)) => {
                    VariableValue::Bool(s1 == s2)
                }
                (VariableValue::Bool(b1), VariableValue::Bool(b2)) => VariableValue::Bool(b1 == b2),
                (VariableValue::Null, VariableValue::Null) => VariableValue::Bool(true),
                (VariableValue::List(a1), VariableValue::List(a2)) => VariableValue::Bool(a1 == a2),
                (VariableValue::ElementReference(e1), VariableValue::ElementReference(e2)) => {
                    VariableValue::Bool(e1 == e2)
                }
                (VariableValue::Date(date1), VariableValue::Date(date2)) => {
                    VariableValue::Bool(date1 == date2)
                }
                (VariableValue::LocalTime(time1), VariableValue::LocalTime(time2)) => {
                    VariableValue::Bool(time1 == time2)
                }
                (VariableValue::LocalDateTime(dt1), VariableValue::LocalDateTime(dt2)) => {
                    VariableValue::Bool(dt1 == dt2)
                }
                (VariableValue::ZonedTime(time1), VariableValue::ZonedTime(time2)) => {
                    VariableValue::Bool(time1 == time2)
                }
                (VariableValue::ZonedDateTime(zdt1), VariableValue::ZonedDateTime(zdt2)) => {
                    VariableValue::Bool(zdt1 == zdt2)
                }
                (VariableValue::Duration(d1), VariableValue::Duration(d2)) => {
                    VariableValue::Bool(d1 == d2)
                }
                (VariableValue::Awaiting, VariableValue::Awaiting) => VariableValue::Bool(true),
                _ => VariableValue::Bool(false),
            },
            ast::BinaryExpression::Ne(e1, e2) => match (
                self.evaluate_expression(context, e1).await?,
                self.evaluate_expression(context, e2).await?,
            ) {
                (VariableValue::Integer(n1), VariableValue::Integer(n2)) => {
                    VariableValue::Bool(n1 != n2)
                }
                (VariableValue::Float(n1), VariableValue::Float(n2)) => {
                    VariableValue::Bool(n1 != n2)
                }
                (VariableValue::Float(n1), VariableValue::Integer(n2)) => {
                    VariableValue::Bool(n1 != match n2.as_i64() {
                        Some(n) => n as f64,
                        None => return Err(EvaluationError::ConversionError),
                    })
                }
                (VariableValue::Integer(n1), VariableValue::Float(n2)) => {
                    VariableValue::Bool(n2 != match n1.as_i64() {
                        Some(n) => n as f64,
                        None => return Err(EvaluationError::ConversionError),
                    })
                }
                (VariableValue::String(s1), VariableValue::String(s2)) => {
                    VariableValue::Bool(s1 != s2)
                }
                (VariableValue::Bool(b1), VariableValue::Bool(b2)) => VariableValue::Bool(b1 != b2),
                (VariableValue::Null, VariableValue::Null) => VariableValue::Bool(false),
                (VariableValue::List(a1), VariableValue::List(a2)) => VariableValue::Bool(a1 != a2),
                (VariableValue::Date(date1), VariableValue::Date(date2)) => {
                    VariableValue::Bool(date1 != date2)
                }
                (VariableValue::LocalTime(time1), VariableValue::LocalTime(time2)) => {
                    VariableValue::Bool(time1 != time2)
                }
                (VariableValue::LocalDateTime(dt1), VariableValue::LocalDateTime(dt2)) => {
                    VariableValue::Bool(dt1 != dt2)
                }
                (VariableValue::ZonedTime(time1), VariableValue::ZonedTime(time2)) => {
                    VariableValue::Bool(time1 != time2)
                }
                (VariableValue::ZonedDateTime(zdt1), VariableValue::ZonedDateTime(zdt2)) => {
                    VariableValue::Bool(zdt1 != zdt2)
                }
                (VariableValue::Duration(d1), VariableValue::Duration(d2)) => {
                    VariableValue::Bool(d1 != d2)
                }
                (VariableValue::ElementReference(e1), VariableValue::ElementReference(e2)) => {
                    VariableValue::Bool(e1 != e2)
                }
                _ => VariableValue::Bool(false),
            },
            ast::BinaryExpression::Lt(e1, e2) => match (
                self.evaluate_expression(context, e1).await?,
                self.evaluate_expression(context, e2).await?,
            ) {
                (VariableValue::Integer(n1), VariableValue::Integer(n2)) => VariableValue::Bool(
                    n1.as_i64().unwrap_or_default() < n2.as_i64().unwrap_or_default(),
                ),
                (VariableValue::Float(n1), VariableValue::Float(n2)) => VariableValue::Bool(
                    n1.as_f64().unwrap_or_default() < n2.as_f64().unwrap_or_default(),
                ),
                (VariableValue::Float(n1), VariableValue::Integer(n2)) => VariableValue::Bool(
                    n1.as_f64().unwrap_or_default() < n2.as_i64().unwrap_or_default() as f64,
                ),
                (VariableValue::Integer(n1), VariableValue::Float(n2)) => VariableValue::Bool(
                    (n1.as_i64().unwrap_or_default() as f64) < n2.as_f64().unwrap_or_default(),
                ),
                (VariableValue::Date(date1), VariableValue::Date(date2)) => {
                    VariableValue::Bool(date1 < date2)
                }
                (VariableValue::LocalTime(time1), VariableValue::LocalTime(time2)) => {
                    VariableValue::Bool(time1 < time2)
                }
                (VariableValue::LocalDateTime(dt1), VariableValue::LocalDateTime(dt2)) => {
                    VariableValue::Bool(dt1 < dt2)
                }
                (VariableValue::LocalDateTime(dt1), VariableValue::Date(date2)) => {
                    let dt2 = NaiveDateTime::new(date2, *MIDNIGHT_NAIVE_TIME);
                    VariableValue::Bool(dt1 < dt2)
                }
                (VariableValue::LocalDateTime(dt1), VariableValue::LocalTime(time2)) => {
                    let dt2 = NaiveDateTime::new(*temporal_constants::EPOCH_NAIVE_DATE, time2);
                    VariableValue::Bool(dt1 < dt2)
                }
                (VariableValue::LocalTime(time1), VariableValue::LocalDateTime(dt2)) => {
                    let dt1 = NaiveDateTime::new(*temporal_constants::EPOCH_NAIVE_DATE, time1);
                    VariableValue::Bool(dt1 < dt2)
                }
                (VariableValue::Date(date1), VariableValue::LocalDateTime(dt2)) => {
                    let dt1 = NaiveDateTime::new(date1, *MIDNIGHT_NAIVE_TIME);
                    VariableValue::Bool(dt1 < dt2)
                }
                (VariableValue::ZonedTime(time1), VariableValue::ZonedTime(time2)) => {
                    let dt1 =
                        match NaiveDateTime::new(*temporal_constants::EPOCH_NAIVE_DATE, *time1.time())
                            .and_local_timezone(*time1.offset()) {
                                LocalResult::Single(dt) => dt,
                                _ => return Err(EvaluationError::InvalidType),  
                            };
                    let dt2 =
                        match NaiveDateTime::new(*temporal_constants::EPOCH_NAIVE_DATE, *time2.time())
                            .and_local_timezone(*time2.offset()){
                                LocalResult::Single(dt) => dt,
                                _ => return Err(EvaluationError::InvalidType),  
                            };
                    VariableValue::Bool(dt1 < dt2)
                }
                (VariableValue::ZonedDateTime(zdt1), VariableValue::ZonedDateTime(zdt2)) => {
                    VariableValue::Bool(zdt1.datetime() < zdt2.datetime())
                }
                (VariableValue::ZonedDateTime(zdt1), VariableValue::Date(date2)) => {
                    let dt2 = NaiveDateTime::new(date2, *MIDNIGHT_NAIVE_TIME);
                    VariableValue::Bool(zdt1.datetime() < &dt2.and_utc())
                }
                (VariableValue::ZonedDateTime(zdt1), VariableValue::LocalDateTime(dt2)) => {
                    VariableValue::Bool(zdt1.datetime().naive_utc() < dt2)
                }
                (VariableValue::ZonedDateTime(zdt1), VariableValue::LocalTime(time2)) => {
                    let dt2 = NaiveDateTime::new(*temporal_constants::EPOCH_NAIVE_DATE, time2);
                    VariableValue::Bool(zdt1.datetime() < &dt2.and_utc())
                }
                (VariableValue::ZonedDateTime(zdt1), VariableValue::ZonedTime(time2)) => {
                    let dt2 =
                        NaiveDateTime::new(*temporal_constants::EPOCH_NAIVE_DATE, *time2.time());
                    let zdt2 = match dt2.and_local_timezone(*time2.offset()) {
                        LocalResult::Single(dt) => dt,
                        _ => return Err(EvaluationError::InvalidType),
                    };
                    VariableValue::Bool(
                        zdt1.datetime() < &zdt2,
                    )
                }
                (VariableValue::ZonedTime(time1), VariableValue::ZonedDateTime(zdt2)) => {
                    let dt1 =
                        NaiveDateTime::new(*temporal_constants::EPOCH_NAIVE_DATE, *time1.time());
                    let zdt1 = match dt1.and_local_timezone(*time1.offset()) {
                        LocalResult::Single(dt) => dt,
                        _ => return Err(EvaluationError::InvalidType),
                    };
                    VariableValue::Bool(
                        zdt1
                            < *zdt2.datetime(),
                    )
                }
                (VariableValue::ZonedTime(time1), VariableValue::Date(date2)) => {
                    let dt1 =
                        NaiveDateTime::new(*temporal_constants::EPOCH_NAIVE_DATE, *time1.time());
                    let dt2 = NaiveDateTime::new(date2, *MIDNIGHT_NAIVE_TIME);
                    VariableValue::Bool(dt1 < dt2)
                }
                (VariableValue::ZonedTime(time1), VariableValue::LocalDateTime(dt2)) => {
                    let dt1 =
                        NaiveDateTime::new(*temporal_constants::EPOCH_NAIVE_DATE, *time1.time());
                    VariableValue::Bool(dt1 < dt2)
                }
                (VariableValue::ZonedTime(time1), VariableValue::LocalTime(time2)) => {
                    let dt1 =
                        NaiveDateTime::new(*temporal_constants::EPOCH_NAIVE_DATE, *time1.time());
                    let dt2 = NaiveDateTime::new(*temporal_constants::EPOCH_NAIVE_DATE, time2);
                    VariableValue::Bool(dt1 < dt2)
                }
                (VariableValue::Duration(d1), VariableValue::Duration(d2)) => {
                    let year1 = d1.year();
                    let year2 = d2.year();
                    let month1 = d1.month() + year1 * 12;
                    let month2 = d2.month() + year2 * 12;
                    // assume 30 days per month?
                    VariableValue::Bool(
                        *d1.duration() + Duration::days(30 * month1)
                            < *d2.duration() + Duration::days(30 * month2),
                    )
                }
                _ => VariableValue::Bool(false),
            },
            ast::BinaryExpression::Le(e1, e2) => match (
                self.evaluate_expression(context, e1).await?,
                self.evaluate_expression(context, e2).await?,
            ) {
                (VariableValue::Integer(n1), VariableValue::Integer(n2)) => VariableValue::Bool(
                    n1.as_i64().unwrap_or_default() <= n2.as_i64().unwrap_or_default(),
                ),
                (VariableValue::Float(n1), VariableValue::Float(n2)) => VariableValue::Bool(
                    n1.as_f64().unwrap_or_default() <= n2.as_f64().unwrap_or_default(),
                ),
                (VariableValue::Float(n1), VariableValue::Integer(n2)) => VariableValue::Bool(
                    n1.as_f64().unwrap_or_default() <= n2.as_i64().unwrap_or_default() as f64,
                ),
                (VariableValue::Integer(n1), VariableValue::Float(n2)) => VariableValue::Bool(
                    n1.as_i64().unwrap_or_default() as f64 <= n2.as_f64().unwrap_or_default(),
                ),
                (VariableValue::ZonedDateTime(zdt1), VariableValue::ZonedDateTime(zdt2)) => {
                    VariableValue::Bool(zdt1.datetime() <= zdt2.datetime())
                }
                (VariableValue::Date(date1), VariableValue::Date(date2)) => {
                    VariableValue::Bool(date1 <= date2)
                }
                (VariableValue::LocalTime(time1), VariableValue::LocalTime(time2)) => {
                    VariableValue::Bool(time1 <= time2)
                }
                (VariableValue::LocalDateTime(dt1), VariableValue::LocalDateTime(dt2)) => {
                    VariableValue::Bool(dt1 <= dt2)
                }
                (VariableValue::LocalDateTime(dt1), VariableValue::Date(date2)) => {
                    let dt2 = NaiveDateTime::new(date2, *MIDNIGHT_NAIVE_TIME);
                    VariableValue::Bool(dt1 <= dt2)
                }
                (VariableValue::LocalDateTime(dt1), VariableValue::LocalTime(time2)) => {
                    let dt2 = NaiveDateTime::new(*temporal_constants::EPOCH_NAIVE_DATE, time2);
                    VariableValue::Bool(dt1 <= dt2)
                }
                (VariableValue::LocalTime(time1), VariableValue::LocalDateTime(dt2)) => {
                    let dt1 = NaiveDateTime::new(*temporal_constants::EPOCH_NAIVE_DATE, time1);
                    VariableValue::Bool(dt1 <= dt2)
                }
                (VariableValue::Date(date1), VariableValue::LocalDateTime(dt2)) => {
                    let dt1 = NaiveDateTime::new(date1, *MIDNIGHT_NAIVE_TIME);
                    VariableValue::Bool(dt1 <= dt2)
                }
                (VariableValue::ZonedTime(time1), VariableValue::ZonedTime(time2)) => {
                    let dt1 =
                        match NaiveDateTime::new(*temporal_constants::EPOCH_NAIVE_DATE, *time1.time())
                            .and_local_timezone(*time1.offset()) {
                                LocalResult::Single(dt) => dt,
                                _ => return Err(EvaluationError::InvalidType),  
                            };
                    let dt2 =
                        match NaiveDateTime::new(*temporal_constants::EPOCH_NAIVE_DATE, *time2.time())
                            .and_local_timezone(*time2.offset()) {
                                LocalResult::Single(dt) => dt,
                                _ => return Err(EvaluationError::InvalidType),  
                            };
                    VariableValue::Bool(dt1 <= dt2)
                }
                (VariableValue::ZonedDateTime(zdt1), VariableValue::Date(date2)) => {
                    let dt2 = NaiveDateTime::new(date2, *MIDNIGHT_NAIVE_TIME);
                    VariableValue::Bool(zdt1.datetime() <= &dt2.and_utc())
                }
                (VariableValue::ZonedDateTime(zdt1), VariableValue::LocalDateTime(dt2)) => {
                    VariableValue::Bool(zdt1.datetime().naive_utc() <= dt2)
                }
                (VariableValue::ZonedDateTime(zdt1), VariableValue::LocalTime(time2)) => {
                    let dt2 = NaiveDateTime::new(*temporal_constants::EPOCH_NAIVE_DATE, time2);
                    VariableValue::Bool(zdt1.datetime() <= &dt2.and_utc())
                }
                (VariableValue::ZonedDateTime(zdt1), VariableValue::ZonedTime(time2)) => {
                    let dt2 =
                        NaiveDateTime::new(*temporal_constants::EPOCH_NAIVE_DATE, *time2.time());
                    let zdt2 = match dt2.and_local_timezone(*time2.offset()) {
                        LocalResult::Single(dt) => dt,
                        _ => return Err(EvaluationError::InvalidType),  
                    };
                    VariableValue::Bool(
                        zdt1.datetime() <= &zdt2,
                    )
                }
                (VariableValue::ZonedTime(time1), VariableValue::ZonedDateTime(zdt2)) => {
                    let dt1 =
                        NaiveDateTime::new(*temporal_constants::EPOCH_NAIVE_DATE, *time1.time());
                    let zdt1 = match dt1.and_local_timezone(*time1.offset()) {
                        LocalResult::Single(dt) => dt,
                        _ => return Err(EvaluationError::InvalidType),  
                    };
                    VariableValue::Bool(
                        zdt1
                            <= *zdt2.datetime(),
                    )
                }
                (VariableValue::ZonedTime(time1), VariableValue::Date(date2)) => {
                    let dt1 =
                        NaiveDateTime::new(*temporal_constants::EPOCH_NAIVE_DATE, *time1.time());
                    let dt2 = NaiveDateTime::new(date2, *MIDNIGHT_NAIVE_TIME);
                    VariableValue::Bool(dt1 <= dt2)
                }
                (VariableValue::ZonedTime(time1), VariableValue::LocalDateTime(dt2)) => {
                    let dt1 =
                        NaiveDateTime::new(*temporal_constants::EPOCH_NAIVE_DATE, *time1.time());
                    VariableValue::Bool(dt1 <= dt2)
                }
                (VariableValue::ZonedTime(time1), VariableValue::LocalTime(time2)) => {
                    let dt1 =
                        NaiveDateTime::new(*temporal_constants::EPOCH_NAIVE_DATE, *time1.time());
                    let dt2 = NaiveDateTime::new(*temporal_constants::EPOCH_NAIVE_DATE, time2);
                    VariableValue::Bool(dt1 <= dt2)
                }
                (VariableValue::Duration(d1), VariableValue::Duration(d2)) => {
                    let year1 = d1.year();
                    let year2 = d2.year();
                    let month1 = d1.month() + year1 * 12;
                    let month2 = d2.month() + year2 * 12;
                    // assume 30 days per month?
                    VariableValue::Bool(
                        *d1.duration() + Duration::days(30 * month1)
                            <= *d2.duration() + Duration::days(30 * month2),
                    )
                }
                _ => VariableValue::Bool(false),
            },
            ast::BinaryExpression::Gt(e1, e2) => match (
                self.evaluate_expression(context, e1).await?,
                self.evaluate_expression(context, e2).await?,
            ) {
                (VariableValue::Integer(n1), VariableValue::Integer(n2)) => VariableValue::Bool(
                    n1.as_i64().unwrap_or_default() > n2.as_i64().unwrap_or_default(),
                ),
                (VariableValue::Float(n1), VariableValue::Float(n2)) => VariableValue::Bool(
                    n1.as_f64().unwrap_or_default() > n2.as_f64().unwrap_or_default(),
                ),
                (VariableValue::Float(n1), VariableValue::Integer(n2)) => VariableValue::Bool(
                    n1.as_f64().unwrap_or_default() > n2.as_i64().unwrap_or_default() as f64,
                ),
                (VariableValue::Integer(n1), VariableValue::Float(n2)) => VariableValue::Bool(
                    n1.as_i64().unwrap_or_default() as f64 > n2.as_f64().unwrap_or_default(),
                ),
                (VariableValue::Date(date1), VariableValue::Date(date2)) => {
                    VariableValue::Bool(date1 > date2)
                }
                (VariableValue::LocalTime(time1), VariableValue::LocalTime(time2)) => {
                    VariableValue::Bool(time1 > time2)
                }
                (VariableValue::LocalDateTime(dt1), VariableValue::LocalDateTime(dt2)) => {
                    VariableValue::Bool(dt1 > dt2)
                }
                (VariableValue::LocalDateTime(dt1), VariableValue::Date(date2)) => {
                    let dt2 = NaiveDateTime::new(date2, *MIDNIGHT_NAIVE_TIME);
                    VariableValue::Bool(dt1 > dt2)
                }
                (VariableValue::LocalDateTime(dt1), VariableValue::LocalTime(time2)) => {
                    let dt2 = NaiveDateTime::new(*temporal_constants::EPOCH_NAIVE_DATE, time2);
                    VariableValue::Bool(dt1 > dt2)
                }
                (VariableValue::LocalTime(time1), VariableValue::LocalDateTime(dt2)) => {
                    let dt1 = NaiveDateTime::new(*temporal_constants::EPOCH_NAIVE_DATE, time1);
                    VariableValue::Bool(dt1 > dt2)
                }
                (VariableValue::Date(date1), VariableValue::LocalDateTime(dt2)) => {
                    let dt1 = NaiveDateTime::new(date1, *MIDNIGHT_NAIVE_TIME);
                    VariableValue::Bool(dt1 > dt2)
                }
                (VariableValue::ZonedTime(time1), VariableValue::ZonedTime(time2)) => {
                    let dt1 =
                        match NaiveDateTime::new(*temporal_constants::EPOCH_NAIVE_DATE, *time1.time())
                            .and_local_timezone(*time1.offset()) {
                                LocalResult::Single(dt) => dt,
                                _ => return Err(EvaluationError::InvalidType),  
                            };
                    let dt2 =
                        match NaiveDateTime::new(*temporal_constants::EPOCH_NAIVE_DATE, *time2.time())
                            .and_local_timezone(*time2.offset()) {
                                LocalResult::Single(dt) => dt,
                                _ => return Err(EvaluationError::InvalidType),  
                            };

                    VariableValue::Bool(dt1 > dt2)
                }
                (VariableValue::ZonedDateTime(zdt1), VariableValue::ZonedDateTime(zdt2)) => {
                    VariableValue::Bool(zdt1.datetime() > zdt2.datetime())
                }
                (VariableValue::ZonedDateTime(zdt1), VariableValue::Date(date2)) => {
                    let dt2 = NaiveDateTime::new(date2, *MIDNIGHT_NAIVE_TIME);
                    VariableValue::Bool(zdt1.datetime() > &dt2.and_utc())
                }
                (VariableValue::ZonedDateTime(zdt1), VariableValue::LocalDateTime(dt2)) => {
                    VariableValue::Bool(zdt1.datetime().naive_utc() > dt2)
                }
                (VariableValue::ZonedDateTime(zdt1), VariableValue::LocalTime(time2)) => {
                    let dt2 = NaiveDateTime::new(*temporal_constants::EPOCH_NAIVE_DATE, time2);
                    VariableValue::Bool(zdt1.datetime() > &dt2.and_utc())
                }
                (VariableValue::ZonedDateTime(zdt1), VariableValue::ZonedTime(time2)) => {
                    let dt2 =
                        NaiveDateTime::new(*temporal_constants::EPOCH_NAIVE_DATE, *time2.time());
                    let zdt2 = match dt2.and_local_timezone(*time2.offset()) {
                        LocalResult::Single(dt) => dt,
                        _ => return Err(EvaluationError::InvalidType),
                    };
                    VariableValue::Bool(
                        zdt1.datetime() > &zdt2,
                    )
                }
                (VariableValue::ZonedTime(time1), VariableValue::ZonedDateTime(zdt2)) => {
                    let dt1 =
                        NaiveDateTime::new(*temporal_constants::EPOCH_NAIVE_DATE, *time1.time());
                    let zdt1 = match dt1.and_local_timezone(*time1.offset()) {
                        LocalResult::Single(dt) => dt,
                        _ => return Err(EvaluationError::InvalidType),
                    };
                    VariableValue::Bool(
                        zdt1
                            > *zdt2.datetime(),
                    )
                }
                (VariableValue::ZonedTime(time1), VariableValue::Date(date2)) => {
                    let dt1 =
                        NaiveDateTime::new(*temporal_constants::EPOCH_NAIVE_DATE, *time1.time());
                    let dt2 = NaiveDateTime::new(date2, *MIDNIGHT_NAIVE_TIME);
                    VariableValue::Bool(dt1 > dt2)
                }
                (VariableValue::ZonedTime(time1), VariableValue::LocalDateTime(dt2)) => {
                    let dt1 =
                        NaiveDateTime::new(*temporal_constants::EPOCH_NAIVE_DATE, *time1.time());
                    VariableValue::Bool(dt1 > dt2)
                }
                (VariableValue::Date(date1), VariableValue::ZonedDateTime(zdt2)) => {
                    let dt1 = NaiveDateTime::new(date1, *MIDNIGHT_NAIVE_TIME);
                    VariableValue::Bool(&dt1.and_utc() > zdt2.datetime())
                }
                (VariableValue::Date(date1), VariableValue::LocalTime(time2)) => {
                    let dt1 = NaiveDateTime::new(date1, *MIDNIGHT_NAIVE_TIME);
                    let dt2 = NaiveDateTime::new(*temporal_constants::EPOCH_NAIVE_DATE, time2);
                    VariableValue::Bool(dt1 > dt2)
                }
                (VariableValue::Date(date1), VariableValue::ZonedTime(time2)) => {
                    let dt1 = NaiveDateTime::new(date1, *MIDNIGHT_NAIVE_TIME);
                    let dt2 =
                        NaiveDateTime::new(*temporal_constants::EPOCH_NAIVE_DATE, *time2.time());
                    let zdt2 = match dt2.and_local_timezone(*time2.offset()) {
                        LocalResult::Single(dt) => dt,
                        _ => return Err(EvaluationError::InvalidType),
                    };
                    VariableValue::Bool(
                        dt1.and_utc() > zdt2,
                    )
                }
                (VariableValue::ZonedTime(time1), VariableValue::LocalTime(time2)) => {
                    let dt1 =
                        NaiveDateTime::new(*temporal_constants::EPOCH_NAIVE_DATE, *time1.time());
                    let dt2 = NaiveDateTime::new(*temporal_constants::EPOCH_NAIVE_DATE, time2);
                    VariableValue::Bool(dt1 > dt2)
                }
                (VariableValue::Duration(d1), VariableValue::Duration(d2)) => {
                    let year1 = d1.year();
                    let year2 = d2.year();
                    let month1 = d1.month() + year1 * 12;
                    let month2 = d2.month() + year2 * 12;
                    // assume 30 days per month?
                    VariableValue::Bool(
                        *d1.duration() + Duration::days(30 * month1)
                            > *d2.duration() + Duration::days(30 * month2),
                    )
                }
                _ => VariableValue::Bool(false),
            },
            ast::BinaryExpression::Ge(e1, e2) => match (
                self.evaluate_expression(context, e1).await?,
                self.evaluate_expression(context, e2).await?,
            ) {
                (VariableValue::Integer(n1), VariableValue::Integer(n2)) => VariableValue::Bool(
                    n1.as_i64().unwrap_or_default() >= n2.as_i64().unwrap_or_default(),
                ),
                (VariableValue::Float(n1), VariableValue::Float(n2)) => VariableValue::Bool(
                    n1.as_f64().unwrap_or_default() >= n2.as_f64().unwrap_or_default(),
                ),
                (VariableValue::Float(n1), VariableValue::Integer(n2)) => VariableValue::Bool(
                    n1.as_f64().unwrap_or_default() >= n2.as_i64().unwrap_or_default() as f64,
                ),
                (VariableValue::Integer(n1), VariableValue::Float(n2)) => VariableValue::Bool(
                    n1.as_i64().unwrap_or_default() as f64 >= n2.as_f64().unwrap_or_default(),
                ),
                (VariableValue::Date(date1), VariableValue::Date(date2)) => {
                    VariableValue::Bool(date1 >= date2)
                }
                (VariableValue::LocalTime(time1), VariableValue::LocalTime(time2)) => {
                    VariableValue::Bool(time1 >= time2)
                }
                (VariableValue::LocalDateTime(dt1), VariableValue::LocalDateTime(dt2)) => {
                    VariableValue::Bool(dt1 >= dt2)
                }
                (VariableValue::LocalDateTime(dt1), VariableValue::Date(date2)) => {
                    let dt2 = NaiveDateTime::new(date2, *MIDNIGHT_NAIVE_TIME);
                    VariableValue::Bool(dt1 >= dt2)
                }
                (VariableValue::LocalDateTime(dt1), VariableValue::LocalTime(time2)) => {
                    let dt2 = NaiveDateTime::new(*temporal_constants::EPOCH_NAIVE_DATE, time2);
                    VariableValue::Bool(dt1 >= dt2)
                }
                (VariableValue::LocalTime(time1), VariableValue::LocalDateTime(dt2)) => {
                    let dt1 = NaiveDateTime::new(*temporal_constants::EPOCH_NAIVE_DATE, time1);
                    VariableValue::Bool(dt1 >= dt2)
                }
                (VariableValue::Date(date1), VariableValue::LocalDateTime(dt2)) => {
                    let dt1 = NaiveDateTime::new(date1, *MIDNIGHT_NAIVE_TIME);
                    VariableValue::Bool(dt1 >= dt2)
                }
                (VariableValue::LocalDateTime(dt1), VariableValue::ZonedDateTime(zdt2)) => {
                    VariableValue::Bool(dt1 >= zdt2.datetime().naive_utc())
                }
                (VariableValue::ZonedDateTime(zdt1), VariableValue::Date(date2)) => {
                    let dt2 = NaiveDateTime::new(date2, *MIDNIGHT_NAIVE_TIME);
                    VariableValue::Bool(zdt1.datetime() >= &dt2.and_utc())
                }
                (VariableValue::ZonedDateTime(zdt1), VariableValue::LocalDateTime(dt2)) => {
                    VariableValue::Bool(zdt1.datetime().naive_utc() >= dt2)
                }
                (VariableValue::ZonedDateTime(zdt1), VariableValue::LocalTime(time2)) => {
                    let dt2 = NaiveDateTime::new(*temporal_constants::EPOCH_NAIVE_DATE, time2);
                    VariableValue::Bool(zdt1.datetime() >= &dt2.and_utc())
                }
                (VariableValue::ZonedDateTime(zdt1), VariableValue::ZonedTime(time2)) => {
                    let dt2 =
                        NaiveDateTime::new(*temporal_constants::EPOCH_NAIVE_DATE, *time2.time());
                    let zdt2 = match dt2.and_local_timezone(*time2.offset()) {
                        LocalResult::Single(dt) => dt,
                        _ => return Err(EvaluationError::InvalidType),
                    };
                    VariableValue::Bool(
                        zdt1.datetime() >= &zdt2,
                    )
                }
                (VariableValue::ZonedTime(time1), VariableValue::ZonedDateTime(zdt2)) => {
                    let dt1 =
                        NaiveDateTime::new(*temporal_constants::EPOCH_NAIVE_DATE, *time1.time());
                    let zdt1 = match dt1.and_local_timezone(zdt2.datetime().timezone()) {
                        LocalResult::Single(dt) => dt,
                        _ => return Err(EvaluationError::InvalidType),
                    };
                    VariableValue::Bool(
                        zdt1
                            >= *zdt2.datetime(),
                    )
                }
                (VariableValue::ZonedTime(time1), VariableValue::Date(date2)) => {
                    let dt1 =
                        NaiveDateTime::new(*temporal_constants::EPOCH_NAIVE_DATE, *time1.time());
                    let dt2 = NaiveDateTime::new(date2, *MIDNIGHT_NAIVE_TIME);
                    VariableValue::Bool(dt1 >= dt2)
                }
                (VariableValue::ZonedTime(time1), VariableValue::LocalDateTime(dt2)) => {
                    let dt1 =
                        NaiveDateTime::new(*temporal_constants::EPOCH_NAIVE_DATE, *time1.time());
                    VariableValue::Bool(dt1 >= dt2)
                }
                (VariableValue::ZonedTime(time1), VariableValue::LocalTime(time2)) => {
                    let dt1 =
                        NaiveDateTime::new(*temporal_constants::EPOCH_NAIVE_DATE, *time1.time());
                    let dt2 = NaiveDateTime::new(*temporal_constants::EPOCH_NAIVE_DATE, time2);
                    VariableValue::Bool(dt1 >= dt2)
                }
                (VariableValue::ZonedTime(time1), VariableValue::ZonedTime(time2)) => {
                    let dt1 =
                        match NaiveDateTime::new(*temporal_constants::EPOCH_NAIVE_DATE, *time1.time())
                            .and_local_timezone(*time1.offset()) {
                                LocalResult::Single(dt) => dt,
                                _ => return Err(EvaluationError::InvalidType),  
                            };
                    let dt2 =
                        match NaiveDateTime::new(*temporal_constants::EPOCH_NAIVE_DATE, *time2.time())
                            .and_local_timezone(*time2.offset()) {
                                LocalResult::Single(dt) => dt,
                                _ => return Err(EvaluationError::InvalidType),  
                            };
                    VariableValue::Bool(dt1 >= dt2)
                }
                (VariableValue::Duration(d1), VariableValue::Duration(d2)) => {
                    let year1 = d1.year();
                    let year2 = d2.year();
                    let month1 = d1.month() + year1 * 12;
                    let month2 = d2.month() + year2 * 12;
                    // assume 30 days per month?
                    VariableValue::Bool(
                        *d1.duration() + Duration::days(30 * month1)
                            >= *d2.duration() + Duration::days(30 * month2),
                    )
                }
                (VariableValue::ZonedDateTime(zdt1), VariableValue::ZonedDateTime(zdt2)) => {
                    VariableValue::Bool(zdt1.datetime() >= zdt2.datetime())
                }
                _ => VariableValue::Bool(false),
            },
            ast::BinaryExpression::Add(e1, e2) => {
                let n1 = self.evaluate_expression(context, e1).await?;
                let n2 = self.evaluate_expression(context, e2).await?;
                match (n1, n2) {
                    (VariableValue::Integer(n1), VariableValue::Integer(n2)) => {
                        VariableValue::Integer(Integer::from(
                            n1.as_i64().unwrap_or_default() + n2.as_i64().unwrap_or_default(),
                        ))
                    }
                    (VariableValue::Float(n1), VariableValue::Float(n2)) => {
                        VariableValue::Float(Float::from(
                            n1.as_f64().unwrap_or_default() + n2.as_f64().unwrap_or_default(),
                        ))
                    }
                    (VariableValue::Float(n1), VariableValue::Integer(n2)) => {
                        VariableValue::Float(Float::from(
                            n1.as_f64().unwrap_or_default()
                                + n2.as_i64().unwrap_or_default() as f64,
                        ))
                    }
                    (VariableValue::Integer(n1), VariableValue::Float(n2)) => {
                        VariableValue::Float(Float::from(
                            n1.as_i64().unwrap_or_default() as f64
                                + n2.as_f64().unwrap_or_default(),
                        ))
                    }
                    (VariableValue::Integer(n1), VariableValue::String(s2)) => {
                        VariableValue::String(n1.to_string() + &s2)
                    }
                    (VariableValue::Float(n1), VariableValue::String(s2)) => {
                        VariableValue::String(n1.to_string() + &s2)
                    }
                    (VariableValue::String(s1), VariableValue::Bool(b2)) => {
                        VariableValue::String(s1 + &b2.to_string())
                    }
                    (VariableValue::String(s1), VariableValue::Integer(n2)) => {
                        VariableValue::String(s1 + &n2.to_string())
                    }
                    (VariableValue::String(s1), VariableValue::Float(n2)) => {
                        VariableValue::String(s1 + &n2.to_string())
                    }
                    (VariableValue::String(s1), VariableValue::String(s2)) => {
                        VariableValue::String(s1 + &s2)
                    }
                    (VariableValue::Date(date), VariableValue::Duration(duration)) => date
                        .checked_add_signed(*duration.duration())
                        .map_or(VariableValue::Null, |new_date| {
                            VariableValue::Date(new_date)
                        }),
                    (VariableValue::LocalTime(time), VariableValue::Duration(duration)) => {
                        let new_time = time + *duration.duration();
                        VariableValue::LocalTime(new_time)
                    }
                    (VariableValue::LocalDateTime(dt), VariableValue::Duration(duration)) => dt
                        .checked_add_signed(*duration.duration())
                        .map_or(VariableValue::Null, |new_dt| {
                            VariableValue::LocalDateTime(new_dt)
                        }),
                    (VariableValue::ZonedTime(time), VariableValue::Duration(duration)) => {
                        let new_time = *time.time() + *duration.duration();
                        VariableValue::ZonedTime(ZonedTime::new(new_time, *time.offset()))
                    }
                    (VariableValue::ZonedDateTime(zdt), VariableValue::Duration(d)) => zdt
                        .datetime()
                        .checked_add_signed(*d.duration())
                        .map_or(VariableValue::Null, |new_zdt| {
                            VariableValue::ZonedDateTime(ZonedDateTime::new(
                                new_zdt,
                                zdt.timezone().clone(),
                            ))
                        }),
                    (VariableValue::Duration(duration1), VariableValue::Duration(duration2)) => {
                        VariableValue::Duration(duration1 + duration2)
                    }
                    _ => VariableValue::Null,
                }
            }
            ast::BinaryExpression::Subtract(e1, e2) => {
                let n1 = self.evaluate_expression(context, e1).await?;
                let n2 = self.evaluate_expression(context, e2).await?;
                match (n1, n2) {
                    (VariableValue::Integer(n1), VariableValue::Integer(n2)) => {
                        VariableValue::Integer(Integer::from(
                            n1.as_i64().unwrap_or_default() - n2.as_i64().unwrap_or_default(),
                        ))
                    }
                    (VariableValue::Float(n1), VariableValue::Float(n2)) => {
                        VariableValue::Float(Float::from(
                            n1.as_f64().unwrap_or_default() - n2.as_f64().unwrap_or_default(),
                        ))
                    }
                    (VariableValue::Float(n1), VariableValue::Integer(n2)) => {
                        VariableValue::Float(Float::from(
                            n1.as_f64().unwrap_or_default()
                                - n2.as_i64().unwrap_or_default() as f64,
                        ))
                    }
                    (VariableValue::Integer(n1), VariableValue::Float(n2)) => {
                        VariableValue::Float(Float::from(
                            n1.as_i64().unwrap_or_default() as f64
                                - n2.as_f64().unwrap_or_default(),
                        ))
                    }
                    (VariableValue::Date(date), VariableValue::Duration(duration)) => date
                        .checked_sub_signed(*duration.duration())
                        .map_or(VariableValue::Null, |new_date| {
                            VariableValue::Date(new_date)
                        }),
                    (VariableValue::LocalTime(time), VariableValue::Duration(duration)) => {
                        let new_time = time - *duration.duration();
                        VariableValue::LocalTime(new_time)
                    }
                    (VariableValue::LocalDateTime(dt), VariableValue::Duration(duration)) => dt
                        .checked_sub_signed(*duration.duration())
                        .map_or(VariableValue::Null, |new_dt| {
                            VariableValue::LocalDateTime(new_dt)
                        }),
                    (VariableValue::ZonedTime(time), VariableValue::Duration(duration)) => {
                        let new_time = *time.time() - *duration.duration();
                        VariableValue::ZonedTime(ZonedTime::new(new_time, *time.offset()))
                    }
                    (VariableValue::ZonedDateTime(zdt), VariableValue::Duration(d)) => zdt
                        .datetime()
                        .checked_sub_signed(*d.duration())
                        .map_or(VariableValue::Null, |new_zdt| {
                            VariableValue::ZonedDateTime(ZonedDateTime::new(
                                new_zdt,
                                zdt.timezone().clone(),
                            ))
                        }),
                    (VariableValue::Duration(duration1), VariableValue::Duration(duration2)) => {
                        VariableValue::Duration(duration1 - duration2)
                    }
                    _ => VariableValue::Null,
                }
            }
            ast::BinaryExpression::Multiply(e1, e2) => {
                let n1 = self.evaluate_expression(context, e1).await?;
                let n2 = self.evaluate_expression(context, e2).await?;
                match (n1, n2) {
                    (VariableValue::Integer(n1), VariableValue::Integer(n2)) => {
                        VariableValue::Integer(Integer::from(
                            n1.as_i64().unwrap_or_default() * n2.as_i64().unwrap_or_default(),
                        ))
                    }
                    (VariableValue::Float(n1), VariableValue::Float(n2)) => {
                        VariableValue::Float(Float::from(
                            n1.as_f64().unwrap_or_default() * n2.as_f64().unwrap_or_default(),
                        ))
                    }
                    (VariableValue::Float(n1), VariableValue::Integer(n2)) => {
                        VariableValue::Float(Float::from(
                            n1.as_f64().unwrap_or_default()
                                * n2.as_i64().unwrap_or_default() as f64,
                        ))
                    }
                    (VariableValue::Integer(n1), VariableValue::Float(n2)) => {
                        VariableValue::Float(Float::from(
                            n1.as_i64().unwrap_or_default() as f64
                                * n2.as_f64().unwrap_or_default(),
                        ))
                    }
                    _ => VariableValue::Null,
                }
            }
            ast::BinaryExpression::Divide(e1, e2) => {
                let n1 = self.evaluate_expression(context, e1).await?;
                let n2 = self.evaluate_expression(context, e2).await?;
                match (n1, n2) {
                    (VariableValue::Integer(n1), VariableValue::Integer(n2)) => {
                        let m1 = n1.as_i64().ok_or(EvaluationError::InvalidType)?;
                        let m2 = n2.as_i64().ok_or(EvaluationError::InvalidType)?;
                        if m2 == 0 {
                            VariableValue::Null
                        } else {
                            VariableValue::Float(Float::from(m1 as f64 / m2 as f64))
                        }
                    }
                    (VariableValue::Float(n1), VariableValue::Float(n2)) => {
                        let m1 = n1.as_f64().ok_or(EvaluationError::InvalidType)?;
                        let m2 = n2.as_f64().ok_or(EvaluationError::InvalidType)?;
                        if m2 == 0.0 {
                            VariableValue::Null
                        } else {
                            VariableValue::Float(Float::from(m1 / m2))
                        }
                    }
                    (VariableValue::Float(n1), VariableValue::Integer(n2)) => {
                        let m1 = n1.as_f64().ok_or(EvaluationError::InvalidType)?;
                        let m2 = n2.as_i64().ok_or(EvaluationError::InvalidType)?;
                        if m2 == 0 {
                            VariableValue::Null
                        } else {
                            VariableValue::Float(Float::from(m1 / m2 as f64))
                        }
                    }
                    (VariableValue::Integer(n1), VariableValue::Float(n2)) => {
                        let m1 = n1.as_i64().ok_or(EvaluationError::InvalidType)?;
                        let m2 = n2.as_f64().ok_or(EvaluationError::InvalidType)?;
                        if m2 == 0.0 {
                            VariableValue::Null
                        } else {
                            VariableValue::Float(Float::from(m1 as f64 / m2))
                        }
                    }
                    _ => VariableValue::Null,
                }
            }
            ast::BinaryExpression::In(e1, e2) => {
                let e1 = self.evaluate_expression(context, e1).await?;
                match self.evaluate_expression(context, e2).await? {
                    VariableValue::List(a) => VariableValue::Bool(a.contains(&e1)),
                    _ => return Err(EvaluationError::InvalidType),
                }
            }
            ast::BinaryExpression::Modulo(e1, e2) => {
                let n1 = self.evaluate_expression(context, e1).await?;
                let n2 = self.evaluate_expression(context, e2).await?;
                match (n1, n2) {
                    (VariableValue::Integer(n1), VariableValue::Integer(n2)) => {
                        VariableValue::Integer(Integer::from(
                            n1.as_i64().unwrap_or_default() % n2.as_i64().unwrap_or_default(),
                        ))
                    }
                    (VariableValue::Float(n1), VariableValue::Float(n2)) => {
                        VariableValue::Float(Float::from(
                            n1.as_f64().unwrap_or_default() % n2.as_f64().unwrap_or_default(),
                        ))
                    }
                    (VariableValue::Float(n1), VariableValue::Integer(n2)) => {
                        VariableValue::Float(Float::from(
                            n1.as_f64().unwrap_or_default()
                                % n2.as_i64().unwrap_or_default() as f64,
                        ))
                    }
                    (VariableValue::Integer(n1), VariableValue::Float(n2)) => {
                        VariableValue::Float(Float::from(
                            n1.as_i64().unwrap_or_default() as f64
                                % n2.as_f64().unwrap_or_default(),
                        ))
                    }
                    _ => VariableValue::Null,
                }
            }
            ast::BinaryExpression::Exponent(e1, e2) => {
                let n1 = self.evaluate_expression(context, e1).await?;
                let n2 = self.evaluate_expression(context, e2).await?;
                match (n1, n2) {
                    (VariableValue::Integer(n1), VariableValue::Integer(n2)) => {
                        VariableValue::Integer(Integer::from(
                            n1.as_i64().unwrap_or_default().pow(
                                n2.as_i64()
                                    .unwrap_or_default()
                                    .try_into()
                                    .map_err(|_| EvaluationError::InvalidType)?,
                            ),
                        ))
                    }
                    (VariableValue::Float(n1), VariableValue::Float(n2)) => {
                        VariableValue::Float(Float::from(
                            n1.as_f64()
                                .unwrap_or_default()
                                .powf(n2.as_f64().unwrap_or_default()),
                        ))
                    }
                    (VariableValue::Float(n1), VariableValue::Integer(n2)) => {
                        VariableValue::Float(Float::from(
                            n1.as_f64()
                                .unwrap_or_default()
                                .powf(n2.as_i64().unwrap_or_default() as f64),
                        ))
                    }
                    (VariableValue::Integer(n1), VariableValue::Float(n2)) => {
                        VariableValue::Float(Float::from(
                            (n1.as_i64().unwrap_or_default() as f64)
                                .powf(n2.as_f64().unwrap_or_default()),
                        ))
                    }
                    _ => VariableValue::Null,
                }
            }
            ast::BinaryExpression::HasLabel(e1, e2) => {
                let subject = self.evaluate_expression(context, e1).await?;
                let label = match self.evaluate_expression(context, e2).await? {
                    VariableValue::String(s) => Arc::from(s),
                    _ => return Err(EvaluationError::InvalidType),
                };

                match subject {
                    VariableValue::Element(n) => {
                        VariableValue::Bool(n.get_metadata().labels.contains(&label))
                    }
                    _ => VariableValue::Bool(false),
                }
            }
            ast::BinaryExpression::Index(e1, e2) => {
                let index_exp = self.evaluate_expression(context, e2).await?;
                let variable_value_list = self.evaluate_expression(context, e1).await?;
                let list = match variable_value_list.as_array() {
                    Some(list) => list,
                    None => return Err(EvaluationError::ConversionError),
                };
                match index_exp {
                    VariableValue::ListRange(list_range) => {
                        let start_bound = match list_range.start {
                            RangeBound::Index(index) => {
                                if index < 0 {
                                    index.abs() + 1
                                } else if index > list.len() as i64 {
                                    list.len() as i64
                                } else {
                                    index
                                }
                            }
                            RangeBound::Unbounded => 0,
                        };
                        let end_bound = match list_range.end {
                            RangeBound::Index(index) => {
                                if index > list.len() as i64 {
                                    list.len() as i64
                                } else if index < 0 {
                                    list.len() as i64 + index
                                } else {
                                    index
                                }
                            }
                            RangeBound::Unbounded => list.len() as i64,
                        };
                        let result = list[start_bound as usize..end_bound as usize].to_vec();
                        return Ok(VariableValue::List(result));
                    }
                    VariableValue::Integer(index) => {
                        if !index.is_i64() {
                            return Err(EvaluationError::InvalidType);
                        }
                        let index_i64 = match index.as_i64() {
                            Some(index) => index,
                            None => return Err(EvaluationError::ConversionError),
                        };
                        if index_i64 >= list.len() as i64 {
                            return Ok(VariableValue::Null);
                        }
                        if index_i64 < 0 {
                            let index_i64 = list.len() as i64 + index_i64;
                            let element = list[index_i64 as usize].clone();
                            return Ok(element);
                        }
                        let index = match index.as_i64() {
                            Some(index) => index as usize,
                            None => return Err(EvaluationError::ConversionError),
                        };
                        let element = list[index].clone();

                        return Ok(element);
                    }
                    _ => return Err(EvaluationError::InvalidType),
                }
            }
        };
        Ok(result)
    }

    async fn evaluate_function_expression(
        &self,
        context: &ExpressionEvaluationContext<'_>,
        expression: &ast::FunctionExpression,
    ) -> Result<VariableValue, EvaluationError> {
        let result = match self.functions.get_function(&expression.name) {
            Some(function) => match function.as_ref() {
                Function::Scalar(scalar) => {
                    let mut values = Vec::new();
                    for arg in &expression.args {
                        values.push(self.evaluate_expression(context, arg).await?);
                    }
                    scalar
                        .call(context, expression, values)
                        .await
                        .map_err(|e| EvaluationError::FunctionError {
                            function_name: expression.name.to_string(),
                            error: Box::new(e),
                        })?
                }
                Function::LazyScalar(scalar) => scalar
                    .call(context, expression, &expression.args)
                    .await
                    .map_err(|e| EvaluationError::FunctionError {
                        function_name: expression.name.to_string(),
                        error: Box::new(e),
                    })?,
                Function::Aggregating(aggregate) => {
                    let mut values = Vec::new();
                    for arg in &expression.args {
                        values.push(self.evaluate_expression(context, arg).await?);
                    }
                    let grouping_keys = Arc::new(match context.get_output_grouping_key() {
                        Some(group_expressions) => {
                            let mut grouping_keys = Vec::new();
                            for group_expression in group_expressions {
                                grouping_keys.push(
                                    self.evaluate_expression(context, group_expression).await?,
                                );
                            }
                            grouping_keys
                        }
                        None => Vec::new(),
                    });

                    let result_key = ResultKey::GroupBy(grouping_keys.clone());
                    let result_owner = ResultOwner::Function(expression.position_in_query);

                    let mut accumulator = {
                        if aggregate.accumulator_is_lazy() {
                            aggregate.initialize_accumulator(
                                context,
                                expression,
                                &grouping_keys,
                                self.result_index.clone(),
                            )
                        } else {
                            match self.result_index.get(&result_key, &result_owner).await? {
                                Some(acc) => Accumulator::Value(acc),
                                None => aggregate.initialize_accumulator(
                                    context,
                                    expression,
                                    &grouping_keys,
                                    self.result_index.clone(),
                                ),
                            }
                        }
                    };

                    let result = match context.get_side_effects() {
                        SideEffects::Apply => aggregate
                            .apply(context, values, &mut accumulator)
                            .await
                            .map_err(|e| EvaluationError::FunctionError {
                                function_name: expression.name.to_string(),
                                error: Box::new(e),
                            })?,
                        SideEffects::RevertForUpdate | SideEffects::RevertForDelete => aggregate
                            .revert(context, values, &mut accumulator)
                            .await
                            .map_err(|e| EvaluationError::FunctionError {
                                function_name: expression.name.to_string(),
                                error: Box::new(e),
                            })?,
                        SideEffects::Snapshot => aggregate
                            .snapshot(context, values, &accumulator)
                            .await
                            .map_err(|e| EvaluationError::FunctionError {
                                function_name: expression.name.to_string(),
                                error: Box::new(e),
                            })?,
                    };

                    //println!("{:?} {}{} : {:?}", context.get_side_effects(), expression.name, expression.position_in_query, result);

                    match accumulator {
                        super::functions::aggregation::Accumulator::Value(va) => {
                            self.result_index
                                .set(result_key, result_owner, Some(va))
                                .await?
                        }
                        super::functions::aggregation::Accumulator::LazySortedSet(
                            mut accumulator,
                        ) => accumulator.commit().await?,
                    };

                    result
                }
                Function::ContextMutator(context_mutator) => {
                    if expression.args.is_empty() {
                        VariableValue::Null
                    } else {
                        let new_context = context_mutator.call(context, expression).await?;

                        self.evaluate_expression(&new_context, &expression.args[0])
                            .await?
                    }
                }
            },
            None => {
                return Err(EvaluationError::UnknownFunction(
                    expression.name.to_string(),
                ))
            }
        };

        Ok(result)
    }

    async fn evaluate_case_expression(
        &self,
        context: &ExpressionEvaluationContext<'_>,
        expression: &ast::CaseExpression,
    ) -> Result<VariableValue, EvaluationError> {
        let match_ = match expression.match_ {
            Some(ref match_) => Some(self.evaluate_expression(context, match_).await?),
            None => None,
        };

        for when in &expression.when {
            match match_ {
                Some(ref match_) => {
                    let condition = self.evaluate_expression(context, &when.0).await?;
                    if condition == *match_ {
                        return self.evaluate_expression(context, &when.1).await;
                    }
                }
                None => {
                    let condition = self.evaluate_predicate(context, &when.0).await?;
                    if condition {
                        return self.evaluate_expression(context, &when.1).await;
                    }
                }
            }
        }

        match expression.else_ {
            Some(ref else_) => Ok(self.evaluate_expression(context, else_).await?),
            None => Ok(VariableValue::Null),
        }
    }

    async fn evaluate_list_expression(
        &self,
        context: &ExpressionEvaluationContext<'_>,
        expression: &ast::ListExpression,
    ) -> Result<VariableValue, EvaluationError> {
        let mut result = Vec::new();
        for e in &expression.elements {
            result.push(self.evaluate_expression(context, e).await?);
        }

        Ok(VariableValue::List(result))
    }

    async fn evaluate_object_expression(
        &self,
        context: &ExpressionEvaluationContext<'_>,
        expression: &ast::ObjectExpression,
    ) -> Result<VariableValue, EvaluationError> {
        let mut result = BTreeMap::new();
        for (key, value) in &expression.elements {
            result.insert(
                key.to_string(),
                self.evaluate_expression(context, value).await?,
            );
        }

        Ok(VariableValue::Object(result))
    }

    pub async fn evaluate_assignment(
        &self,
        context: &ExpressionEvaluationContext<'_>,
        expression: &ast::Expression,
    ) -> Result<(Arc<str>, VariableValue), EvaluationError> {
        match expression {
            ast::Expression::BinaryExpression(exp) => match exp {
                ast::BinaryExpression::Eq(var, val) => {
                    let variable = match *var.clone() {
                        ast::Expression::UnaryExpression(exp) => match exp {
                            ast::UnaryExpression::Identifier(ident) => ident,
                            _ => return Err(EvaluationError::InvalidType),
                        },
                        _ => return Err(EvaluationError::InvalidType),
                    };
                    let value = self.evaluate_expression(context, val).await?;
                    Ok((variable, value))
                }
                _ => Err(EvaluationError::InvalidType),
            },
            _ => Err(EvaluationError::InvalidType),
        }
    }

    async fn evaluate_iterator_expression(
        &self,
        context: &ExpressionEvaluationContext<'_>,
        expression: &ast::IteratorExpression,
    ) -> Result<VariableValue, EvaluationError> {
        let items = self
            .evaluate_expression(context, &expression.list_expression)
            .await?;
        match items {
            VariableValue::List(items) => {
                let mut result = Vec::new();
                let mut variables = context.clone_variables();
                for item in items {
                    if let Some(filter) = &expression.filter {
                        variables
                            .insert(expression.item_identifier.to_string().into(), item.clone());
                        let local_context =
                            ExpressionEvaluationContext::new(&variables, context.get_clock());
                        if !self.evaluate_predicate(&local_context, filter).await? {
                            continue;
                        }
                    }

                    match &expression.map_expression {
                        Some(map_expression) => {
                            variables.insert(
                                expression.item_identifier.to_string().into(),
                                item.clone(),
                            );
                            let local_context =
                                ExpressionEvaluationContext::new(&variables, context.get_clock());
                            let item_result = self
                                .evaluate_expression(&local_context, map_expression)
                                .await?;
                            result.push(item_result);
                        }
                        None => {
                            result.push(item);
                        }
                    }
                }
                Ok(VariableValue::List(result))
            }
            _ => Err(EvaluationError::InvalidType),
        }
    }

    pub async fn reduce_iterator_expression(
        &self,
        context: &ExpressionEvaluationContext<'_>,
        expression: &ast::IteratorExpression,
        accumulator_variable: Arc<str>,
    ) -> Result<VariableValue, EvaluationError> {
        let items = self
            .evaluate_expression(context, &expression.list_expression)
            .await?;
        match items {
            VariableValue::List(items) => {
                let mut result = match context.get_variable(accumulator_variable.clone()) {
                    Some(value) => value.clone(),
                    None => VariableValue::Null,
                };
                let mut variables = context.clone_variables();
                for item in items {
                    if let Some(filter) = &expression.filter {
                        variables
                            .insert(expression.item_identifier.to_string().into(), item.clone());
                        let local_context =
                            ExpressionEvaluationContext::new(&variables, context.get_clock());

                        if !self.evaluate_predicate(&local_context, filter).await? {
                            continue;
                        }
                    }

                    match &expression.map_expression {
                        Some(map_expression) => {
                            variables.insert(
                                expression.item_identifier.to_string().into(),
                                item.clone(),
                            );
                            let local_context =
                                ExpressionEvaluationContext::new(&variables, context.get_clock());
                            result = self
                                .evaluate_expression(&local_context, map_expression)
                                .await?;
                            variables
                                .insert(accumulator_variable.to_string().into(), result.clone());
                        }
                        None => {}
                    }
                }
                Ok(result)
            }
            _ => Err(EvaluationError::InvalidType),
        }
    }
}

async fn get_date_property(date: NaiveDate, property: String) -> Option<u32> {
    match property.as_str() {
        "year" => Some(date.year() as u32),
        "month" => Some(date.month()),
        "day" => Some(date.day()),
        "quarter" => Some((date.month() - 1) / 3 + 1),
        "week" => Some(date.iso_week().week()),
        "dayOfWeek" => {
            let day_of_week = date.weekday();
            let day_of_week_num = match day_of_week {
                Weekday::Mon => 1,
                Weekday::Tue => 2,
                Weekday::Wed => 3,
                Weekday::Thu => 4,
                Weekday::Fri => 5,
                Weekday::Sat => 6,
                Weekday::Sun => 7,
            };
            Some(day_of_week_num)
        }
        "weekDay" => {
            let day_of_week = date.weekday();
            let day_of_week_num = match day_of_week {
                Weekday::Mon => 1,
                Weekday::Tue => 2,
                Weekday::Wed => 3,
                Weekday::Thu => 4,
                Weekday::Fri => 5,
                Weekday::Sat => 6,
                Weekday::Sun => 7,
            };
            Some(day_of_week_num)
        }
        "ordinalDay" => Some(date.ordinal()),
        "weekYear" => Some(date.iso_week().year() as u32),
        "dayOfQuarter" => {
            let quarter = (date.month() - 1) / 3 + 1;
            let start_date =
                match NaiveDate::from_ymd_opt(date.year(), (quarter - 1) * 3 + 1, 1) {
                    Some(date) => date,
                    None => return None,
                };
            
            let duration = date - start_date;
            let num_days = duration.num_days();
            Some((num_days + 1) as u32)
        }
        "quarterDay" => {
            let quarter = (date.month() - 1) / 3 + 1;
            let start_date =
                match NaiveDate::from_ymd_opt(date.year(), (quarter - 1) * 3 + 1, 1) {
                    Some(date) => date,
                    None => return None,
                };
            let duration = date - start_date;
            let num_days = duration.num_days();
            Some((num_days + 1) as u32)
        }
        _ => None,
    }
}

async fn get_local_time_property(time: NaiveTime, property: String) -> Option<u32> {
    match property.as_str() {
        "hour" => Some(time.hour()),
        "minute" => Some(time.minute()),
        "second" => Some(time.second()),
        "millisecond" => Some(time.nanosecond() / 1000000),
        "microsecond" => Some(time.nanosecond() / 1000),
        "nanosecond" => Some(time.nanosecond()),
        _ => None,
    }
}

async fn get_time_property(zoned_time: ZonedTime, property: String) -> Option<String> {
    let time = zoned_time.time();
    let offset = zoned_time.offset();
    match property.as_str() {
        "hour" => Some(time.hour().to_string()),
        "minute" => Some(time.minute().to_string()),
        "second" => Some(time.second().to_string()),
        "millisecond" => Some((time.nanosecond() / 1000000).to_string()),
        "microsecond" => Some((time.nanosecond() / 1000).to_string()),
        "nanosecond" => Some(time.nanosecond().to_string()),
        "offset" => Some(offset.to_string()),
        "timezone" => Some(offset.to_string()),
        "offsetSeconds" => {
            let seconds = offset.local_minus_utc();
            Some(seconds.to_string())
        }
        "offsetMinutes" => {
            let minutes = offset.local_minus_utc() / 60;
            Some(minutes.to_string())
        }
        _ => None,
    }
}

async fn get_local_datetime_property(datetime: NaiveDateTime, property: String) -> Option<u32> {
    let date = datetime.date();
    let time = datetime.time();
    match property.as_str() {
        "year" => Some(date.year() as u32),
        "month" => Some(date.month()),
        "day" => Some(date.day()),
        "quarter" => Some((date.month() - 1) / 3 + 1),
        "week" => Some(date.iso_week().week()),
        "dayOfWeek" => {
            let day_of_week = date.weekday();
            let day_of_week_num = match day_of_week {
                Weekday::Mon => 1,
                Weekday::Tue => 2,
                Weekday::Wed => 3,
                Weekday::Thu => 4,
                Weekday::Fri => 5,
                Weekday::Sat => 6,
                Weekday::Sun => 7,
            };
            Some(day_of_week_num)
        }
        "weekDay" => {
            let day_of_week = date.weekday();
            let day_of_week_num = match day_of_week {
                Weekday::Mon => 1,
                Weekday::Tue => 2,
                Weekday::Wed => 3,
                Weekday::Thu => 4,
                Weekday::Fri => 5,
                Weekday::Sat => 6,
                Weekday::Sun => 7,
            };
            Some(day_of_week_num)
        }
        "ordinalDay" => Some(date.ordinal()),
        "weekYear" => Some(date.iso_week().year() as u32),
        "dayOfQuarter" => {
            let quarter = (date.month() - 1) / 3 + 1;
            let start_date =
                match NaiveDate::from_ymd_opt(date.year(), (quarter - 1) * 3 + 1, 1) {
                    Some(date) => date,
                    None => return None,
                };
            let duration = date - start_date;
            let num_days = duration.num_days();
            Some((num_days + 1) as u32)
        }
        "quarterDay" => {
            let quarter = (date.month() - 1) / 3 + 1;
            let start_date =
                match NaiveDate::from_ymd_opt(date.year(), (quarter - 1) * 3 + 1, 1) {
                    Some(date) => date,
                    None => return None,
                };
            let duration = date - start_date;
            let num_days = duration.num_days();
            Some((num_days + 1) as u32)
        }
        "hour" => Some(time.hour()),
        "minute" => Some(time.minute()),
        "second" => Some(time.second()),
        "millisecond" => Some(time.nanosecond() / 1000000),
        "microsecond" => Some(time.nanosecond() / 1000),
        "nanosecond" => Some(time.nanosecond()),
        _ => None,
    }
}

async fn get_datetime_property(zoned_datetime: ZonedDateTime, property: String) -> Option<String> {
    let datetime = zoned_datetime.datetime();

    let date = datetime.date_naive();
    let time = datetime.time();
    let offset = datetime.offset();
    let timezone = zoned_datetime.timezone();

    match property.as_str() {
        "year" => Some(date.year().to_string()),
        "month" => Some(date.month().to_string()),
        "day" => Some(date.day().to_string()),
        "quarter" => Some(((date.month() - 1) / 3 + 1).to_string()),
        "week" => Some(date.iso_week().week().to_string()),
        "dayOfWeek" => {
            let day_of_week = date.weekday();
            let day_of_week_num = match day_of_week {
                Weekday::Mon => "1",
                Weekday::Tue => "2",
                Weekday::Wed => "3",
                Weekday::Thu => "4",
                Weekday::Fri => "5",
                Weekday::Sat => "6",
                Weekday::Sun => "7",
            };
            Some(day_of_week_num.to_string())
        }
        "weekDay" => {
            let day_of_week = date.weekday();
            let day_of_week_num = match day_of_week {
                Weekday::Mon => "1",
                Weekday::Tue => "2",
                Weekday::Wed => "3",
                Weekday::Thu => "4",
                Weekday::Fri => "5",
                Weekday::Sat => "6",
                Weekday::Sun => "7",
            };
            Some(day_of_week_num.to_string())
        }
        "ordinalDay" => Some(date.ordinal().to_string()),
        "weekYear" => Some(date.iso_week().year().to_string()),
        "dayOfQuarter" => {
            let quarter = (date.month() - 1) / 3 + 1;
            let start_date =
                match NaiveDate::from_ymd_opt(date.year(), (quarter - 1) * 3 + 1, 1) {
                    Some(date) => date,
                    None => return None,
                };
            let duration = date - start_date;
            let num_days = duration.num_days();
            Some((num_days + 1).to_string())
        }
        "quarterDay" => {
            let quarter = (date.month() - 1) / 3 + 1;
            let start_date =
                match NaiveDate::from_ymd_opt(date.year(), (quarter - 1) * 3 + 1, 1) {
                    Some(date) => date,
                    None => return None,
                };
            let duration = date - start_date;
            let num_days = duration.num_days();
            Some((num_days + 1).to_string())
        }
        "hour" => Some(time.hour().to_string()),
        "minute" => Some(time.minute().to_string()),
        "second" => Some(time.second().to_string()),
        "millisecond" => Some((time.nanosecond() / 1000000).to_string()),
        "microsecond" => Some((time.nanosecond() / 1000).to_string()),
        "nanosecond" => Some(time.nanosecond().to_string()),
        "timezone" => match timezone {
            Some(tz) => Some(tz.to_string()),
            None => Some(offset.to_string()),
        },
        "offset" => Some(offset.to_string()),
        "offsetSeconds" => {
            let seconds = offset.local_minus_utc();
            Some(seconds.to_string())
        }
        "offsetMinutes" => {
            let minutes = offset.local_minus_utc() / 60;
            Some(minutes.to_string())
        }
        "epochMillis" => {
            let epoch = *EPOCH_MIDNIGHT_NAIVE_DATETIME;
            let duration = datetime.naive_utc() - epoch;
            let millis = duration.num_milliseconds();
            Some(millis.to_string())
        }
        "epochSeconds" => {
            let epoch = *EPOCH_MIDNIGHT_NAIVE_DATETIME;
            let duration = datetime.naive_utc() - epoch;
            let seconds = duration.num_seconds();
            Some(seconds.to_string())
        }
        _ => None,
    }
}

async fn get_duration_property(duration_struct: DurationStruct, property: String) -> Option<i64> {
    let year = duration_struct.year();
    let month = duration_struct.month();
    let duration = duration_struct.duration();

    match property.as_str() {
        "years" => Some(year + month / 12 + duration.num_days() / 365),
        "months" => Some(month + year * 12 + duration.num_days() / 30),
        "quarters" => Some(month / 3 + year * 4),
        "weeks" => Some(duration.num_weeks()),
        "days" => Some(duration.num_days()),
        "hours" => Some(duration.num_hours()),
        "minutes" => Some(duration.num_minutes()),
        "seconds" => Some(duration.num_seconds()),
        "milliseconds" => Some(duration.num_milliseconds()),
        "microseconds" => Some(match duration.num_microseconds() {
            Some(micros) => micros,
            None => 0,
        }),
        "nanoseconds" => Some(match duration.num_nanoseconds(){
            Some(nanos) => nanos,
            None => 0,
        }),
        "quartersOfYear" => {
            let quarters = month / 3 + 1;
            Some(quarters)
        }
        "monthsOfYear" => {
            if month % 12 == 0 {
                return Some(12);
            }
            let months = month % 12;
            Some(months)
        }
        "monthsOfQuarter" => {
            if month % 3 == 0 {
                return Some(3);
            }
            let months = month % 3;
            Some(months)
        }
        "daysOfWeek" => {
            if duration.num_days() % 7 == 0 {
                return Some(7);
            }
            let days = duration.num_days() % 7;
            Some(days)
        }
        "minutesOfHour" => {
            let mins = duration.num_minutes() % 60;
            Some(mins)
        }
        "secondsOfMinute" => {
            let secs = duration.num_seconds() % 60;
            Some(secs)
        }
        "millisecondsOfSecond" => {
            let millis = duration.num_milliseconds() % 1000;
            Some(millis)
        }
        "microsecondsOfSecond" => {
            let micros = match duration.num_microseconds(){
                Some(micros) => micros,
                None => 0,
            } % 1000000;
            Some(micros)
        }
        "nanosecondsOfSecond" => {
            let nanos = match duration.num_nanoseconds(){
                Some(nanos) => nanos,
                None => 0,
            } % 1000000000;
            Some(nanos)
        }
        _ => None,
    }
}
