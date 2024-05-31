mod stored_accumulator;
mod stored_element;
mod stored_future;
mod stored_value;

mod redis_args;

//TODO: make more efficient by serializing elements to and from redis values directly, avoiding the intermediate StoredElement type
pub use stored_accumulator::{StoredValueAccumulator, StoredValueAccumulatorContainer};
pub use stored_element::{
    StoredElement, StoredElementContainer, StoredElementMetadata, StoredElementReference, StoredRelation,
};
pub use stored_future::{StoredFutureElementRef, StoredFutureElementRefWithContext};
pub use stored_value::{StoredValue, StoredValueMap};
