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

use std::{collections::HashMap, sync::Arc};

use drasi_query_ast::api::QueryParser;
use drasi_query_ast::ast::Query;
use drasi_query_cypher::CypherParser;
use drasi_query_gql::GQLParser;

use crate::{
    evaluation::{
        functions::{
            future::RegisterFutureFunctions, past::RegisterPastFunctions, FunctionRegistry, FunctionType,
        },
        ExpressionEvaluator, QueryPartEvaluator,
    },
    in_memory_index::{
        in_memory_element_index::InMemoryElementIndex, in_memory_future_queue::InMemoryFutureQueue,
        in_memory_result_index::InMemoryResultIndex,
    },
    index_cache::shadowed_future_queue::ShadowedFutureQueue,
    interface::{
        ElementArchiveIndex, ElementIndex, FutureQueue, MiddlewareSetupError, QueryBuilderError,
        ResultIndex,
    },
    middleware::{
        MiddlewareContainer, MiddlewareTypeRegistry, SourceMiddlewarePipeline,
        SourceMiddlewarePipelineCollection,
    },
    models::{QueryJoin, SourceMiddlewareConfig},
    path_solver::{match_path::MatchPath, MatchPathSolver},
};

use super::ContinuousQuery;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum QueryParserType {
    Cypher,
    GQL,
}

impl Default for QueryParserType {
    fn default() -> Self {
        QueryParserType::Cypher
    }
}

pub struct QueryBuilder {
    function_registry: Option<Arc<FunctionRegistry>>,
    expr_evaluator: Option<Arc<ExpressionEvaluator>>,
    element_index: Option<Arc<dyn ElementIndex>>,
    archive_index: Option<Arc<dyn ElementArchiveIndex>>,
    result_index: Option<Arc<dyn ResultIndex>>,
    future_queue: Option<Arc<dyn FutureQueue>>,
    part_evaluator: Option<Arc<QueryPartEvaluator>>,
    joins: Vec<Arc<QueryJoin>>,
    middleware_registry: Option<Arc<MiddlewareTypeRegistry>>,
    source_middleware: Vec<Arc<SourceMiddlewareConfig>>,
    source_pipelines: HashMap<Arc<str>, Vec<Arc<str>>>,

    query_source: String,
    query_parser: Option<Arc<dyn QueryParser>>,
    parser_type: QueryParserType,
}

impl QueryBuilder {
    pub fn new(query: impl Into<String>) -> Self {
        QueryBuilder {
            function_registry: None,
            expr_evaluator: None,
            element_index: None,
            archive_index: None,
            result_index: None,
            future_queue: None,
            part_evaluator: None,
            joins: Vec::new(),
            middleware_registry: None,
            source_middleware: Vec::new(),
            source_pipelines: HashMap::new(),
            query_source: query.into(),
            query_parser: None,
            parser_type: QueryParserType::default(),
        }
    }

    pub fn with_query_parser(mut self, parser_type: QueryParserType) -> Self {
        self.parser_type = parser_type;
        self
    }

    pub fn with_middleware_registry(
        mut self,
        middleware_registry: Arc<MiddlewareTypeRegistry>,
    ) -> Self {
        self.middleware_registry = Some(middleware_registry);
        self
    }

    pub fn with_source_middleware(
        mut self,
        source_middleware: Arc<SourceMiddlewareConfig>,
    ) -> Self {
        self.source_middleware.push(source_middleware);
        self
    }

    pub fn with_source_pipeline(mut self, source: impl Into<String>, pipeline: &[String]) -> Self {
        let pipeline = pipeline.iter().map(|s| Arc::from(s.as_str())).collect();
        self.source_pipelines
            .insert(Arc::from(source.into()), pipeline);
        self
    }

    pub fn with_join(mut self, join: QueryJoin) -> Self {
        self.joins.push(Arc::new(join));
        self
    }

    pub fn with_joins(mut self, joins: Vec<QueryJoin>) -> Self {
        for join in joins {
            self.joins.push(Arc::new(join));
        }
        self
    }

    pub fn with_function_registry(mut self, function_registry: Arc<FunctionRegistry>) -> Self {
        self.function_registry = Some(function_registry);
        self
    }

    pub fn with_element_index(mut self, element_index: Arc<dyn ElementIndex>) -> Self {
        self.element_index = Some(element_index);
        self
    }

    pub fn with_archive_index(mut self, archive_index: Arc<dyn ElementArchiveIndex>) -> Self {
        self.archive_index = Some(archive_index);
        self
    }

    pub fn with_result_index(mut self, accumulator_result_index: Arc<dyn ResultIndex>) -> Self {
        self.result_index = Some(accumulator_result_index);
        self
    }

    pub fn with_future_queue(mut self, future_queue: Arc<dyn FutureQueue>) -> Self {
        self.future_queue = Some(future_queue);
        self
    }


    pub fn get_joins(&self) -> &Vec<Arc<QueryJoin>> {
        &self.joins
    }

    pub async fn build(self) -> ContinuousQuery {
        self.try_build().await.unwrap()
    }

    pub async fn try_build(mut self) -> Result<ContinuousQuery, QueryBuilderError> {
        let function_registry = match self.function_registry.take() {
            Some(registry) => registry,
            None => match self.parser_type {
                QueryParserType::Cypher => Arc::new(FunctionRegistry::new()),
                QueryParserType::GQL => Arc::new(FunctionRegistry::with_type(FunctionType::GQL)),
            },
        };

        let query_parser = match self.query_parser.take() {
            Some(parser) => parser,
            None => {
                match self.parser_type {
                    QueryParserType::Cypher => {
                        let cypher_parser = CypherParser::new(function_registry.clone());
                        Arc::new(cypher_parser) as Arc<dyn QueryParser>
                    }
                    QueryParserType::GQL => {
                        let gql_parser = GQLParser::new(function_registry.clone());
                        Arc::new(gql_parser) as Arc<dyn QueryParser>
                    }
                }
            }
        };

        let query = query_parser.parse(self.query_source.as_str())?;
        let match_path = Arc::new(MatchPath::from_query(&query.parts[0])?);

        let element_index = match self.element_index.take() {
            Some(index) => index,
            None => Arc::new(InMemoryElementIndex::new()),
        };

        if let Some(archive_index) = self.archive_index.take() {
            function_registry.register_past_functions(archive_index);
        }

        let result_index = match self.result_index.take() {
            Some(index) => index,
            None => Arc::new(InMemoryResultIndex::new()),
        };

        let future_queue = match self.future_queue.take() {
            Some(queue) => queue,
            None => Arc::new(InMemoryFutureQueue::new()),
        };

        let future_queue = Arc::new(ShadowedFutureQueue::new(future_queue));

        let expr_evaluator = match self.expr_evaluator.take() {
            Some(evaluator) => evaluator,
            None => Arc::new(ExpressionEvaluator::new(
                function_registry.clone(),
                result_index.clone(),
            )),
        };

        let part_evaluator = match self.part_evaluator.take() {
            Some(evaluator) => evaluator,
            None => Arc::new(QueryPartEvaluator::new(
                expr_evaluator.clone(),
                result_index.clone(),
            )),
        };

        let path_solver = Arc::new(MatchPathSolver::new(element_index.clone()));

        function_registry.register_future_functions(
            future_queue.clone(),
            result_index.clone(),
            Arc::downgrade(&expr_evaluator.clone()),
        );

        let source_pipelines: SourceMiddlewarePipelineCollection = {
            if self.source_middleware.is_empty() {
                Ok(SourceMiddlewarePipelineCollection::new())
            } else {
                match self.middleware_registry.as_ref() {
                    Some(registry) => {
                        let container =
                            MiddlewareContainer::new(registry, self.source_middleware.clone())?;
                        let mut pipelines = SourceMiddlewarePipelineCollection::new();
                        for (source_id, pipeline_keys) in self.source_pipelines.iter() {
                            let pipeline =
                                SourceMiddlewarePipeline::new(&container, pipeline_keys.clone())?;
                            pipelines.insert(source_id.clone(), pipeline);
                        }
                        Ok(pipelines)
                    }
                    None => Err(MiddlewareSetupError::NoRegistry),
                }
            }
        }?;

        element_index.set_joins(&match_path, &self.joins).await;

        Ok(ContinuousQuery::new(
            Arc::new(query),
            match_path,
            expr_evaluator,
            element_index,
            path_solver,
            part_evaluator,
            future_queue,
            source_pipelines,
        ))
    }
}
