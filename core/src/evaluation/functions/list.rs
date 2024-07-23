mod reduce;
mod tail;
mod range;

use super::{Function, FunctionRegistry};
use std::sync::Arc;

pub use reduce::Reduce;
pub use tail::Tail;
pub use range::Range;

pub trait RegisterListFunctions {
    fn register_list_functions(&self);
}

impl RegisterListFunctions for FunctionRegistry {
    fn register_list_functions(&self) {
        self.register_function("reduce", Function::Scalar(Arc::new(Reduce {})));
        self.register_function("tail", Function::Scalar(Arc::new(Tail {})));
        self.register_function("range", Function::Scalar(Arc::new(Range {})));
    }
}
