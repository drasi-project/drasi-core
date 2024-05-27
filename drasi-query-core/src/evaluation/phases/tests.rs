use drasi_query_ast::ast::Query;

use crate::evaluation::InstantQueryClock;

use super::*;

macro_rules! variablemap {
  ($( $key: expr => $val: expr ),*) => {{
       let mut map = ::std::collections::BTreeMap::new();
       $( map.insert($key.to_string().into_boxed_str(), VariableValue::from($val)); )*
       map
  }}
}
// macro_rules! variablemap {
//   ($( $key: expr => $val: expr ),*) => {{
//        let mut map = ::std::collections::BTreeMap::new();
//        $( map.insert($key.to_string().into_boxed_str(), $val); )*
//        map
//   }}
// }

mod multi_phase;
mod single_phase_aggregating;
mod single_phase_non_aggregating;

async fn process_solution<'a>(
    query: &Query,
    evaluator: &'a QueryPhaseEvaluator,
    context: PhaseEvaluationContext,
) -> Vec<PhaseEvaluationContext> {
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

    let mut phase_num = 0;

    for phase in &query.phases {
        phase_num += 1;
        result.clear();

        for ctx in &contexts {
            let mut new_contexts = evaluator
                .evaluate(ctx.clone(), phase_num, phase, &change_context)
                .await
                .unwrap();
            result.append(&mut new_contexts);
        }
        contexts = result.clone();
    }

    result
}
