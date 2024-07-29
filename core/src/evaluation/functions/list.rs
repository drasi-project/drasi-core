mod reduce;
mod tail;

use super::{Function, FunctionRegistry};
use std::sync::Arc;

pub use reduce::Reduce;
pub use tail::Tail;

pub trait RegisterListFunctions {
    fn register_list_functions(&self);
}

impl RegisterListFunctions for FunctionRegistry {
    fn register_list_functions(&self) {
        self.register_function("reduce", Function::LazyScalar(Arc::new(Reduce::new())));
        self.register_function("tail", Function::Scalar(Arc::new(Tail {})));
    }
}
