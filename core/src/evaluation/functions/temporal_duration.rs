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

mod temporal_duration;

#[cfg(test)]
mod tests;

use std::sync::Arc;

use self::temporal_duration::{Between, DurationFunc, InDays, InMonths, InSeconds};

use super::{Function, FunctionRegistry};

pub trait RegisterCypherTemporalDurationFunctions {
    fn register_cypher_temporal_duration_functions(&self);
}
pub trait RegisterGqlTemporalDurationFunctions {
    fn register_gql_temporal_duration_functions(&self);
}

impl RegisterCypherTemporalDurationFunctions for FunctionRegistry {
    fn register_cypher_temporal_duration_functions(&self) {
        self.register_function("duration.between", Function::Scalar(Arc::new(Between {})));
        self.register_function("duration.inMonths", Function::Scalar(Arc::new(InMonths {})));
        self.register_function("duration.inDays", Function::Scalar(Arc::new(InDays {})));
        self.register_function(
            "duration.inSeconds",
            Function::Scalar(Arc::new(InSeconds {})),
        );
        self.register_function("duration", Function::Scalar(Arc::new(DurationFunc {})));
    }
}

impl RegisterGqlTemporalDurationFunctions for FunctionRegistry {
    fn register_gql_temporal_duration_functions(&self) {
        self.register_function("dateDiff", Function::Scalar(Arc::new(Between {})));
    }
}