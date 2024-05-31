mod temporal_instant;
#[cfg(test)]
mod tests;

use std::sync::Arc;

use self::temporal_instant::{
    Clock, ClockFunction, ClockResult, Date, DateTime, LocalDateTime, LocalTime, Time, Truncate,
};

use super::{Function, FunctionRegistry};

pub trait RegisterTemporalInstantFunctions {
    fn register_temporal_instant_functions(&self);
}

impl RegisterTemporalInstantFunctions for FunctionRegistry {
    fn register_temporal_instant_functions(&self) {
        self.register_function("date", Function::Scalar(Arc::new(Date {})));
        self.register_function("date.truncate", Function::Scalar(Arc::new(Truncate {})));
        self.register_function(
            "date.statement",
            Function::Scalar(Arc::new(ClockFunction::new(
                Clock::Statement,
                ClockResult::Date,
            ))),
        );
        self.register_function(
            "date.transaction",
            Function::Scalar(Arc::new(ClockFunction::new(
                Clock::Transaction,
                ClockResult::Date,
            ))),
        );
        self.register_function(
            "date.realtime",
            Function::Scalar(Arc::new(ClockFunction::new(
                Clock::RealTime,
                ClockResult::Date,
            ))),
        );
        self.register_function("localtime", Function::Scalar(Arc::new(LocalTime {})));
        self.register_function(
            "localtime.realtime",
            Function::Scalar(Arc::new(ClockFunction::new(
                Clock::RealTime,
                ClockResult::LocalTime,
            ))),
        );
        self.register_function(
            "localtime.transaction",
            Function::Scalar(Arc::new(ClockFunction::new(
                Clock::Transaction,
                ClockResult::LocalTime,
            ))),
        );
        self.register_function(
            "localtime.statement",
            Function::Scalar(Arc::new(ClockFunction::new(
                Clock::Statement,
                ClockResult::LocalTime,
            ))),
        );
        self.register_function(
            "localdatetime",
            Function::Scalar(Arc::new(LocalDateTime {})),
        );
        self.register_function(
            "localdatetime.statement",
            Function::Scalar(Arc::new(ClockFunction::new(
                Clock::Statement,
                ClockResult::LocalDateTime,
            ))),
        );
        self.register_function(
            "localdatetime.transaction",
            Function::Scalar(Arc::new(ClockFunction::new(
                Clock::Transaction,
                ClockResult::LocalDateTime,
            ))),
        );
        self.register_function(
            "localdatetime.realtime",
            Function::Scalar(Arc::new(ClockFunction::new(
                Clock::RealTime,
                ClockResult::LocalDateTime,
            ))),
        );
        self.register_function(
            "time.realtime",
            Function::Scalar(Arc::new(ClockFunction::new(
                Clock::RealTime,
                ClockResult::ZonedTime,
            ))),
        );
        self.register_function(
            "time.statement",
            Function::Scalar(Arc::new(ClockFunction::new(
                Clock::Statement,
                ClockResult::ZonedTime,
            ))),
        );
        self.register_function(
            "time.transaction",
            Function::Scalar(Arc::new(ClockFunction::new(
                Clock::Transaction,
                ClockResult::ZonedTime,
            ))),
        );
        self.register_function("time.truncate", Function::Scalar(Arc::new(Truncate {})));
        self.register_function("time", Function::Scalar(Arc::new(Time {})));
        self.register_function("datetime", Function::Scalar(Arc::new(DateTime {})));

        self.register_function(
            "datetime.transaction",
            Function::Scalar(Arc::new(ClockFunction::new(
                Clock::Transaction,
                ClockResult::ZonedDateTime,
            ))),
        );
        self.register_function(
            "datetime.realtime",
            Function::Scalar(Arc::new(ClockFunction::new(
                Clock::RealTime,
                ClockResult::ZonedDateTime,
            ))),
        );
        self.register_function(
            "datetime.statement",
            Function::Scalar(Arc::new(ClockFunction::new(
                Clock::Statement,
                ClockResult::ZonedDateTime,
            ))),
        );

        self.register_function("datetime.truncate", Function::Scalar(Arc::new(Truncate {})));
        self.register_function(
            "localtime.truncate",
            Function::Scalar(Arc::new(Truncate {})),
        );
        self.register_function(
            "localdatetime.truncate",
            Function::Scalar(Arc::new(Truncate {})),
        );
    }
}
