use drasi_query_ast::ast::Query;
use drasi_query_ast::api::QueryParser;
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

async fn process_solution<'a>(
    query: &Query,
    evaluator: &'a QueryPartEvaluator,
    context: QueryPartEvaluationContext,
) -> Vec<QueryPartEvaluationContext> {
    let mut result = Vec::new();
    let mut contexts = Vec::new();
    contexts.push(context);

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
    let parser = CypherParser::new(Arc::new(FunctionRegistry::new()));
    parser.parse(query).unwrap()
}