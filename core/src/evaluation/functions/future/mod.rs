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

use std::sync::{Arc, Weak};

use crate::{
    evaluation::ExpressionEvaluator,
    interface::{FutureQueue, ResultIndex},
};

use super::{Function, FunctionRegistry};

mod awaiting;
mod future_element;
mod previous_distinct_value;
mod previous_value;
mod sliding_window;
mod true_for;
mod true_later;
mod true_now_or_later;
mod true_until;

#[cfg(test)]
mod retained_tests;
#[cfg(test)]
mod tests;

pub trait RegisterFutureFunctions {
    fn register_future_functions(
        &self,
        future_queue: Arc<dyn FutureQueue>,
        result_index: Arc<dyn ResultIndex>,
        expression_evaluator: Weak<ExpressionEvaluator>,
    );
}

impl RegisterFutureFunctions for FunctionRegistry {
    fn register_future_functions(
        &self,
        _future_queue: Arc<dyn FutureQueue>,
        _result_index: Arc<dyn ResultIndex>,
        _expression_evaluator: Weak<ExpressionEvaluator>,
    ) {
        self.register_function(
            "drasi.awaiting",
            Function::Scalar(Arc::new(awaiting::Awaiting::new())),
        );
        self.register_function(
            "drasi.future",
            Function::Temporal(Arc::new(future_element::FutureElement)),
        );
        self.register_function(
            "drasi.trueUntil",
            Function::Temporal(Arc::new(true_until::TrueUntil)),
        );
        self.register_function(
            "drasi.trueFor",
            Function::Temporal(Arc::new(true_for::TrueFor)),
        );
        self.register_function(
            "drasi.trueLater",
            Function::Temporal(Arc::new(true_later::TrueLater)),
        );
        self.register_function(
            "drasi.trueNowOrLater",
            Function::Temporal(Arc::new(true_now_or_later::TrueNowOrLater)),
        );
        self.register_function(
            "drasi.previousValue",
            Function::Temporal(Arc::new(previous_value::PreviousValue)),
        );
        self.register_function(
            "drasi.previousDistinctValue",
            Function::Temporal(Arc::new(previous_distinct_value::PreviousDistinctValue)),
        );
        self.register_function(
            "drasi.slidingWindow",
            Function::LazyTemporal(Arc::new(sliding_window::SlidingWindow)),
        );
    }
}
