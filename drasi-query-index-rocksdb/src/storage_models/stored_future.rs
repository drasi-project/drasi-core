use super::StoredElementReference;
use drasi_query_core::interface::FutureElementRef;

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

impl Into<FutureElementRef> for StoredFutureElementRef {
    fn into(self) -> FutureElementRef {
        FutureElementRef {
            element_ref: self.element_ref.into(),
            original_time: self.original_time,
            due_time: self.due_time,
            group_signature: self.group_signature,
        }
    }
}
