// Copyright 2024 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

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
