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

mod avg;
mod count;
mod last;
pub mod lazy_sorted_set;
mod linear_gradient;
mod max;
mod min;
mod sum;

pub use avg::Avg;
pub use count::Count;
pub use last::AggregatingLast;
pub use linear_gradient::LinearGradient;
pub use max::Max;
pub use min::Min;
pub use sum::Sum;

use crate::models::ElementValue;

use self::lazy_sorted_set::LazySortedSet;

#[derive(Debug, Clone)]
pub enum ValueAccumulator {
    Sum {
        value: f64,
    },
    Avg {
        sum: f64,
        count: i64,
    },
    Count {
        value: i64,
    },
    TimeMarker {
        timestamp: u64,
    },
    Signature(u64),
    LinearGradient {
        count: i64,
        mean_x: f64,
        mean_y: f64,
        m2: f64,
        cov: f64,
    },
    Value(ElementValue),
}

#[derive(Clone)]
pub enum Accumulator {
    Value(ValueAccumulator),
    LazySortedSet(LazySortedSet),
}
