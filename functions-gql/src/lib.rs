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

use drasi_core::evaluation::functions::*;

use drasi_core::evaluation::functions::FunctionRegistry;
#[cfg(test)]
mod tests;

mod gql_scalar;
use gql_scalar::Cast;

pub trait GQLFunctionSet {
    fn with_gql_function_set(self) -> Arc<FunctionRegistry>;
}

impl GQLFunctionSet for Arc<FunctionRegistry> {
    fn with_gql_function_set(self) -> Arc<FunctionRegistry> {
        register_default_gql_functions(&self);
        self
    }
}

pub fn register_default_gql_functions(registry: &FunctionRegistry) {
    register_text_functions(registry);
    register_numeric_functions(registry);
    register_gql_scalar_functions(registry);
    register_list_functions(registry);
    register_metadata_functions(registry);
    register_drasi_functions(registry);
    register_context_mutators(registry);
    register_aggregation_functions(registry);
    register_temporal_instant_functions(registry);
    register_temporal_duration_functions(registry);
}

fn register_text_functions(registry: &FunctionRegistry) {
    registry.register_function("upper", Function::Scalar(Arc::new(ToUpper {})));
    registry.register_function("lower", Function::Scalar(Arc::new(ToLower {})));
    registry.register_function("trim", Function::Scalar(Arc::new(Trim {})));
    registry.register_function("ltrim", Function::Scalar(Arc::new(LTrim {})));
    registry.register_function("rtrim", Function::Scalar(Arc::new(RTrim {})));
    registry.register_function("reverse", Function::Scalar(Arc::new(Reverse {})));
    registry.register_function("left", Function::Scalar(Arc::new(Left {})));
    registry.register_function("right", Function::Scalar(Arc::new(Right {})));
    registry.register_function("replace", Function::Scalar(Arc::new(Replace {})));
    registry.register_function("split", Function::Scalar(Arc::new(Split {})));
    registry.register_function("substring", Function::Scalar(Arc::new(Substring {})));
}

fn register_numeric_functions(registry: &FunctionRegistry) {
    registry.register_function("abs", Function::Scalar(Arc::new(Abs {})));
    registry.register_function("ceil", Function::Scalar(Arc::new(Ceil {})));
    registry.register_function("floor", Function::Scalar(Arc::new(Floor {})));
    registry.register_function("round", Function::Scalar(Arc::new(Round {})));
}

fn register_gql_scalar_functions(registry: &FunctionRegistry) {
    registry.register_function("char_length", Function::Scalar(Arc::new(CharLength {})));
    registry.register_function("size", Function::Scalar(Arc::new(Size {})));
    registry.register_function("coalesce", Function::Scalar(Arc::new(Coalesce {})));
    registry.register_function("last", Function::Scalar(Arc::new(CypherLast {})));
    registry.register_function("cast", Function::Scalar(Arc::new(Cast {})));
}

fn register_list_functions(registry: &FunctionRegistry) {
    registry.register_function("reduce", Function::LazyScalar(Arc::new(Reduce::new())));
}

fn register_metadata_functions(registry: &FunctionRegistry) {
    registry.register_function("element_id", Function::Scalar(Arc::new(ElementId {})));
    registry.register_function(
        "drasi.changeDateTime",
        Function::Scalar(Arc::new(ChangeDateTime {})),
    );
}

fn register_drasi_functions(registry: &FunctionRegistry) {
    registry.register_function("drasi.listMax", Function::Scalar(Arc::new(DrasiMax {})));
    registry.register_function("drasi.listMin", Function::Scalar(Arc::new(DrasiMin {})));
    registry.register_function("drasi.stdevp", Function::Scalar(Arc::new(DrasiStdevP {})));
}

fn register_context_mutators(registry: &FunctionRegistry) {
    registry.register_function(
        "retainHistory",
        Function::ContextMutator(Arc::new(RetainHistory {})),
    );
}

fn register_aggregation_functions(registry: &FunctionRegistry) {
    registry.register_function("sum", Function::Aggregating(Arc::new(Sum {})));
    registry.register_function("avg", Function::Aggregating(Arc::new(Avg {})));
    registry.register_function("count", Function::Aggregating(Arc::new(Count {})));
    registry.register_function("min", Function::Aggregating(Arc::new(Min {})));
    registry.register_function("max", Function::Aggregating(Arc::new(Max {})));
    registry.register_function(
        "drasi.linearGradient",
        Function::Aggregating(Arc::new(LinearGradient {})),
    );
    registry.register_function(
        "drasi.last",
        Function::Aggregating(Arc::new(AggregatingLast {})),
    );
}

fn register_temporal_instant_functions(registry: &FunctionRegistry) {
    registry.register_function("date", Function::Scalar(Arc::new(Date {})));
    registry.register_function("zoned_time", Function::Scalar(Arc::new(Time {})));
    registry.register_function("local_time", Function::Scalar(Arc::new(LocalTime {})));
    registry.register_function("zoned_datetime", Function::Scalar(Arc::new(DateTime {})));
    registry.register_function(
        "local_datetime",
        Function::Scalar(Arc::new(LocalDateTime {})),
    );
}

fn register_temporal_duration_functions(registry: &FunctionRegistry) {
    registry.register_function("duration_between", Function::Scalar(Arc::new(Between {})));
    registry.register_function("duration.inMonths", Function::Scalar(Arc::new(InMonths {})));
    registry.register_function("duration.inDays", Function::Scalar(Arc::new(InDays {})));
    registry.register_function(
        "duration.inSeconds",
        Function::Scalar(Arc::new(InSeconds {})),
    );
    registry.register_function("duration", Function::Scalar(Arc::new(DurationFunc {})));
}
