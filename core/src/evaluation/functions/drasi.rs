mod max;
mod min;
mod stdevp;
#[cfg(test)]
mod tests;

pub use max::DrasiMax;
pub use min::DrasiMin;
use std::sync::Arc;
pub use stdevp::DrasiStdevP;

use super::{Function, FunctionRegistry};

pub trait RegisterDrasiFunctions {
    fn register_drasi_functions(&self);
}

impl RegisterDrasiFunctions for FunctionRegistry {
    fn register_drasi_functions(&self) {
        self.register_function("drasi.listMax", Function::Scalar(Arc::new(DrasiMax {})));
        self.register_function("drasi.listMin", Function::Scalar(Arc::new(DrasiMin {})));
        self.register_function("drasi.stdevp", Function::Scalar(Arc::new(DrasiStdevP {})));
    }
}
