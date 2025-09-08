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

use std::sync::Arc;

use drasi_query_ast::ast;

use crate::evaluation::context::QueryVariables;
use crate::evaluation::functions::cypher_scalar::timestamp;
use crate::evaluation::functions::ScalarFunction;
use crate::evaluation::{ExpressionEvaluationContext, InstantQueryClock};

fn get_func_expr() -> ast::FunctionExpression {
    ast::FunctionExpression {
        name: Arc::from("function"),
        args: vec![],
        position_in_query: 10,
    }
}

#[tokio::test]
async fn test_timestamp() {
    let timestamp = timestamp::Timestamp {};

    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![];
    let result = match timestamp
        .call(&context, &get_func_expr(), args.clone())
        .await
    {
        Ok(result) => result,
        Err(e) => panic!("Error: {e:?}"),
    };

    let time_since_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    let time_since_epoch = time_since_epoch.as_millis() as i64;

    //abs diff should be less than 500
    assert!((result.as_i64().unwrap() - time_since_epoch).abs() < 500);
}
