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
        expression: &ast::FunctionExpression,
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
                        match Float::from_f64(match n.as_i64() {
                            Some(i) => i as f64,
                            None => return Err(EvaluationError::FunctionError { 
                                function_name: expression.name.to_string(), 
                                error: Box::new(EvaluationError::ConversionError) }),
                        } as f64) {
                            Some(f) => f,
                            None => return Err(EvaluationError::FunctionError { 
                                function_name: expression.name.to_string(), 
                                error: Box::new(EvaluationError::ConversionError) }),
                        },
                    )),
                    VariableValue::Float(n) => {
                        let input_as_f64 = match n.as_f64() {
                            Some(f) => f,
                            None => return Err(EvaluationError::FunctionError { 
                                function_name: expression.name.to_string(), 
                                error: Box::new(EvaluationError::ConversionError) }),
                        };
                        if input_as_f64.fract() == -0.5 {
                            //Cypher edge case
                            return Ok(VariableValue::Float(
                                match Float::from_f64(input_as_f64.trunc()) {
                                    Some(f) => f,
                                    None => return Err(EvaluationError::FunctionError { 
                                        function_name: expression.name.to_string(), 
                                        error: Box::new(EvaluationError::ConversionError) }),
                                }
                            ));
                        }
                        Ok(VariableValue::Float(
                            match Float::from_f64(match n.as_f64() {
                            Some(f) => f,
                            None => return Err(EvaluationError::FunctionError { 
                                function_name: expression.name.to_string(), 
                                error: Box::new(EvaluationError::ConversionError) }),
                            }.round()) {
                                Some(f) => f,
                                None => return Err(EvaluationError::FunctionError { 
                                    function_name: expression.name.to_string(), 
                                    error: Box::new(EvaluationError::ConversionError) }),
                            },
                        ))
                    }
                    _ => Err(EvaluationError::InvalidType),
                }
            }
            2 => {
                match (&args[0], &args[1]) {
                    (VariableValue::Float(n), VariableValue::Integer(p)) => {
                        let multiplier = 10.0_f64.powi(match p.as_i64() {
                            Some(i) => i as i32,
                            None => return Err(EvaluationError::FunctionError { 
                                function_name: expression.name.to_string(), 
                                error: Box::new(EvaluationError::ConversionError) }),
                        });

                        if match n.as_f64() {
                            Some(f) => f,
                            None => return Err(EvaluationError::FunctionError { 
                                function_name: expression.name.to_string(), 
                                error: Box::new(EvaluationError::ConversionError) }),
                        } > std::f64::MAX / multiplier {
                            return Ok(VariableValue::Float(n.clone()));
                        }
                        let intermediate_value = match n.as_f64() {
                            Some(f) => f,
                            None => return Err(EvaluationError::FunctionError { 
                                function_name: expression.name.to_string(), 
                                error: Box::new(EvaluationError::ConversionError) }),
                        } * multiplier;
                        if intermediate_value.is_sign_negative()
                            && intermediate_value.fract() == -0.5
                        {
                            //Cypher edge case
                            let rounded_value = intermediate_value.trunc() / multiplier;
                            return Ok(VariableValue::Float(
                                match Float::from_f64(rounded_value) {
                                    Some(f) => f,
                                    None => return Err(EvaluationError::FunctionError { 
                                        function_name: expression.name.to_string(), 
                                        error: Box::new(EvaluationError::ConversionError) }),
                                },
                            ));
                        }
                        let rounded_value = (match n.as_f64() {
                            Some(f) => f,
                            None => return Err(EvaluationError::FunctionError { 
                                function_name: expression.name.to_string(), 
                                error: Box::new(EvaluationError::ConversionError) }),
                        } * multiplier).round() / multiplier;
                        Ok(VariableValue::Float(
                            match Float::from_f64(rounded_value) {
                                Some(f) => f,
                                None => return Err(EvaluationError::FunctionError { 
                                    function_name: expression.name.to_string(), 
                                    error: Box::new(EvaluationError::ConversionError) }),
                            },
                        ))
                    }
                    (VariableValue::Integer(n), VariableValue::Integer(_p)) => {
                        Ok(VariableValue::Integer(Integer::from(match n.as_i64() {
                            Some(i) => i as f64,
                            None => return Err(EvaluationError::FunctionError { 
                                function_name: expression.name.to_string(), 
                                error: Box::new(EvaluationError::ConversionError) }),
                        })))
                    }
                    _ => Err(EvaluationError::InvalidType),
                }
            }
            3 => { // Rounding with mode
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
                        let mode = m.to_uppercase();
                        if !valid_modes.contains(&mode) {
                            return Err(EvaluationError::InvalidType);
                        }
                        let is_positive = match n.as_f64() {
                            Some(f) => f,
                            None => return Err(EvaluationError::FunctionError { 
                                function_name: expression.name.to_string(), 
                                error: Box::new(EvaluationError::ConversionError) }),
                        }.is_sign_positive();
                        match mode.as_str() {
                            "UP" => {
                                if is_positive {
                                    let result =
                                        round_up(match n.as_f64() {
                            Some(f) => f,
                            None => return Err(EvaluationError::FunctionError { 
                                function_name: expression.name.to_string(), 
                                error: Box::new(EvaluationError::ConversionError) }),
                        }, match p.as_i64() {
                            Some(i) => i as i32,
                            None => return Err(EvaluationError::FunctionError { 
                                function_name: expression.name.to_string(), 
                                error: Box::new(EvaluationError::ConversionError) }),
                        });
                                    return Ok(VariableValue::Float(
                                        match Float::from_f64(result) {
                                            Some(f) => f,
                                            None => return Err(EvaluationError::FunctionError { 
                                                function_name: expression.name.to_string(), 
                                                error: Box::new(EvaluationError::ConversionError) }),
                                        },
                                    ));
                                } else {
                                    //Cypher being weird :)
                                    let result =
                                        round_down(match n.as_f64() {
                            Some(f) => f,
                            None => return Err(EvaluationError::FunctionError { 
                                function_name: expression.name.to_string(), 
                                error: Box::new(EvaluationError::ConversionError) }),
                        }, match p.as_i64() {
                            Some(i) => i as i32,
                            None => return Err(EvaluationError::FunctionError { 
                                function_name: expression.name.to_string(), 
                                error: Box::new(EvaluationError::ConversionError) }),
                        });
                                    return Ok(VariableValue::Float(
                                        match Float::from_f64(result) {
                                            Some(f) => f,
                                            None => return Err(EvaluationError::FunctionError { 
                                                function_name: expression.name.to_string(), 
                                                error: Box::new(EvaluationError::ConversionError) }),
                                        },
                                    ));
                                }
                            }
                            "DOWN" => {
                                if is_positive {
                                    let result =
                                        round_down(match n.as_f64() {
                            Some(f) => f,
                            None => return Err(EvaluationError::FunctionError { 
                                function_name: expression.name.to_string(), 
                                error: Box::new(EvaluationError::ConversionError) }),
                        }, match p.as_i64() {
                            Some(i) => i as i32,
                            None => return Err(EvaluationError::FunctionError { 
                                function_name: expression.name.to_string(), 
                                error: Box::new(EvaluationError::ConversionError) }),
                        });
                                    return Ok(VariableValue::Float(
                                        match Float::from_f64(result) {
                                            Some(f) => f,
                                            None => return Err(EvaluationError::FunctionError { 
                                                function_name: expression.name.to_string(), 
                                                error: Box::new(EvaluationError::ConversionError) }),
                                        },
                                    ));
                                } else {
                                    let result =
                                        round_up(match n.as_f64() {
                            Some(f) => f,
                            None => return Err(EvaluationError::FunctionError { 
                                function_name: expression.name.to_string(), 
                                error: Box::new(EvaluationError::ConversionError) }),
                        }, match p.as_i64() {
                            Some(i) => i as i32,
                            None => return Err(EvaluationError::FunctionError { 
                                function_name: expression.name.to_string(), 
                                error: Box::new(EvaluationError::ConversionError) }),
                        });
                                    return Ok(VariableValue::Float(
                                        match Float::from_f64(result) {
                                            Some(f) => f,
                                            None => return Err(EvaluationError::FunctionError { 
                                                function_name: expression.name.to_string(), 
                                                error: Box::new(EvaluationError::ConversionError) }),
                                        },
                                    ));
                                }
                            }
                            "CEILING" => {
                                let result =
                                    round_up(match n.as_f64() {
                            Some(f) => f,
                            None => return Err(EvaluationError::FunctionError { 
                                function_name: expression.name.to_string(), 
                                error: Box::new(EvaluationError::ConversionError) }),
                        }, match p.as_i64() {
                            Some(i) => i as i32,
                            None => return Err(EvaluationError::FunctionError { 
                                function_name: expression.name.to_string(), 
                                error: Box::new(EvaluationError::ConversionError) }),
                        });
                                return Ok(VariableValue::Float(match Float::from_f64(result) {
                                    Some(f) => f,
                                    None => return Err(EvaluationError::FunctionError { 
                                        function_name: expression.name.to_string(), 
                                        error: Box::new(EvaluationError::ConversionError) }),
                                }));
                            }
                            "FLOOR" => {
                                let result =
                                    round_down(match n.as_f64() {
                            Some(f) => f,
                            None => return Err(EvaluationError::FunctionError { 
                                function_name: expression.name.to_string(), 
                                error: Box::new(EvaluationError::ConversionError) }),
                        }, match p.as_i64() {
                            Some(i) => i as i32,
                            None => return Err(EvaluationError::FunctionError { 
                                function_name: expression.name.to_string(), 
                                error: Box::new(EvaluationError::ConversionError) }),
                        });
                                return Ok(VariableValue::Float(match Float::from_f64(result) {
                                    Some(f) => f,
                                    None => return Err(EvaluationError::FunctionError { 
                                        function_name: expression.name.to_string(), 
                                        error: Box::new(EvaluationError::ConversionError) }),
                                }));
                            }
                            "HALF_UP" => {
                                let multiplier = 10.0_f64.powi(match p.as_i64() {
                                    Some(i) => i as i32,
                                    None => return Err(EvaluationError::FunctionError { 
                                        function_name: expression.name.to_string(), 
                                        error: Box::new(EvaluationError::ConversionError) }),
                                });
                                if match n.as_f64() {
                            Some(f) => f,
                            None => return Err(EvaluationError::FunctionError { 
                                function_name: expression.name.to_string(), 
                                error: Box::new(EvaluationError::ConversionError) }),
                        } > std::f64::MAX / multiplier {
                                    return Ok(VariableValue::Float(n.clone()));
                                }
                                let intermediate_value = match n.as_f64() {
                            Some(f) => f,
                            None => return Err(EvaluationError::FunctionError { 
                                function_name: expression.name.to_string(), 
                                error: Box::new(EvaluationError::ConversionError) }),
                        } * multiplier;
                                if intermediate_value.fract() == 0.5 {
                                    let rounded_value =
                                        (intermediate_value.trunc() + 1.0) / multiplier;
                                    return Ok(VariableValue::Float(
                                        match Float::from_f64(rounded_value) {
                                            Some(f) => f,
                                            None => return Err(EvaluationError::FunctionError { 
                                                function_name: expression.name.to_string(), 
                                                error: Box::new(EvaluationError::ConversionError) }),
                                        },
                                    ));
                                } else if intermediate_value.fract() == -0.5 {
                                    let rounded_value =
                                        (intermediate_value.trunc() - 1.0) / multiplier;
                                    return Ok(VariableValue::Float(
                                        match Float::from_f64(rounded_value) {
                                            Some(f) => f,
                                            None => return Err(EvaluationError::FunctionError { 
                                                function_name: expression.name.to_string(), 
                                                error: Box::new(EvaluationError::ConversionError) }),
                                        },
                                    ));
                                }
                                let rounded_value =
                                    (match n.as_f64() {
                            Some(f) => f,
                            None => return Err(EvaluationError::FunctionError { 
                                function_name: expression.name.to_string(), 
                                error: Box::new(EvaluationError::ConversionError) }),
                        } * multiplier).round() / multiplier;
                                return Ok(VariableValue::Float(
                                    match Float::from_f64(rounded_value) {
                                        Some(f) => f,
                                        None => return Err(EvaluationError::FunctionError { 
                                            function_name: expression.name.to_string(), 
                                            error: Box::new(EvaluationError::ConversionError) }),
                                    },
                                ));
                            }
                            "HALF_DOWN" => {
                                let multiplier = 10.0_f64.powi(match p.as_i64() {
                                    Some(i) => i as i32,
                                    None => return Err(EvaluationError::FunctionError { 
                                        function_name: expression.name.to_string(), 
                                        error: Box::new(EvaluationError::ConversionError) }),
                                });
                                if match n.as_f64() {
                            Some(f) => f,
                            None => return Err(EvaluationError::FunctionError { 
                                function_name: expression.name.to_string(), 
                                error: Box::new(EvaluationError::ConversionError) }),
                        } > std::f64::MAX / multiplier {
                                    return Ok(VariableValue::Float(n.clone()));
                                }
                                let intermediate_value = match n.as_f64() {
                            Some(f) => f,
                            None => return Err(EvaluationError::FunctionError { 
                                function_name: expression.name.to_string(), 
                                error: Box::new(EvaluationError::ConversionError) }),
                        } * multiplier;
                                if intermediate_value.fract() == 0.5 {
                                    let rounded_value = (intermediate_value.trunc()) / multiplier;
                                    return Ok(VariableValue::Float(
                                        match Float::from_f64(rounded_value) {
                                            Some(f) => f,
                                            None => return Err(EvaluationError::FunctionError { 
                                                function_name: expression.name.to_string(), 
                                                error: Box::new(EvaluationError::ConversionError) }),
                                        },
                                    ));
                                } else if intermediate_value.fract() == -0.5 {
                                    let rounded_value = (intermediate_value.trunc()) / multiplier;
                                    return Ok(VariableValue::Float(
                                        match Float::from_f64(rounded_value) {
                                            Some(f) => f,
                                            None => return Err(EvaluationError::FunctionError { 
                                                function_name: expression.name.to_string(), 
                                                error: Box::new(EvaluationError::ConversionError) }),
                                        },
                                    ));
                                }
                                let rounded_value =
                                    (match n.as_f64() {
                            Some(f) => f,
                            None => return Err(EvaluationError::FunctionError { 
                                function_name: expression.name.to_string(), 
                                error: Box::new(EvaluationError::ConversionError) }),
                        } * multiplier).round() / multiplier;
                                return Ok(VariableValue::Float(
                                    match Float::from_f64(rounded_value) {
                                        Some(f) => f,
                                        None => return Err(EvaluationError::FunctionError { 
                                            function_name: expression.name.to_string(), 
                                            error: Box::new(EvaluationError::ConversionError) }),
                                    },
                                ));
                            }
                            "HALF_EVEN" => {
                                let multiplier = 10.0_f64.powi(match p.as_i64() {
                                    Some(i) => i as i32,
                                    None => return Err(EvaluationError::FunctionError { 
                                        function_name: expression.name.to_string(), 
                                        error: Box::new(EvaluationError::ConversionError) }),
                                });
                                if match n.as_f64() {
                            Some(f) => f,
                            None => return Err(EvaluationError::FunctionError { 
                                function_name: expression.name.to_string(), 
                                error: Box::new(EvaluationError::ConversionError) }),
                        } > std::f64::MAX / multiplier {
                                    return Ok(VariableValue::Float(n.clone()));
                                }
                                let intermediate_value = match n.as_f64() {
                            Some(f) => f,
                            None => return Err(EvaluationError::FunctionError { 
                                function_name: expression.name.to_string(), 
                                error: Box::new(EvaluationError::ConversionError) }),
                        } * multiplier;
                                if intermediate_value.fract() == 0.5 {
                                    if intermediate_value.trunc() % 2.0 == 0.0 {
                                        let rounded_value =
                                            (intermediate_value.trunc()) / multiplier;
                                        return Ok(VariableValue::Float(
                                            match Float::from_f64(rounded_value) {
                                                Some(f) => f,
                                                None => return Err(EvaluationError::FunctionError { 
                                                    function_name: expression.name.to_string(), 
                                                    error: Box::new(EvaluationError::ConversionError) }),
                                            },
                                        ));
                                    } else {
                                        let rounded_value =
                                            (intermediate_value.trunc() + 1.0) / multiplier;
                                        return Ok(VariableValue::Float(
                                            match Float::from_f64(rounded_value) {
                                                Some(f) => f,
                                                None => return Err(EvaluationError::FunctionError { 
                                                    function_name: expression.name.to_string(), 
                                                    error: Box::new(EvaluationError::ConversionError) }),
                                            },
                                        ));
                                    }
                                } else if intermediate_value.fract() == -0.5 {
                                    if intermediate_value.trunc() % 2.0 == 0.0 {
                                        let rounded_value =
                                            (intermediate_value.trunc()) / multiplier;
                                        return Ok(VariableValue::Float(
                                            match Float::from_f64(rounded_value) {
                                                Some(f) => f,
                                                None => return Err(EvaluationError::FunctionError { 
                                                    function_name: expression.name.to_string(), 
                                                    error: Box::new(EvaluationError::ConversionError) }),
                                            },
                                        ));
                                    } else {
                                        let rounded_value =
                                            (intermediate_value.trunc() - 1.0) / multiplier;
                                        return Ok(VariableValue::Float(
                                            match Float::from_f64(rounded_value) {
                                                Some(f) => f,
                                                None => return Err(EvaluationError::FunctionError { 
                                                    function_name: expression.name.to_string(), 
                                                    error: Box::new(EvaluationError::ConversionError) }),
                                            },
                                        ));
                                    }
                                }
                                let rounded_value =
                                    (match n.as_f64() {
                            Some(f) => f,
                            None => return Err(EvaluationError::FunctionError { 
                                function_name: expression.name.to_string(), 
                                error: Box::new(EvaluationError::ConversionError) }),
                        } * multiplier).round() / multiplier;
                                return Ok(VariableValue::Float(
                                    match Float::from_f64(rounded_value) {
                                        Some(f) => f,
                                        None => return Err(EvaluationError::FunctionError { 
                                            function_name: expression.name.to_string(), 
                                            error: Box::new(EvaluationError::ConversionError) }),
                                    },
                                ));
                            }
                            _ => return Err(EvaluationError::InvalidType),
                        }
                    }
                    (
                        VariableValue::Integer(n),
                        VariableValue::Integer(_p),
                        VariableValue::Integer(_m),
                    ) => Ok(VariableValue::Integer(Integer::from(match n.as_i64() {
                            Some(i) => i as f64,
                            None => return Err(EvaluationError::FunctionError { 
                                function_name: expression.name.to_string(), 
                                error: Box::new(EvaluationError::ConversionError) }),
                        }))),
                    _ => Err(EvaluationError::InvalidType),
                }
            }
            _ => Err(EvaluationError::InvalidArgumentCount("round".to_string())),
        }
    }
}
