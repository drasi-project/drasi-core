mod avg;
mod count;
mod last;
pub mod lazy_sorted_set;
mod linear_gradient;
mod max;
mod min;
mod sum;

use std::sync::Arc;

pub use avg::Avg;
pub use count::Count;
pub use last::Last;
pub use linear_gradient::LinearGradient;
pub use max::Max;
pub use min::Min;
pub use sum::Sum;

use crate::models::ElementValue;

use self::lazy_sorted_set::LazySortedSet;

use super::{Function, FunctionRegistry};

pub trait RegisterAggregationFunctions {
    fn register_aggregation_functions(&self);
}

impl RegisterAggregationFunctions for FunctionRegistry {
    fn register_aggregation_functions(&self) {
        self.register_function("sum", Function::Aggregating(Arc::new(Sum {})));
        self.register_function("avg", Function::Aggregating(Arc::new(Avg {})));
        self.register_function("count", Function::Aggregating(Arc::new(Count {})));
        self.register_function("min", Function::Aggregating(Arc::new(Min {})));
        self.register_function("max", Function::Aggregating(Arc::new(Max {})));
        self.register_function(
            "drasi.linearGradient",
            Function::Aggregating(Arc::new(LinearGradient {})),
        );
        self.register_function("drasi.last", Function::Aggregating(Arc::new(Last {})));
    }
}

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
