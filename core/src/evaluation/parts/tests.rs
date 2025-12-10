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

use drasi_query_ast::api::QueryParser;
use drasi_query_ast::ast::Query;
use drasi_query_cypher::CypherParser;

use crate::evaluation::{functions::FunctionRegistry, InstantQueryClock};

use super::*;

macro_rules! variablemap {
  ($( $key: expr => $val: expr ),*) => {{
       let mut map = ::std::collections::BTreeMap::new();
       $( map.insert($key.to_string().into_boxed_str(), VariableValue::from($val)); )*
       map
  }}
}

mod multi_part;
mod single_part_aggregating;
mod single_part_non_aggregating;

async fn process_solution(
    query: &Query,
    evaluator: &QueryPartEvaluator,
    context: QueryPartEvaluationContext,
) -> Vec<QueryPartEvaluationContext> {
    let mut result = Vec::new();
    let mut contexts = vec![context];

    let change_context = ChangeContext {
        before_clock: Arc::new(InstantQueryClock::new(0, 0)),
        after_clock: Arc::new(InstantQueryClock::new(0, 0)),
        solution_signature: 0,
        before_anchor_element: None,
        after_anchor_element: None,
        is_future_reprocess: false,
        before_grouping_hash: 0,
        after_grouping_hash: 0,
    };

    let mut part_num = 0;

    for part in &query.parts {
        part_num += 1;
        result.clear();

        for ctx in &contexts {
            let mut new_contexts = evaluator
                .evaluate(ctx.clone(), part_num, part, &change_context)
                .await
                .unwrap();
            result.append(&mut new_contexts);
        }
        contexts = result.clone();
    }

    result
}

fn build_query(query: &str) -> Query {
    let function_registry = Arc::new(FunctionRegistry::new());

    use crate::evaluation::functions::{Avg, Count, Function, Max, Min, Sum};
    function_registry.register_function("sum", Function::Aggregating(Arc::new(Sum {})));
    function_registry.register_function("min", Function::Aggregating(Arc::new(Min {})));
    function_registry.register_function("max", Function::Aggregating(Arc::new(Max {})));
    function_registry.register_function("avg", Function::Aggregating(Arc::new(Avg {})));
    function_registry.register_function("count", Function::Aggregating(Arc::new(Count {})));

    let parser = Arc::new(CypherParser::new(function_registry.clone()));
    parser.parse(query).unwrap()
}
