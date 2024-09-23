use super::StoredElementReference;
use drasi_core::interface::FutureElementRef;

#[derive(prost::Message, Hash)]
pub struct StoredFutureElementRef {
    #[prost(message, required, tag = "1")]
    pub element_ref: StoredElementReference,
    #[prost(uint64, tag = "2")]
    pub original_time: u64,
    #[prost(uint64, tag = "3")]
    pub due_time: u64,
    #[prost(uint64, tag = "4")]
    pub group_signature: u64,
    #[prost(uint32, tag = "5")]
    pub position_in_query: u32,
}

impl From<StoredFutureElementRef> for FutureElementRef {
    fn from(val: StoredFutureElementRef) -> Self {
        FutureElementRef {
            element_ref: val.element_ref.into(),
            original_time: val.original_time,
            due_time: val.due_time,
            group_signature: val.group_signature,
        }
    }
}
