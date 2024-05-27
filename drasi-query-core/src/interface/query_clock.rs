use std::fmt::Debug;

pub trait QueryClock: Debug + Send + Sync {
    fn get_transaction_time(&self) -> u64;
    fn get_realtime(&self) -> u64;
}
