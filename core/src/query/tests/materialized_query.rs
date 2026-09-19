// Copyright 2026 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{collections::HashMap, sync::Arc};

use drasi_query_cypher::CypherParser;

use crate::{
    evaluation::{
        context::{QueryPartEvaluationContext, QueryVariables},
        functions::{Avg, Count, Floor, Function, FunctionRegistry, Sum, ToFloat},
    },
    in_memory_index::{
        in_memory_element_index::InMemoryElementIndex, in_memory_future_queue::InMemoryFutureQueue,
        in_memory_result_index::InMemoryResultIndex,
    },
    models::{QueryJoin, SourceChange},
    query::{ContinuousQuery, QueryBuilder},
};

pub(super) struct MaterializedQuery {
    query: ContinuousQuery,
    pub rows: HashMap<u64, QueryVariables>,
}

impl MaterializedQuery {
    pub async fn new(query_text: &str) -> Self {
        Self::with_joins(query_text, vec![]).await
    }

    pub async fn with_joins(query_text: &str, joins: Vec<QueryJoin>) -> Self {
        let functions = Arc::new(FunctionRegistry::new());
        functions.register_function("count", Function::Aggregating(Arc::new(Count {})));
        functions.register_function("sum", Function::Aggregating(Arc::new(Sum {})));
        functions.register_function("avg", Function::Aggregating(Arc::new(Avg {})));
        functions.register_function("floor", Function::Scalar(Arc::new(Floor {})));
        functions.register_function("toFloat", Function::Scalar(Arc::new(ToFloat {})));
        let parser = Arc::new(CypherParser::new(functions.clone()));
        let element_index = Arc::new(InMemoryElementIndex::new());
        let mut builder = QueryBuilder::new(query_text, parser)
            .with_function_registry(functions)
            .with_element_index(element_index.clone())
            .with_archive_index(element_index)
            .with_result_index(Arc::new(InMemoryResultIndex::new()))
            .with_future_queue(Arc::new(InMemoryFutureQueue::new()));
        for join in joins {
            builder = builder.with_join(join);
        }
        Self {
            query: builder.build().await,
            rows: HashMap::new(),
        }
    }

    pub async fn process(&mut self, change: SourceChange) -> Vec<QueryPartEvaluationContext> {
        let changes = self.query.process_source_change(change).await.unwrap();
        for change in &changes {
            match change {
                QueryPartEvaluationContext::Adding {
                    after,
                    row_signature,
                }
                | QueryPartEvaluationContext::Updating {
                    after,
                    row_signature,
                    ..
                }
                | QueryPartEvaluationContext::Aggregation {
                    after,
                    row_signature,
                    ..
                } => {
                    self.rows.insert(*row_signature, after.clone());
                }
                QueryPartEvaluationContext::Removing { row_signature, .. } => {
                    self.rows.remove(row_signature);
                }
                QueryPartEvaluationContext::Noop => {}
            }
        }
        changes
    }
}
