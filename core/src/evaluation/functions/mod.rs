use std::{
    collections::HashMap,
    fmt::Debug,
    sync::{Arc, RwLock},
};

use async_trait::async_trait;

use drasi_query_ast::ast;
use drasi_query_cypher::CypherConfiguration;

use crate::evaluation::variable_value::VariableValue;
use crate::interface::ResultIndex;

use self::{
    aggregation::{Accumulator, RegisterAggregationFunctions},
    context_mutators::RegisterContextMutatorFunctions,
    cypher_scalar::RegisterCypherScalarFunctions,
    drasi::RegisterDrasiFunctions,
    list::RegisterListFunctions,
    metadata::RegisterMetadataFunctions,
    numeric::RegisterNumericFunctions,
    temporal_duration::RegisterTemporalDurationFunctions,
    temporal_instant::RegisterTemporalInstantFunctions,
    text::RegisterTextFunctions,
};

use super::{EvaluationError, ExpressionEvaluationContext};

pub mod aggregation;
pub mod context_mutators;
pub mod cypher_scalar;
pub mod drasi;
pub mod future;
pub mod list;
pub mod metadata;
pub mod numeric;
pub mod past;
pub mod temporal_duration;
pub mod temporal_instant;
pub mod text;

pub enum Function {
    Scalar(Arc<dyn ScalarFunction>),
    Aggregating(Arc<dyn AggregatingFunction>),
    ContextMutator(Arc<dyn ContextMutatorFunction>),
}

#[async_trait]
pub trait ScalarFunction: Send + Sync {
    async fn call(
        &self,
        context: &ExpressionEvaluationContext,
        expression: &ast::FunctionExpression,
        args: Vec<VariableValue>,
    ) -> Result<VariableValue, EvaluationError>;
}

#[async_trait]
pub trait AggregatingFunction: Debug + Send + Sync {
    fn initialize_accumulator(
        &self,
        context: &ExpressionEvaluationContext,
        expression: &ast::FunctionExpression,
        grouping_keys: &Vec<VariableValue>,
        index: Arc<dyn ResultIndex>,
    ) -> Accumulator; //todo: switch `dyn ResultIndex` to `dyn LazySortedSetStore` after trait upcasting is stable
    async fn apply(
        &self,
        context: &ExpressionEvaluationContext,
        args: Vec<VariableValue>,
        accumulator: &mut Accumulator,
    ) -> Result<VariableValue, EvaluationError>;
    async fn revert(
        &self,
        context: &ExpressionEvaluationContext,
        args: Vec<VariableValue>,
        accumulator: &mut Accumulator,
    ) -> Result<VariableValue, EvaluationError>;
    async fn snapshot(
        &self,
        context: &ExpressionEvaluationContext,
        args: Vec<VariableValue>,
        accumulator: &Accumulator,
    ) -> Result<VariableValue, EvaluationError>;
    fn accumulator_is_lazy(&self) -> bool;
}

#[async_trait]
pub trait ContextMutatorFunction: Send + Sync {
    async fn call<'a>(
        &self,
        context: &ExpressionEvaluationContext<'a>,
        expression: &ast::FunctionExpression,
    ) -> Result<ExpressionEvaluationContext<'a>, EvaluationError>;
}

pub struct FunctionRegistry {
    functions: Arc<RwLock<HashMap<String, Arc<Function>>>>,
}

impl Default for FunctionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl FunctionRegistry {
    pub fn new() -> FunctionRegistry {
        let result = FunctionRegistry {
            functions: Arc::new(RwLock::new(HashMap::new())),
        };

        result.register_text_functions();
        result.register_metadata_functions();
        result.register_numeric_functions();
        result.register_aggregation_functions();
        result.register_temporal_instant_functions();
        result.register_temporal_duration_functions();
        result.register_list_functions();
        result.register_drasi_functions();
        result.register_scalar_functions();
        result.register_context_mutators();

        result
    }

    pub fn register_function(&self, name: &str, function: Function) {
        let mut lock = self.functions.write().unwrap();
        lock.insert(name.to_string(), Arc::new(function));
    }

    pub fn get_function(&self, name: &str) -> Option<Arc<Function>> {
        let lock = self.functions.read().unwrap();
        lock.get(name).cloned()
    }
}

impl CypherConfiguration for FunctionRegistry {
    fn get_aggregating_function_names(&self) -> std::collections::HashSet<String> {
        let lock = self.functions.read().unwrap();
        lock.iter()
            .filter_map(|(name, function)| match function.as_ref() {
                Function::Aggregating(_) => Some(name.clone()),
                _ => None,
            })
            .collect()
    }
}