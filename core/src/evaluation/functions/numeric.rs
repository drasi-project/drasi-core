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

pub trait RegisterNumericFunctions {
    fn register_numeric_functions(&self);
}

impl RegisterNumericFunctions for FunctionRegistry {
    fn register_numeric_functions(&self) {
        self.register_function("abs", Function::Scalar(Arc::new(Abs {})));
        self.register_function("ceil", Function::Scalar(Arc::new(Ceil {})));
        self.register_function("floor", Function::Scalar(Arc::new(Floor {})));
        self.register_function("rand", Function::Scalar(Arc::new(Rand {})));
        self.register_function("round", Function::Scalar(Arc::new(Round {})));
        self.register_function("sign", Function::Scalar(Arc::new(Sign {})));
    }
}
