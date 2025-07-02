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

mod range;
mod reduce;
mod tail;

use super::{Function, FunctionRegistry};
use std::sync::Arc;

pub use reduce::Reduce;
pub use tail::Tail;

pub use range::Range;

pub trait RegisterCypherListFunctions {
    fn register_cypher_list_functions(&self);
}

pub trait RegisterGqlListFunctions {
    fn register_gql_list_functions(&self);
}

impl RegisterCypherListFunctions for FunctionRegistry {
    fn register_cypher_list_functions(&self) {
        self.register_function("reduce", Function::LazyScalar(Arc::new(Reduce::new())));
        self.register_function("tail", Function::Scalar(Arc::new(Tail {})));
        self.register_function("range", Function::Scalar(Arc::new(Range {})));
    }
}

impl RegisterGqlListFunctions for FunctionRegistry {
    fn register_gql_list_functions(&self) {
        self.register_function("reduce", Function::LazyScalar(Arc::new(Reduce::new())));
    }
}