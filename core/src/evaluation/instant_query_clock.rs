use crate::{interface::QueryClock, models::SourceChange};

#[derive(Debug)]
pub struct InstantQueryClock {
    transaction_time: u64,
    realtime: u64,
}

impl InstantQueryClock {
    pub fn new(transaction_time: u64, realtime: u64) -> InstantQueryClock {
        InstantQueryClock {
            transaction_time,
            realtime,
        }
    }

    pub fn from_source_change(change: &SourceChange) -> InstantQueryClock {
        InstantQueryClock {
            transaction_time: change.get_transaction_time(),
            realtime: change.get_realtime(),
        }
    }
}

impl QueryClock for InstantQueryClock {
    fn get_transaction_time(&self) -> u64 {
        self.transaction_time
    }

    fn get_realtime(&self) -> u64 {
        self.realtime
    }
}
