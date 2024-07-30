use async_trait::async_trait;
use drasi_query_ast::ast;

use crate::evaluation::functions::ScalarFunction;
use crate::evaluation::variable_value::float::Float;
use crate::evaluation::variable_value::integer::Integer;
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::{EvaluationError, ExpressionEvaluationContext};
use std::collections::HashSet;

extern crate round;
use round::{round_down, round_up};

#[derive(Debug)]
pub struct Round {}

#[async_trait]
impl ScalarFunction for Round {
    async fn call(
        &self,
        _context: &ExpressionEvaluationContext,
        _expression: &ast::FunctionExpression,
        args: Vec<VariableValue>,
    ) -> Result<VariableValue, EvaluationError> {
        if args.is_empty() || args.len() > 3 {
            return Err(EvaluationError::InvalidArgumentCount("round".to_string()));
        }
        if args.contains(&VariableValue::Null) {
            return Ok(VariableValue::Null);
        }
        match args.len() {
            1 => {
                match &args[0] {
                    VariableValue::Null => Ok(VariableValue::Null),
                    VariableValue::Integer(n) => Ok(VariableValue::Float(
                        Float::from_f64(n.as_i64().unwrap() as f64).unwrap(),
                    )),
                    VariableValue::Float(n) => {
                        let input_as_f64 = n.as_f64().unwrap();
                        if input_as_f64.fract() == -0.5 {
                            //Cypher edge case
                            return Ok(VariableValue::Float(
                                Float::from_f64(input_as_f64.trunc()).unwrap(),
                            ));
                        }
                        Ok(VariableValue::Float(
                            Float::from_f64(n.as_f64().unwrap().round()).unwrap(),
                        ))
                    }
                    _ => Err(EvaluationError::InvalidType),
                }
            }
            2 => {
                match (&args[0], &args[1]) {
                    (VariableValue::Float(n), VariableValue::Integer(p)) => {
                        let multiplier = 10.0_f64.powi(p.as_i64().unwrap() as i32);
                        //edge case

                        if n.as_f64().unwrap() > std::f64::MAX / multiplier {
                            return Ok(VariableValue::Float(n.clone()));
                        }
                        let intermediate_value = n.as_f64().unwrap() * multiplier;
                        if intermediate_value.is_sign_negative()
                            && intermediate_value.fract() == -0.5
                        {
                            //Cypher edge case
                            let rounded_value = intermediate_value.trunc() / multiplier;
                            return Ok(VariableValue::Float(
                                Float::from_f64(rounded_value).unwrap(),
                            ));
                        }
                        let rounded_value = (n.as_f64().unwrap() * multiplier).round() / multiplier;
                        Ok(VariableValue::Float(
                            Float::from_f64(rounded_value).unwrap(),
                        ))
                    }
                    (VariableValue::Integer(n), VariableValue::Integer(_p)) => {
                        Ok(VariableValue::Integer(Integer::from(n.as_i64().unwrap())))
                    }
                    _ => Err(EvaluationError::InvalidType),
                }
            }
            3 => {
                match (&args[0], &args[1], &args[2]) {
                    (
                        VariableValue::Float(n),
                        VariableValue::Integer(p),
                        VariableValue::String(m),
                    ) => {
                        let valid_modes: HashSet<String> = [
                            "UP",
                            "DOWN",
                            "CEILING",
                            "FLOOR",
                            "HALF_UP",
                            "HALF_DOWN",
                            "HALF_EVEN",
                        ]
                        .iter()
                        .map(|s| s.to_string())
                        .collect();
                        // let valid_keys: HashSet<String> = vec!["year", "month", "week", "day", "ordinalDay", "quarter", "dayOfWeek", "dayOfQuarter"].iter().map(|s| s.to_string()).collect();
                        let mode = m.to_uppercase();
                        if !valid_modes.contains(&mode) {
                            return Err(EvaluationError::InvalidType);
                        }
                        let is_positive = n.as_f64().unwrap().is_sign_positive();
                        match mode.as_str() {
                            "UP" => {
                                if is_positive {
                                    let result =
                                        round_up(n.as_f64().unwrap(), p.as_i64().unwrap() as i32);
                                    return Ok(VariableValue::Float(
                                        Float::from_f64(result).unwrap(),
                                    ));
                                } else {
                                    //Cypher being weird :)
                                    let result =
                                        round_down(n.as_f64().unwrap(), p.as_i64().unwrap() as i32);
                                    return Ok(VariableValue::Float(
                                        Float::from_f64(result).unwrap(),
                                    ));
                                }
                            }
                            "DOWN" => {
                                if is_positive {
                                    let result =
                                        round_down(n.as_f64().unwrap(), p.as_i64().unwrap() as i32);
                                    return Ok(VariableValue::Float(
                                        Float::from_f64(result).unwrap(),
                                    ));
                                } else {
                                    let result =
                                        round_up(n.as_f64().unwrap(), p.as_i64().unwrap() as i32);
                                    return Ok(VariableValue::Float(
                                        Float::from_f64(result).unwrap(),
                                    ));
                                }
                            }
                            "CEILING" => {
                                let result =
                                    round_up(n.as_f64().unwrap(), p.as_i64().unwrap() as i32);
                                return Ok(VariableValue::Float(Float::from_f64(result).unwrap()));
                            }
                            "FLOOR" => {
                                let result =
                                    round_down(n.as_f64().unwrap(), p.as_i64().unwrap() as i32);
                                return Ok(VariableValue::Float(Float::from_f64(result).unwrap()));
                            }
                            "HALF_UP" => {
                                let multiplier = 10.0_f64.powi(p.as_i64().unwrap() as i32);
                                if n.as_f64().unwrap() > std::f64::MAX / multiplier {
                                    return Ok(VariableValue::Float(n.clone()));
                                }
                                let intermediate_value = n.as_f64().unwrap() * multiplier;
                                if intermediate_value.fract() == 0.5 {
                                    let rounded_value =
                                        (intermediate_value.trunc() + 1.0) / multiplier;
                                    return Ok(VariableValue::Float(
                                        Float::from_f64(rounded_value).unwrap(),
                                    ));
                                } else if intermediate_value.fract() == -0.5 {
                                    let rounded_value =
                                        (intermediate_value.trunc() - 1.0) / multiplier;
                                    return Ok(VariableValue::Float(
                                        Float::from_f64(rounded_value).unwrap(),
                                    ));
                                }
                                let rounded_value =
                                    (n.as_f64().unwrap() * multiplier).round() / multiplier;
                                return Ok(VariableValue::Float(
                                    Float::from_f64(rounded_value).unwrap(),
                                ));
                            }
                            "HALF_DOWN" => {
                                let multiplier = 10.0_f64.powi(p.as_i64().unwrap() as i32);
                                if n.as_f64().unwrap() > std::f64::MAX / multiplier {
                                    return Ok(VariableValue::Float(n.clone()));
                                }
                                let intermediate_value = n.as_f64().unwrap() * multiplier;
                                if intermediate_value.fract() == 0.5 {
                                    let rounded_value = (intermediate_value.trunc()) / multiplier;
                                    return Ok(VariableValue::Float(
                                        Float::from_f64(rounded_value).unwrap(),
                                    ));
                                } else if intermediate_value.fract() == -0.5 {
                                    let rounded_value = (intermediate_value.trunc()) / multiplier;
                                    return Ok(VariableValue::Float(
                                        Float::from_f64(rounded_value).unwrap(),
                                    ));
                                }
                                let rounded_value =
                                    (n.as_f64().unwrap() * multiplier).round() / multiplier;
                                return Ok(VariableValue::Float(
                                    Float::from_f64(rounded_value).unwrap(),
                                ));
                            }
                            "HALF_EVEN" => {
                                let multiplier = 10.0_f64.powi(p.as_i64().unwrap() as i32);
                                if n.as_f64().unwrap() > std::f64::MAX / multiplier {
                                    return Ok(VariableValue::Float(n.clone()));
                                }
                                let intermediate_value = n.as_f64().unwrap() * multiplier;
                                if intermediate_value.fract() == 0.5 {
                                    if intermediate_value.trunc() % 2.0 == 0.0 {
                                        let rounded_value =
                                            (intermediate_value.trunc()) / multiplier;
                                        return Ok(VariableValue::Float(
                                            Float::from_f64(rounded_value).unwrap(),
                                        ));
                                    } else {
                                        let rounded_value =
                                            (intermediate_value.trunc() + 1.0) / multiplier;
                                        return Ok(VariableValue::Float(
                                            Float::from_f64(rounded_value).unwrap(),
                                        ));
                                    }
                                } else if intermediate_value.fract() == -0.5 {
                                    if intermediate_value.trunc() % 2.0 == 0.0 {
                                        let rounded_value =
                                            (intermediate_value.trunc()) / multiplier;
                                        return Ok(VariableValue::Float(
                                            Float::from_f64(rounded_value).unwrap(),
                                        ));
                                    } else {
                                        let rounded_value =
                                            (intermediate_value.trunc() - 1.0) / multiplier;
                                        return Ok(VariableValue::Float(
                                            Float::from_f64(rounded_value).unwrap(),
                                        ));
                                    }
                                }
                                let rounded_value =
                                    (n.as_f64().unwrap() * multiplier).round() / multiplier;
                                return Ok(VariableValue::Float(
                                    Float::from_f64(rounded_value).unwrap(),
                                ));
                            }
                            _ => return Err(EvaluationError::InvalidType),
                        }
                    }
                    (
                        VariableValue::Integer(n),
                        VariableValue::Integer(_p),
                        VariableValue::Integer(_m),
                    ) => Ok(VariableValue::Integer(Integer::from(n.as_i64().unwrap()))),
                    _ => Err(EvaluationError::InvalidType),
                }
            }
            _ => Err(EvaluationError::InvalidArgumentCount("round".to_string())),
        }
    }
}
