mod temporal_duration;

#[cfg(test)]
mod tests;

use std::sync::Arc;

use self::temporal_duration::{Between, DurationFunc, InDays, InMonths, InSeconds};

use super::{Function, FunctionRegistry};

pub trait RegisterTemporalDurationFunctions {
    fn register_temporal_duration_functions(&self);
}

impl RegisterTemporalDurationFunctions for FunctionRegistry {
    fn register_temporal_duration_functions(&self) {
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
