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

use self::{future_element::FutureElement, true_for::TrueFor, true_until::TrueUntil};

use super::{Function, FunctionRegistry};

mod awaiting;
mod future_element;
mod true_for;
mod true_later;
mod true_now_or_later;
mod true_until;
mod before;

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
        future_queue: Arc<dyn FutureQueue>,
        result_index: Arc<dyn ResultIndex>,
        expression_evaluator: Weak<ExpressionEvaluator>,
    ) {
        self.register_function(
            "drasi.awaiting",
            Function::Scalar(Arc::new(awaiting::Awaiting::new())),
        );

        self.register_function(
            "drasi.future",
            Function::Scalar(Arc::new(FutureElement::new(future_queue.clone()))),
        );

        self.register_function(
            "drasi.trueUntil",
            Function::Scalar(Arc::new(TrueUntil::new(future_queue.clone()))),
        );

        self.register_function(
            "drasi.trueFor",
            Function::Scalar(Arc::new(TrueFor::new(
                future_queue.clone(),
                result_index.clone(),
                expression_evaluator.clone(),
            ))),
        );

        self.register_function(
            "drasi.trueLater",
            Function::Scalar(Arc::new(true_later::TrueLater::new(future_queue.clone()))),
        );

        self.register_function(
            "drasi.trueNowOrLater",
            Function::Scalar(Arc::new(true_now_or_later::TrueNowOrLater::new(
                future_queue.clone(),
            ))),
        );

        self.register_function(
            "drasi.before",
            Function::Scalar(Arc::new(before::Before::new(
                result_index.clone(),
                expression_evaluator.clone(),
            ))),
        );
    }
}
