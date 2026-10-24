// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use async_trait::async_trait;
use drasi_query_ast::{api::QueryParser, ast};
use drasi_query_cypher::CypherParser;

use crate::{
    evaluation::{
        context::{ChangeContext, QueryPartEvaluationContext, QueryVariables},
        functions::{Function, FunctionRegistry, ScalarFunction},
        variable_value::VariableValue,
        ExpressionEvaluationContext, ExpressionEvaluator, FunctionError, InstantQueryClock,
        QueryPartEvaluator,
    },
    in_memory_index::in_memory_result_index::InMemoryResultIndex,
};

struct ClockProbe(Arc<AtomicUsize>);

#[async_trait]
impl ScalarFunction for ClockProbe {
    async fn call(
        &self,
        context: &ExpressionEvaluationContext,
        _: &ast::FunctionExpression,
        _: Vec<VariableValue>,
    ) -> Result<VariableValue, FunctionError> {
        self.0.fetch_add(1, Ordering::Relaxed);
        Ok(VariableValue::Integer(context.get_realtime().into()))
    }
}

#[tokio::test]
async fn repeated_or_older_future_hints_do_not_repeat_already_applied_projections() {
    let calls = Arc::new(AtomicUsize::new(0));
    let functions = Arc::new(FunctionRegistry::new());
    functions.register_function(
        "probe",
        Function::Scalar(Arc::new(ClockProbe(calls.clone()))),
    );
    let query = CypherParser::new(functions.clone())
        .parse("MATCH (n) RETURN probe() AS observed")
        .unwrap();
    let index = Arc::new(InMemoryResultIndex::new());
    let evaluator = QueryPartEvaluator::new(
        Arc::new(ExpressionEvaluator::new(functions, index.clone())),
        index,
    );
    let variables = QueryVariables::from([("n".into(), VariableValue::Integer(1.into()))]);
    let context = |due, future| ChangeContext {
        before_clock: Arc::new(InstantQueryClock::new(1000, 1000)),
        after_clock: Arc::new(InstantQueryClock::new(1000, due)),
        solution_signature: 42,
        before_anchor_element: None,
        after_anchor_element: None,
        is_future_reprocess: future,
        before_grouping_hash: 42,
        after_grouping_hash: 42,
    };
    evaluator
        .evaluate(
            QueryPartEvaluationContext::Adding {
                after: variables.clone(),
                row_signature: 42,
            },
            1,
            &query.parts[0],
            &context(1000, false),
        )
        .await
        .unwrap();
    let update = || QueryPartEvaluationContext::Updating {
        before: variables.clone(),
        after: variables.clone(),
        row_signature: 42,
    };
    let first = evaluator
        .evaluate(update(), 1, &query.parts[0], &context(2000, true))
        .await
        .unwrap();
    assert!(
        matches!(&first[..], [QueryPartEvaluationContext::Updating { before, after, .. }]
        if before["observed"] == VariableValue::Integer(1000.into())
        && after["observed"] == VariableValue::Integer(2000.into()))
    );
    for due in [2000, 1500] {
        calls.store(0, Ordering::Relaxed);
        assert_eq!(
            evaluator
                .evaluate(update(), 1, &query.parts[0], &context(due, true))
                .await
                .unwrap(),
            vec![QueryPartEvaluationContext::Noop],
        );
        assert_eq!(
            calls.load(Ordering::Relaxed),
            0,
            "already-applied hints must not repeat expression work"
        );
    }
    let later = evaluator
        .evaluate(update(), 1, &query.parts[0], &context(3000, true))
        .await
        .unwrap();
    assert!(
        matches!(&later[..], [QueryPartEvaluationContext::Updating { before, after, .. }]
        if before["observed"] == VariableValue::Integer(2000.into())
        && after["observed"] == VariableValue::Integer(3000.into()))
    );
}
