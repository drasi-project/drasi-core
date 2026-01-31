// Copyright 2024 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use async_trait::async_trait;
use drasi_query_ast::ast;

use crate::evaluation::functions::ScalarFunction;
use crate::evaluation::variable_value::float::Float;
use crate::evaluation::variable_value::integer::Integer;
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::{ExpressionEvaluationContext, FunctionError, FunctionEvaluationError};
use std::collections::HashSet;

use round;
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
    ) -> Result<VariableValue, FunctionError> {
        if args.is_empty() || args.len() > 3 {
            return Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
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
                            None => {
                                return Err(FunctionError {
                                    function_name: expression.name.to_string(),
                                    error: FunctionEvaluationError::OverflowError,
                                })
                            }
                        }) {
                            Some(f) => f,
                            None => {
                                return Err(FunctionError {
                                    function_name: expression.name.to_string(),
                                    error: FunctionEvaluationError::OverflowError,
                                })
                            }
                        },
                    )),
                    VariableValue::Float(n) => {
                        let input_as_f64 = match n.as_f64() {
                            Some(f) => f,
                            None => {
                                return Err(FunctionError {
                                    function_name: expression.name.to_string(),
                                    error: FunctionEvaluationError::OverflowError,
                                })
                            }
                        };
                        if input_as_f64.fract() == -0.5 {
                            //Cypher edge case
                            return Ok(VariableValue::Float(
                                match Float::from_f64(input_as_f64.trunc()) {
                                    Some(f) => f,
                                    None => {
                                        return Err(FunctionError {
                                            function_name: expression.name.to_string(),
                                            error: FunctionEvaluationError::OverflowError,
                                        })
                                    }
                                },
                            ));
                        }
                        Ok(VariableValue::Float(
                            match Float::from_f64(match n.as_f64() {
                                Some(f) => f.round(),
                                None => {
                                    return Err(FunctionError {
                                        function_name: expression.name.to_string(),
                                        error: FunctionEvaluationError::OverflowError,
                                    })
                                }
                            }) {
                                Some(f) => f,
                                None => {
                                    return Err(FunctionError {
                                        function_name: expression.name.to_string(),
                                        error: FunctionEvaluationError::OverflowError,
                                    })
                                }
                            },
                        ))
                    }
                    _ => Err(FunctionError {
                        function_name: expression.name.to_string(),
                        error: FunctionEvaluationError::InvalidArgument(0),
                    }),
                }
            }
            2 => {
                match (&args[0], &args[1]) {
                    (VariableValue::Float(n), VariableValue::Integer(p)) => {
                        let multiplier = 10.0_f64.powi(match p.as_i64() {
                            Some(i) => i as i32,
                            None => {
                                return Err(FunctionError {
                                    function_name: expression.name.to_string(),
                                    error: FunctionEvaluationError::OverflowError,
                                })
                            }
                        });
                        //edge case

                        if match n.as_f64() {
                            Some(f) => f,
                            None => {
                                return Err(FunctionError {
                                    function_name: expression.name.to_string(),
                                    error: FunctionEvaluationError::OverflowError,
                                })
                            }
                        } > f64::MAX / multiplier
                        {
                            return Ok(VariableValue::Float(n.clone()));
                        }
                        let intermediate_value = match n.as_f64() {
                            Some(f) => f,
                            None => {
                                return Err(FunctionError {
                                    function_name: expression.name.to_string(),
                                    error: FunctionEvaluationError::OverflowError,
                                })
                            }
                        } * multiplier;
                        if intermediate_value.is_sign_negative()
                            && intermediate_value.fract() == -0.5
                        {
                            //Cypher edge case
                            let rounded_value = intermediate_value.trunc() / multiplier;
                            return Ok(VariableValue::Float(
                                match Float::from_f64(rounded_value) {
                                    Some(f) => f,
                                    None => {
                                        return Err(FunctionError {
                                            function_name: expression.name.to_string(),
                                            error: FunctionEvaluationError::OverflowError,
                                        })
                                    }
                                },
                            ));
                        }
                        let rounded_value = (match n.as_f64() {
                            Some(f) => f,
                            None => {
                                return Err(FunctionError {
                                    function_name: expression.name.to_string(),
                                    error: FunctionEvaluationError::OverflowError,
                                })
                            }
                        } * multiplier)
                            .round()
                            / multiplier;
                        Ok(VariableValue::Float(match Float::from_f64(rounded_value) {
                            Some(f) => f,
                            None => {
                                return Err(FunctionError {
                                    function_name: expression.name.to_string(),
                                    error: FunctionEvaluationError::OverflowError,
                                })
                            }
                        }))
                    }
                    (VariableValue::Integer(n), VariableValue::Integer(_p)) => {
                        Ok(VariableValue::Integer(Integer::from(match n.as_i64() {
                            Some(i) => i,
                            None => {
                                return Err(FunctionError {
                                    function_name: expression.name.to_string(),
                                    error: FunctionEvaluationError::OverflowError,
                                })
                            }
                        })))
                    }
                    (VariableValue::Float(_n), _) => {
                        return Err(FunctionError {
                            function_name: expression.name.to_string(),
                            error: FunctionEvaluationError::InvalidArgument(1),
                        });
                    }
                    (VariableValue::Integer(_n), _) => {
                        return Err(FunctionError {
                            function_name: expression.name.to_string(),
                            error: FunctionEvaluationError::InvalidArgument(1),
                        });
                    }
                    _ => {
                        return Err(FunctionError {
                            function_name: expression.name.to_string(),
                            error: FunctionEvaluationError::InvalidArgument(0),
                        });
                    }
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
                            return Err(FunctionError {
                                function_name: expression.name.to_string(),
                                error: FunctionEvaluationError::InvalidArgument(2),
                            });
                        }
                        let is_positive = match n.as_f64() {
                            Some(f) => f,
                            None => {
                                return Err(FunctionError {
                                    function_name: expression.name.to_string(),
                                    error: FunctionEvaluationError::OverflowError,
                                })
                            }
                        }
                        .is_sign_positive();
                        match mode.as_str() {
                            "UP" => {
                                if is_positive {
                                    let result = round_up(
                                        match n.as_f64() {
                                            Some(f) => f,
                                            None => {
                                                return Err(FunctionError {
                                                    function_name: expression.name.to_string(),
                                                    error: FunctionEvaluationError::OverflowError,
                                                })
                                            }
                                        },
                                        match p.as_i64() {
                                            Some(i) => i as i32,
                                            None => {
                                                return Err(FunctionError {
                                                    function_name: expression.name.to_string(),
                                                    error: FunctionEvaluationError::OverflowError,
                                                })
                                            }
                                        },
                                    );
                                    return Ok(VariableValue::Float(
                                        match Float::from_f64(result) {
                                            Some(f) => f,
                                            None => {
                                                return Err(FunctionError {
                                                    function_name: expression.name.to_string(),
                                                    error: FunctionEvaluationError::OverflowError,
                                                })
                                            }
                                        },
                                    ));
                                } else {
                                    //Cypher being weird :)
                                    let result = round_down(
                                        match n.as_f64() {
                                            Some(f) => f,
                                            None => {
                                                return Err(FunctionError {
                                                    function_name: expression.name.to_string(),
                                                    error: FunctionEvaluationError::OverflowError,
                                                })
                                            }
                                        },
                                        match p.as_i64() {
                                            Some(i) => i as i32,
                                            None => {
                                                return Err(FunctionError {
                                                    function_name: expression.name.to_string(),
                                                    error: FunctionEvaluationError::OverflowError,
                                                })
                                            }
                                        },
                                    );
                                    return Ok(VariableValue::Float(
                                        match Float::from_f64(result) {
                                            Some(f) => f,
                                            None => {
                                                return Err(FunctionError {
                                                    function_name: expression.name.to_string(),
                                                    error: FunctionEvaluationError::OverflowError,
                                                })
                                            }
                                        },
                                    ));
                                }
                            }
                            "DOWN" => {
                                if is_positive {
                                    let result = round_down(
                                        match n.as_f64() {
                                            Some(f) => f,
                                            None => {
                                                return Err(FunctionError {
                                                    function_name: expression.name.to_string(),
                                                    error: FunctionEvaluationError::OverflowError,
                                                })
                                            }
                                        },
                                        match p.as_i64() {
                                            Some(i) => i as i32,
                                            None => {
                                                return Err(FunctionError {
                                                    function_name: expression.name.to_string(),
                                                    error: FunctionEvaluationError::OverflowError,
                                                })
                                            }
                                        },
                                    );
                                    return Ok(VariableValue::Float(
                                        match Float::from_f64(result) {
                                            Some(f) => f,
                                            None => {
                                                return Err(FunctionError {
                                                    function_name: expression.name.to_string(),
                                                    error: FunctionEvaluationError::OverflowError,
                                                })
                                            }
                                        },
                                    ));
                                } else {
                                    let result = round_up(
                                        match n.as_f64() {
                                            Some(f) => f,
                                            None => {
                                                return Err(FunctionError {
                                                    function_name: expression.name.to_string(),
                                                    error: FunctionEvaluationError::OverflowError,
                                                })
                                            }
                                        },
                                        match p.as_i64() {
                                            Some(i) => i as i32,
                                            None => {
                                                return Err(FunctionError {
                                                    function_name: expression.name.to_string(),
                                                    error: FunctionEvaluationError::OverflowError,
                                                })
                                            }
                                        },
                                    );
                                    return Ok(VariableValue::Float(
                                        match Float::from_f64(result) {
                                            Some(f) => f,
                                            None => {
                                                return Err(FunctionError {
                                                    function_name: expression.name.to_string(),
                                                    error: FunctionEvaluationError::OverflowError,
                                                })
                                            }
                                        },
                                    ));
                                }
                            }
                            "CEILING" => {
                                let result = round_up(
                                    match n.as_f64() {
                                        Some(f) => f,
                                        None => {
                                            return Err(FunctionError {
                                                function_name: expression.name.to_string(),
                                                error: FunctionEvaluationError::OverflowError,
                                            })
                                        }
                                    },
                                    match p.as_i64() {
                                        Some(i) => i as i32,
                                        None => {
                                            return Err(FunctionError {
                                                function_name: expression.name.to_string(),
                                                error: FunctionEvaluationError::OverflowError,
                                            })
                                        }
                                    },
                                );
                                return Ok(VariableValue::Float(match Float::from_f64(result) {
                                    Some(f) => f,
                                    None => {
                                        return Err(FunctionError {
                                            function_name: expression.name.to_string(),
                                            error: FunctionEvaluationError::OverflowError,
                                        })
                                    }
                                }));
                            }
                            "FLOOR" => {
                                let result = round_down(
                                    match n.as_f64() {
                                        Some(f) => f,
                                        None => {
                                            return Err(FunctionError {
                                                function_name: expression.name.to_string(),
                                                error: FunctionEvaluationError::OverflowError,
                                            })
                                        }
                                    },
                                    match p.as_i64() {
                                        Some(i) => i as i32,
                                        None => {
                                            return Err(FunctionError {
                                                function_name: expression.name.to_string(),
                                                error: FunctionEvaluationError::OverflowError,
                                            })
                                        }
                                    },
                                );
                                return Ok(VariableValue::Float(match Float::from_f64(result) {
                                    Some(f) => f,
                                    None => {
                                        return Err(FunctionError {
                                            function_name: expression.name.to_string(),
                                            error: FunctionEvaluationError::OverflowError,
                                        })
                                    }
                                }));
                            }
                            "HALF_UP" => {
                                let multiplier = 10.0_f64.powi(match p.as_i64() {
                                    Some(i) => i as i32,
                                    None => {
                                        return Err(FunctionError {
                                            function_name: expression.name.to_string(),
                                            error: FunctionEvaluationError::OverflowError,
                                        })
                                    }
                                });
                                if match n.as_f64() {
                                    Some(f) => f,
                                    None => {
                                        return Err(FunctionError {
                                            function_name: expression.name.to_string(),
                                            error: FunctionEvaluationError::OverflowError,
                                        })
                                    }
                                } > f64::MAX / multiplier
                                {
                                    return Ok(VariableValue::Float(n.clone()));
                                }
                                let intermediate_value = match n.as_f64() {
                                    Some(f) => f,
                                    None => {
                                        return Err(FunctionError {
                                            function_name: expression.name.to_string(),
                                            error: FunctionEvaluationError::OverflowError,
                                        })
                                    }
                                } * multiplier;
                                if intermediate_value.fract() == 0.5 {
                                    let rounded_value =
                                        (intermediate_value.trunc() + 1.0) / multiplier;
                                    return Ok(VariableValue::Float(
                                        match Float::from_f64(rounded_value) {
                                            Some(f) => f,
                                            None => {
                                                return Err(FunctionError {
                                                    function_name: expression.name.to_string(),
                                                    error: FunctionEvaluationError::OverflowError,
                                                })
                                            }
                                        },
                                    ));
                                } else if intermediate_value.fract() == -0.5 {
                                    let rounded_value =
                                        (intermediate_value.trunc() - 1.0) / multiplier;
                                    return Ok(VariableValue::Float(
                                        match Float::from_f64(rounded_value) {
                                            Some(f) => f,
                                            None => {
                                                return Err(FunctionError {
                                                    function_name: expression.name.to_string(),
                                                    error: FunctionEvaluationError::OverflowError,
                                                })
                                            }
                                        },
                                    ));
                                }
                                let rounded_value = (match n.as_f64() {
                                    Some(f) => f,
                                    None => {
                                        return Err(FunctionError {
                                            function_name: expression.name.to_string(),
                                            error: FunctionEvaluationError::OverflowError,
                                        })
                                    }
                                } * multiplier)
                                    .round()
                                    / multiplier;
                                return Ok(VariableValue::Float(
                                    match Float::from_f64(rounded_value) {
                                        Some(f) => f,
                                        None => {
                                            return Err(FunctionError {
                                                function_name: expression.name.to_string(),
                                                error: FunctionEvaluationError::OverflowError,
                                            })
                                        }
                                    },
                                ));
                            }
                            "HALF_DOWN" => {
                                let multiplier = 10.0_f64.powi(match p.as_i64() {
                                    Some(i) => i as i32,
                                    None => {
                                        return Err(FunctionError {
                                            function_name: expression.name.to_string(),
                                            error: FunctionEvaluationError::OverflowError,
                                        })
                                    }
                                });
                                if match n.as_f64() {
                                    Some(f) => f,
                                    None => {
                                        return Err(FunctionError {
                                            function_name: expression.name.to_string(),
                                            error: FunctionEvaluationError::OverflowError,
                                        })
                                    }
                                } > f64::MAX / multiplier
                                {
                                    return Ok(VariableValue::Float(n.clone()));
                                }
                                let intermediate_value = match n.as_f64() {
                                    Some(f) => f,
                                    None => {
                                        return Err(FunctionError {
                                            function_name: expression.name.to_string(),
                                            error: FunctionEvaluationError::OverflowError,
                                        })
                                    }
                                } * multiplier;
                                if intermediate_value.fract() == 0.5
                                    || intermediate_value.fract() == -0.5
                                {
                                    let rounded_value = (intermediate_value.trunc()) / multiplier;
                                    return Ok(VariableValue::Float(
                                        match Float::from_f64(rounded_value) {
                                            Some(f) => f,
                                            None => {
                                                return Err(FunctionError {
                                                    function_name: expression.name.to_string(),
                                                    error: FunctionEvaluationError::OverflowError,
                                                })
                                            }
                                        },
                                    ));
                                }
                                let rounded_value = (match n.as_f64() {
                                    Some(f) => f,
                                    None => {
                                        return Err(FunctionError {
                                            function_name: expression.name.to_string(),
                                            error: FunctionEvaluationError::OverflowError,
                                        })
                                    }
                                } * multiplier)
                                    .round()
                                    / multiplier;
                                return Ok(VariableValue::Float(
                                    match Float::from_f64(rounded_value) {
                                        Some(f) => f,
                                        None => {
                                            return Err(FunctionError {
                                                function_name: expression.name.to_string(),
                                                error: FunctionEvaluationError::OverflowError,
                                            })
                                        }
                                    },
                                ));
                            }
                            "HALF_EVEN" => {
                                let multiplier = 10.0_f64.powi(match p.as_i64() {
                                    Some(i) => i as i32,
                                    None => {
                                        return Err(FunctionError {
                                            function_name: expression.name.to_string(),
                                            error: FunctionEvaluationError::OverflowError,
                                        })
                                    }
                                });
                                if match n.as_f64() {
                                    Some(f) => f,
                                    None => {
                                        return Err(FunctionError {
                                            function_name: expression.name.to_string(),
                                            error: FunctionEvaluationError::OverflowError,
                                        })
                                    }
                                } > f64::MAX / multiplier
                                {
                                    return Ok(VariableValue::Float(n.clone()));
                                }
                                let intermediate_value = match n.as_f64() {
                                    Some(f) => f,
                                    None => {
                                        return Err(FunctionError {
                                            function_name: expression.name.to_string(),
                                            error: FunctionEvaluationError::OverflowError,
                                        })
                                    }
                                } * multiplier;
                                if intermediate_value.fract() == 0.5 {
                                    if intermediate_value.trunc() % 2.0 == 0.0 {
                                        let rounded_value =
                                            (intermediate_value.trunc()) / multiplier;
                                        return Ok(VariableValue::Float(
                                            match Float::from_f64(rounded_value) {
                                                Some(f) => f,
                                                None => {
                                                    return Err(FunctionError {
                                                        function_name: expression.name.to_string(),
                                                        error:
                                                            FunctionEvaluationError::OverflowError,
                                                    })
                                                }
                                            },
                                        ));
                                    } else {
                                        let rounded_value =
                                            (intermediate_value.trunc() + 1.0) / multiplier;
                                        return Ok(VariableValue::Float(
                                            match Float::from_f64(rounded_value) {
                                                Some(f) => f,
                                                None => {
                                                    return Err(FunctionError {
                                                        function_name: expression.name.to_string(),
                                                        error:
                                                            FunctionEvaluationError::OverflowError,
                                                    })
                                                }
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
                                                None => {
                                                    return Err(FunctionError {
                                                        function_name: expression.name.to_string(),
                                                        error:
                                                            FunctionEvaluationError::OverflowError,
                                                    })
                                                }
                                            },
                                        ));
                                    } else {
                                        let rounded_value =
                                            (intermediate_value.trunc() - 1.0) / multiplier;
                                        return Ok(VariableValue::Float(
                                            match Float::from_f64(rounded_value) {
                                                Some(f) => f,
                                                None => {
                                                    return Err(FunctionError {
                                                        function_name: expression.name.to_string(),
                                                        error:
                                                            FunctionEvaluationError::OverflowError,
                                                    })
                                                }
                                            },
                                        ));
                                    }
                                }
                                let rounded_value = (match n.as_f64() {
                                    Some(f) => f,
                                    None => {
                                        return Err(FunctionError {
                                            function_name: expression.name.to_string(),
                                            error: FunctionEvaluationError::OverflowError,
                                        })
                                    }
                                } * multiplier)
                                    .round()
                                    / multiplier;
                                return Ok(VariableValue::Float(
                                    match Float::from_f64(rounded_value) {
                                        Some(f) => f,
                                        None => {
                                            return Err(FunctionError {
                                                function_name: expression.name.to_string(),
                                                error: FunctionEvaluationError::OverflowError,
                                            })
                                        }
                                    },
                                ));
                            }
                            _ => {
                                return Err(FunctionError {
                                    function_name: expression.name.to_string(),
                                    error: FunctionEvaluationError::InvalidArgument(2),
                                })
                            }
                        }
                    }
                    (
                        VariableValue::Integer(n),
                        VariableValue::Integer(_p),
                        VariableValue::Integer(_m),
                    ) => Ok(VariableValue::Integer(Integer::from(match n.as_i64() {
                        Some(i) => i,
                        None => {
                            return Err(FunctionError {
                                function_name: expression.name.to_string(),
                                error: FunctionEvaluationError::OverflowError,
                            })
                        }
                    }))),
                    _ => Err(FunctionError {
                        function_name: expression.name.to_string(),
                        error: FunctionEvaluationError::InvalidArgument(2),
                    }),
                }
            }
            _ => unreachable!(),
        }
    }
}
