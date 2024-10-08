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

mod char_length;
mod coalesce;
mod head;
mod last;
mod size;
mod timestamp;
mod to_boolean;
mod to_float;
mod to_integer;

#[cfg(test)]
mod tests;

use std::sync::Arc;

use char_length::CharLength;
use coalesce::Coalesce;

use super::{Function, FunctionRegistry};
use head::Head;
use last::Last;
use size::Size;
use timestamp::Timestamp;
use to_boolean::{ToBoolean, ToBooleanOrNull};
use to_float::{ToFloat, ToFloatOrNull};
use to_integer::{ToInteger, ToIntegerOrNull};

pub trait RegisterCypherScalarFunctions {
    fn register_scalar_functions(&self);
}

impl RegisterCypherScalarFunctions for FunctionRegistry {
    fn register_scalar_functions(&self) {
        self.register_function("char_length", Function::Scalar(Arc::new(CharLength {})));
        self.register_function(
            "character_length",
            Function::Scalar(Arc::new(CharLength {})),
        );
        self.register_function("size", Function::Scalar(Arc::new(Size {})));
        self.register_function("toInteger", Function::Scalar(Arc::new(ToInteger {})));
        self.register_function(
            "toIntegerOrNull",
            Function::Scalar(Arc::new(ToIntegerOrNull {})),
        );
        self.register_function("toFloat", Function::Scalar(Arc::new(ToFloat {})));
        self.register_function(
            "toFloatOrNull",
            Function::Scalar(Arc::new(ToFloatOrNull {})),
        );
        self.register_function("toBoolean", Function::Scalar(Arc::new(ToBoolean {})));
        self.register_function(
            "toBooleanOrNull",
            Function::Scalar(Arc::new(ToBooleanOrNull {})),
        );
        self.register_function("coalesce", Function::Scalar(Arc::new(Coalesce {})));
        self.register_function("head", Function::Scalar(Arc::new(Head {})));
        self.register_function("last", Function::Scalar(Arc::new(Last {})));
        self.register_function("timestamp", Function::Scalar(Arc::new(Timestamp {})));
    }
}
