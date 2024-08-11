use crate::evaluation::variable_value::VariableValue;
use async_trait::async_trait;
use drasi_query_ast::ast;

use crate::evaluation::functions::ScalarFunction;
use crate::evaluation::{FunctionError, FunctionEvaluationError, ExpressionEvaluationContext};

#[derive(Debug, PartialEq)]
pub struct ToUpper {}

#[async_trait]
impl ScalarFunction for ToUpper {
    async fn call(
        &self,
        _context: &ExpressionEvaluationContext,
        expression: &ast::FunctionExpression,
        args: Vec<VariableValue>,
    ) -> Result<VariableValue, FunctionError> {
        if args.len() != 1 {
            return Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }
        match &args[0] {
            VariableValue::String(s) => Ok(VariableValue::String(s.to_uppercase())),
            VariableValue::Null => Ok(VariableValue::Null),
            _ => Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgument(0),
            }),
        }
    }
}

#[derive(Debug)]
pub struct ToLower {}

#[async_trait]
impl ScalarFunction for ToLower {
    async fn call(
        &self,
        _context: &ExpressionEvaluationContext,
        expression: &ast::FunctionExpression,
        args: Vec<VariableValue>,
    ) -> Result<VariableValue, FunctionError> {
        if args.len() != 1 {
            return Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }
        match &args[0] {
            VariableValue::String(s) => Ok(VariableValue::String(s.to_lowercase())),
            VariableValue::Null => Ok(VariableValue::Null),
            _ => Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgument(0),
            }),
        }
    }
}

#[derive(Debug)]
pub struct Trim {}

#[async_trait]
impl ScalarFunction for Trim {
    async fn call(
        &self,
        _context: &ExpressionEvaluationContext,
        expression: &ast::FunctionExpression,
        args: Vec<VariableValue>,
    ) -> Result<VariableValue, FunctionError> {
        if args.len() != 1 {
            return Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }
        match &args[0] {
            VariableValue::String(s) => Ok(VariableValue::String(s.trim().to_string())),
            VariableValue::Null => Ok(VariableValue::Null),
            _ => Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgument(0),
            }),
        }
    }
}

#[derive(Debug)]
pub struct LTrim {}

#[async_trait]
impl ScalarFunction for LTrim {
    async fn call(
        &self,
        _context: &ExpressionEvaluationContext,
        expression: &ast::FunctionExpression,
        args: Vec<VariableValue>,
    ) -> Result<VariableValue, FunctionError> {
        if args.len() != 1 {
            return Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }
        match &args[0] {
            VariableValue::String(s) => Ok(VariableValue::String(s.trim_start().to_string())),
            VariableValue::Null => Ok(VariableValue::Null),
            _ => Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgument(0),
            }),
        }
    }
}

#[derive(Debug)]
pub struct RTrim {}

#[async_trait]
impl ScalarFunction for RTrim {
    async fn call(
        &self,
        _context: &ExpressionEvaluationContext,
        expression: &ast::FunctionExpression,
        args: Vec<VariableValue>,
    ) -> Result<VariableValue, FunctionError> {
        if args.len() != 1 {
            return Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }
        match &args[0] {
            VariableValue::String(s) => Ok(VariableValue::String(s.trim_end().to_string())),
            VariableValue::Null => Ok(VariableValue::Null),
            _ => Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgument(0),
            }),
        }
    }
}

#[derive(Debug)]
pub struct Reverse {}

#[async_trait]
impl ScalarFunction for Reverse {
    async fn call(
        &self,
        _context: &ExpressionEvaluationContext,
        expression: &ast::FunctionExpression,
        args: Vec<VariableValue>,
    ) -> Result<VariableValue, FunctionError> {
        if args.len() != 1 {
            return Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }
        match &args[0] {
            VariableValue::String(s) => Ok(VariableValue::String(s.chars().rev().collect())),
            VariableValue::List(l) => {
                let mut l = l.clone();
                l.reverse();
                Ok(VariableValue::List(l))
            }
            VariableValue::Null => Ok(VariableValue::Null),
            _ => Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgument(0),
            }),
        }
    }
}

#[derive(Debug)]
pub struct Left {}

#[async_trait]
impl ScalarFunction for Left {
    async fn call(
        &self,
        _context: &ExpressionEvaluationContext,
        expression: &ast::FunctionExpression,
        args: Vec<VariableValue>,
    ) -> Result<VariableValue, FunctionError> {
        if args.len() != 2 {
            return Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }
        match (&args[0], &args[1]) {
            (VariableValue::Null, VariableValue::Integer(_length)) => Ok(VariableValue::Null),
            (VariableValue::Null, VariableValue::Null) => Ok(VariableValue::Null),
            (VariableValue::String(original), VariableValue::Integer(length)) => {
                let len = match length.as_i64() {
                    Some(l) => {
                        if l <= 0 {
                            return Err(FunctionError {
                                function_name: expression.name.to_string(),
                                error: FunctionEvaluationError::OverflowError,
                            });
                        }
                        l as usize
                    }
                    None => return Err(FunctionError {
                        function_name: expression.name.to_string(),
                        error: FunctionEvaluationError::InvalidArgument(1),
                    }),
                };
                if len > original.len() {
                    return Ok(VariableValue::String(original.to_string()));
                }
                let result = original.chars().take(len).collect::<String>();
                Ok(VariableValue::String(result))
            }
            (VariableValue::String(original), _) => return Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgument(1),
            }),
            _ => Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgument(0),
            }),
        }
    }
}

#[derive(Debug)]
pub struct Right {}

#[async_trait]
impl ScalarFunction for Right {
    async fn call(
        &self,
        _context: &ExpressionEvaluationContext,
        expression: &ast::FunctionExpression,
        args: Vec<VariableValue>,
    ) -> Result<VariableValue, FunctionError> {
        if args.len() != 2 {
            return Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }
        match (&args[0], &args[1]) {
            (VariableValue::Null, VariableValue::Integer(_length)) => Ok(VariableValue::Null),
            (VariableValue::Null, VariableValue::Null) => Ok(VariableValue::Null),
            (VariableValue::String(original), VariableValue::Integer(length)) => {
                let len = match length.as_i64() {
                    Some(l) => {
                        if l <= 0 {
                            return Err(FunctionError {
                                function_name: expression.name.to_string(),
                                error: FunctionEvaluationError::OverflowError,
                            });
                        }
                        l as usize
                    }
                    None => return Err(FunctionError {
                        function_name: expression.name.to_string(),
                        error: FunctionEvaluationError::InvalidArgument(1),
                    }),
                };
                if len > original.len() {
                    return Ok(VariableValue::String(original.to_string()));
                }
                let start_index = original.len() - len;
                let result = original[start_index..].to_string();
                Ok(VariableValue::String(result))
            }
            (VariableValue::String(_), _) => return Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgument(1),
            }),
            _ => Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgument(0),
            }),
        }
    }
}

#[derive(Debug)]
pub struct Replace {}

#[async_trait]
impl ScalarFunction for Replace {
    async fn call(
        &self,
        _context: &ExpressionEvaluationContext,
        expression: &ast::FunctionExpression,
        args: Vec<VariableValue>,
    ) -> Result<VariableValue, FunctionError> {
        if args.len() != 3 {
            return Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }
        match (&args[0], &args[1], &args[2]) {
            (
                VariableValue::String(original),
                VariableValue::String(search),
                VariableValue::String(replace),
            ) => {
                if search.is_empty() {
                    return Ok(VariableValue::String(original.to_string()));
                }
                let result = original.replace(search, replace);
                return Ok(VariableValue::String(result));
            }
            
            (VariableValue::Null, _, _) => Ok(VariableValue::Null),
            (_, VariableValue::Null, _) => Ok(VariableValue::Null),
            (_, _, VariableValue::Null) => Ok(VariableValue::Null),
            (VariableValue::String(_), VariableValue::String(_), _) => Err(FunctionError{
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgument(2),
            }),
            (_, VariableValue::String(_), _) =>  Err(FunctionError{
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgument(0),
            }),
            _ => Err(FunctionError{
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgument(0),
            }),
            
        }
    }
}

#[derive(Debug)]
pub struct Split {}

#[async_trait]
impl ScalarFunction for Split {
    async fn call(
        &self,
        _context: &ExpressionEvaluationContext,
        expression: &ast::FunctionExpression,
        args: Vec<VariableValue>,
    ) -> Result<VariableValue, FunctionError> {
        if args.len() != 2 {
            return Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }
        match (&args[0], &args[1]) {
            (VariableValue::Null, _) => Ok(VariableValue::Null),
            (_, VariableValue::Null) => Ok(VariableValue::Null),
            (VariableValue::String(original), VariableValue::String(separator)) => {
                if separator.is_empty() {
                    return Err(FunctionError {
                        function_name: expression.name.to_string(),
                        error: FunctionEvaluationError::InvalidArgument(1),
                    });
                }
                let result = original
                    .split(separator)
                    .map(|s| VariableValue::String(s.to_string()))
                    .collect::<Vec<VariableValue>>();
                return Ok(VariableValue::List(result));
            }
            (VariableValue::String(original), VariableValue::List(delimiters)) => {
                // An array of delimiters
                let mut delimiters_vector = Vec::new();
                let mut result = Vec::new();
                let mut current_word = String::new();

                for delimiter in delimiters {
                    match delimiter {
                        VariableValue::String(s) => delimiters_vector.push(s),
                        _ => return Err(FunctionError {
                            function_name: expression.name.to_string(),
                            error: FunctionEvaluationError::InvalidArgument(1),
                        }),
                    }
                }
                for c in original.chars() {
                    if delimiters_vector.iter().any(|d| d.contains(c)) {
                        if !current_word.is_empty() {
                            result.push(VariableValue::String(current_word.clone()));
                            current_word.clear();
                        }
                    } else {
                        current_word.push(c);
                    }
                }
                if !current_word.is_empty() {
                    result.push(VariableValue::String(current_word));
                }
                return Ok(VariableValue::List(result));
            }
            _ => Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgument(0),
            }),
        }
    }
}

#[derive(Debug)]
pub struct Substring {}

#[async_trait]
impl ScalarFunction for Substring {
    async fn call(
        &self,
        _context: &ExpressionEvaluationContext,
        expression: &ast::FunctionExpression,
        args: Vec<VariableValue>,
    ) -> Result<VariableValue, FunctionError> {
        if args.len() < 2 || args.len() > 3 {
            return Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }
        match (&args[0], &args[1], &args.get(2)) {
            (VariableValue::Null, _, _) => Ok(VariableValue::Null),
            (_, VariableValue::Null, _) => Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgument(1),
            }),
            (_, _, Some(VariableValue::Null)) => Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgument(2),
            }),
            (VariableValue::String(original), VariableValue::Integer(start), None) => {
                // Handle case with two arguments
                let start_index = match start.as_i64() {
                    Some(s) => {
                        if s < 0 {
                            return Err(FunctionError {
                                function_name: expression.name.to_string(),
                                error: FunctionEvaluationError::InvalidType { expected: "positive integer".to_string() },
                            });
                        }
                        s as usize
                    }
                    None => return Err(FunctionError {
                        function_name: expression.name.to_string(),
                        error: FunctionEvaluationError::InvalidArgument(1),
                    }),
                };
                if start_index > original.len() {
                    return Err(FunctionError {
                        function_name: expression.name.to_string(),
                        error: FunctionEvaluationError::InvalidArgument(1),
                    });
                }
                return Ok(VariableValue::String(original[start_index..].to_string()));
            },
            (VariableValue::String(_), _, None) => {
                return Err(FunctionError {
                    function_name: expression.name.to_string(),
                    error: FunctionEvaluationError::InvalidArgument(1),
                });
            }
            (
                VariableValue::String(original),
                VariableValue::Integer(start),
                Some(VariableValue::Integer(length)),
            ) => {

                let start_index = match start.as_i64() {
                    Some(s) => {
                        if s < 0 {
                            return Err(FunctionError {
                                function_name: expression.name.to_string(),
                                error: FunctionEvaluationError::InvalidType { expected: "positive integer".to_string() },
                            });
                        }
                        s as usize
                    }
                    None => return Err(FunctionError {
                        function_name: expression.name.to_string(),
                        error: FunctionEvaluationError::InvalidArgument(1),
                    }),
                };

                let len = match length.as_i64() {
                    Some(l) => {
                        if l < 0 {
                            return Err(FunctionError {
                                function_name: expression.name.to_string(),
                                error: FunctionEvaluationError::InvalidType { expected: "positive integer".to_string() },
                            });
                        }
                        l as usize
                    }
                    None => return Err(FunctionError {
                        function_name: expression.name.to_string(),
                        error: FunctionEvaluationError::InvalidArgument(2),
                    }),
                };
                if start_index > original.len() || start_index + len - start_index > original.len()
                {
                    return Err(FunctionError {
                        function_name: expression.name.to_string(),
                        error: FunctionEvaluationError::InvalidType {
                            expected: "Valid index or length".to_string(),
                        },
                    });
                }
                return Ok(VariableValue::String(
                    original[start_index..start_index + len].to_string(),
                ));
            }
            (VariableValue::String(_), VariableValue::Integer(_), _ ) => {
                return Err(FunctionError {
                    function_name: expression.name.to_string(),
                    error: FunctionEvaluationError::InvalidArgument(2),
                });
            }
            _ => return Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgument(0),
            }),
        }
    }
}

pub struct ToString {}

#[async_trait]
impl ScalarFunction for ToString {
    async fn call(
        &self,
        _context: &ExpressionEvaluationContext,
        expression: &ast::FunctionExpression,
        args: Vec<VariableValue>,
    ) -> Result<VariableValue, FunctionError> {
        if args.len() != 1 {
            return Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }
        match &args[0] {
            VariableValue::Integer(n) => return Ok(VariableValue::String(n.to_string())),
            VariableValue::Float(n) => return Ok(VariableValue::String(n.to_string())),
            VariableValue::String(s) => return Ok(VariableValue::String(s.to_string())),
            VariableValue::Bool(b) => return Ok(VariableValue::String(b.to_string())),
            VariableValue::List(a) => {
                let mut result = String::new();
                result.push('[');
                for v in a {
                    result.push_str(&v.to_string());
                    result.push_str(", ");
                }
                result.truncate(result.len() - 2);
                result.push(']');
                return Ok(VariableValue::String(result));
            }
            VariableValue::Null => return Ok(VariableValue::Null),
            _ => return Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgument(0),
            }),
        }
    }
}

pub struct ToStringOrNull {}

#[async_trait]
impl ScalarFunction for ToStringOrNull {
    async fn call(
        &self,
        _context: &ExpressionEvaluationContext,
        expression: &ast::FunctionExpression,
        args: Vec<VariableValue>,
    ) -> Result<VariableValue, FunctionError> {
        if args.len() != 1 {
            return Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }
        match &args[0] {
            VariableValue::Null => return Ok(VariableValue::Null),
            VariableValue::Integer(n) => return Ok(VariableValue::String(n.to_string())),
            VariableValue::Float(n) => return Ok(VariableValue::String(n.to_string())),
            VariableValue::String(s) => return Ok(VariableValue::String(s.to_string())),
            VariableValue::List(a) => {
                let mut result = String::new();
                result.push('[');
                for v in a {
                    result.push_str(&v.to_string());
                    result.push_str(", ");
                }
                result.truncate(result.len() - 2);
                result.push(']');
                return Ok(VariableValue::String(result));
            }
            VariableValue::Object(_o) => {
                // For objects should return null
                return Ok(VariableValue::Null);
            }
            _ => return Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgument(0),
            }),
        }
    }
}
