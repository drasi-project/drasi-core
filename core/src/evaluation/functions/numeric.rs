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

mod abs;
mod ceil;
mod floor;
mod numeric_round;
mod random;
mod sign;

#[cfg(test)]
mod tests;

use std::sync::Arc;

pub use abs::Abs;
pub use ceil::Ceil;
pub use floor::Floor;
pub use numeric_round::Round;
pub use random::Rand;
pub use sign::Sign;

use super::{Function, FunctionRegistry};

pub trait RegisterCypherNumericFunctions {
    fn register_cypher_numeric_functions(&self);
}

pub trait RegisterGqlNumericFunctions {
    fn register_gql_numeric_functions(&self);
}

impl RegisterCypherNumericFunctions for FunctionRegistry {
    fn register_cypher_numeric_functions(&self) {
        self.register_function("abs", Function::Scalar(Arc::new(Abs {})));
        self.register_function("ceil", Function::Scalar(Arc::new(Ceil {})));
        self.register_function("floor", Function::Scalar(Arc::new(Floor {})));
        self.register_function("rand", Function::Scalar(Arc::new(Rand {})));
        self.register_function("round", Function::Scalar(Arc::new(Round {})));
        self.register_function("sign", Function::Scalar(Arc::new(Sign {})));
    }
}

impl RegisterGqlNumericFunctions for FunctionRegistry {
    fn register_gql_numeric_functions(&self) {
        self.register_function("abs", Function::Scalar(Arc::new(Abs {})));
        self.register_function("ceil", Function::Scalar(Arc::new(Ceil {})));
        self.register_function("floor", Function::Scalar(Arc::new(Floor {})));
        self.register_function("round", Function::Scalar(Arc::new(Round {})));
    }
}