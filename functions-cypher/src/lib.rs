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

pub trait CypherFunctionSet {
    fn with_cypher_function_set(self) -> Arc<FunctionRegistry>;
}

impl CypherFunctionSet for Arc<FunctionRegistry> {
    fn with_cypher_function_set(self) -> Arc<FunctionRegistry> {
        register_default_cypher_functions(&self);
        self
    }
}

pub fn register_default_cypher_functions(registry: &FunctionRegistry) {
    register_text_functions(registry);
    register_numeric_functions(registry);
    register_trigonometric_functions(registry);
    register_cypher_scalar_functions(registry);
    register_list_functions(registry);
    register_metadata_functions(registry);
    register_drasi_functions(registry);
    register_context_mutators(registry);
    register_aggregation_functions(registry);
    register_temporal_instant_functions(registry);
    register_temporal_duration_functions(registry);
}

fn register_text_functions(registry: &FunctionRegistry) {
    registry.register_function("toUpper", Function::Scalar(Arc::new(ToUpper {})));
    registry.register_function("toLower", Function::Scalar(Arc::new(ToLower {})));
    registry.register_function("trim", Function::Scalar(Arc::new(Trim {})));
    registry.register_function("ltrim", Function::Scalar(Arc::new(LTrim {})));
    registry.register_function("rtrim", Function::Scalar(Arc::new(RTrim {})));
    registry.register_function("reverse", Function::Scalar(Arc::new(Reverse {})));
    registry.register_function("left", Function::Scalar(Arc::new(Left {})));
    registry.register_function("right", Function::Scalar(Arc::new(Right {})));
    registry.register_function("replace", Function::Scalar(Arc::new(Replace {})));
    registry.register_function("split", Function::Scalar(Arc::new(Split {})));
    registry.register_function("substring", Function::Scalar(Arc::new(Substring {})));
    registry.register_function("toString", Function::Scalar(Arc::new(ToString {})));
    registry.register_function(
        "toStringOrNull",
        Function::Scalar(Arc::new(ToStringOrNull {})),
    );
    registry.register_function("randomUUID", Function::Scalar(Arc::new(RandomUUID {})));
}

fn register_numeric_functions(registry: &FunctionRegistry) {
    registry.register_function("abs", Function::Scalar(Arc::new(Abs {})));
    registry.register_function("ceil", Function::Scalar(Arc::new(Ceil {})));
    registry.register_function("floor", Function::Scalar(Arc::new(Floor {})));
    registry.register_function("rand", Function::Scalar(Arc::new(Rand {})));
    registry.register_function("round", Function::Scalar(Arc::new(Round {})));
    registry.register_function("sign", Function::Scalar(Arc::new(Sign {})));
}

fn register_trigonometric_functions(registry: &FunctionRegistry) {
    registry.register_function("cos", Function::Scalar(Arc::new(Cos {})));
    registry.register_function("degrees", Function::Scalar(Arc::new(Degrees {})));
    registry.register_function("pi", Function::Scalar(Arc::new(Pi {})));
    registry.register_function("radians", Function::Scalar(Arc::new(Radians {})));
    registry.register_function("sin", Function::Scalar(Arc::new(Sin {})));
    registry.register_function("tan", Function::Scalar(Arc::new(Tan {})));
}

fn register_cypher_scalar_functions(registry: &FunctionRegistry) {
    registry.register_function("char_length", Function::Scalar(Arc::new(CharLength {})));
    registry.register_function(
        "character_length",
        Function::Scalar(Arc::new(CharLength {})),
    );
    registry.register_function("size", Function::Scalar(Arc::new(Size {})));
    registry.register_function("toInteger", Function::Scalar(Arc::new(ToInteger {})));
    registry.register_function(
        "toIntegerOrNull",
        Function::Scalar(Arc::new(ToIntegerOrNull {})),
    );
    registry.register_function("toFloat", Function::Scalar(Arc::new(ToFloat {})));
    registry.register_function(
        "toFloatOrNull",
        Function::Scalar(Arc::new(ToFloatOrNull {})),
    );
    registry.register_function("toBoolean", Function::Scalar(Arc::new(ToBoolean {})));
    registry.register_function(
        "toBooleanOrNull",
        Function::Scalar(Arc::new(ToBooleanOrNull {})),
    );
    registry.register_function("coalesce", Function::Scalar(Arc::new(Coalesce {})));
    registry.register_function("nullIf", Function::Scalar(Arc::new(NullIf {})));
    registry.register_function("isEmpty", Function::Scalar(Arc::new(IsEmpty {})));
    registry.register_function("head", Function::Scalar(Arc::new(Head {})));
    registry.register_function("last", Function::Scalar(Arc::new(CypherLast {})));
    registry.register_function("timestamp", Function::Scalar(Arc::new(Timestamp {})));
}

fn register_list_functions(registry: &FunctionRegistry) {
    registry.register_function("reduce", Function::LazyScalar(Arc::new(Reduce::new())));
    registry.register_function("tail", Function::Scalar(Arc::new(Tail {})));
    registry.register_function("range", Function::Scalar(Arc::new(Range {})));
    registry.register_function("coll.distinct", Function::Scalar(Arc::new(Distinct {})));
    registry.register_function("coll.indexOf", Function::Scalar(Arc::new(IndexOf {})));
    registry.register_function("coll.insert", Function::Scalar(Arc::new(Insert {})));
}

fn register_metadata_functions(registry: &FunctionRegistry) {
    registry.register_function("elementId", Function::Scalar(Arc::new(ElementId {})));
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
    registry.register_function("collect", Function::Aggregating(Arc::new(Collect {})));
    registry.register_function(
        "collect_list",
        Function::Aggregating(Arc::new(CollectList {})),
    );
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
    registry.register_function("date.truncate", Function::Scalar(Arc::new(Truncate {})));
    registry.register_function(
        "date.statement",
        Function::Scalar(Arc::new(ClockFunction::new(
            Clock::Statement,
            ClockResult::Date,
        ))),
    );
    registry.register_function(
        "date.transaction",
        Function::Scalar(Arc::new(ClockFunction::new(
            Clock::Transaction,
            ClockResult::Date,
        ))),
    );
    registry.register_function(
        "date.realtime",
        Function::Scalar(Arc::new(ClockFunction::new(
            Clock::RealTime,
            ClockResult::Date,
        ))),
    );
    registry.register_function("localtime", Function::Scalar(Arc::new(LocalTime {})));
    registry.register_function(
        "localtime.realtime",
        Function::Scalar(Arc::new(ClockFunction::new(
            Clock::RealTime,
            ClockResult::LocalTime,
        ))),
    );
    registry.register_function(
        "localtime.transaction",
        Function::Scalar(Arc::new(ClockFunction::new(
            Clock::Transaction,
            ClockResult::LocalTime,
        ))),
    );
    registry.register_function(
        "localtime.statement",
        Function::Scalar(Arc::new(ClockFunction::new(
            Clock::Statement,
            ClockResult::LocalTime,
        ))),
    );
    registry.register_function(
        "localdatetime",
        Function::Scalar(Arc::new(LocalDateTime {})),
    );
    registry.register_function(
        "localdatetime.statement",
        Function::Scalar(Arc::new(ClockFunction::new(
            Clock::Statement,
            ClockResult::LocalDateTime,
        ))),
    );
    registry.register_function(
        "localdatetime.transaction",
        Function::Scalar(Arc::new(ClockFunction::new(
            Clock::Transaction,
            ClockResult::LocalDateTime,
        ))),
    );
    registry.register_function(
        "localdatetime.realtime",
        Function::Scalar(Arc::new(ClockFunction::new(
            Clock::RealTime,
            ClockResult::LocalDateTime,
        ))),
    );
    registry.register_function(
        "time.realtime",
        Function::Scalar(Arc::new(ClockFunction::new(
            Clock::RealTime,
            ClockResult::ZonedTime,
        ))),
    );
    registry.register_function(
        "time.statement",
        Function::Scalar(Arc::new(ClockFunction::new(
            Clock::Statement,
            ClockResult::ZonedTime,
        ))),
    );
    registry.register_function(
        "time.transaction",
        Function::Scalar(Arc::new(ClockFunction::new(
            Clock::Transaction,
            ClockResult::ZonedTime,
        ))),
    );
    registry.register_function("time.truncate", Function::Scalar(Arc::new(Truncate {})));
    registry.register_function("time", Function::Scalar(Arc::new(Time {})));
    registry.register_function("datetime", Function::Scalar(Arc::new(DateTime {})));
    registry.register_function(
        "datetime.transaction",
        Function::Scalar(Arc::new(ClockFunction::new(
            Clock::Transaction,
            ClockResult::ZonedDateTime,
        ))),
    );
    registry.register_function(
        "datetime.realtime",
        Function::Scalar(Arc::new(ClockFunction::new(
            Clock::RealTime,
            ClockResult::ZonedDateTime,
        ))),
    );
    registry.register_function(
        "datetime.statement",
        Function::Scalar(Arc::new(ClockFunction::new(
            Clock::Statement,
            ClockResult::ZonedDateTime,
        ))),
    );
    registry.register_function("datetime.truncate", Function::Scalar(Arc::new(Truncate {})));
    registry.register_function(
        "localtime.truncate",
        Function::Scalar(Arc::new(Truncate {})),
    );
    registry.register_function(
        "localdatetime.truncate",
        Function::Scalar(Arc::new(Truncate {})),
    );
}

fn register_temporal_duration_functions(registry: &FunctionRegistry) {
    registry.register_function("duration.between", Function::Scalar(Arc::new(Between {})));
    registry.register_function("duration.inMonths", Function::Scalar(Arc::new(InMonths {})));
    registry.register_function("duration.inDays", Function::Scalar(Arc::new(InDays {})));
    registry.register_function(
        "duration.inSeconds",
        Function::Scalar(Arc::new(InSeconds {})),
    );
    registry.register_function("duration", Function::Scalar(Arc::new(DurationFunc {})));
}
