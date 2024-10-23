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

use crate::interface::FutureElementRef;

use super::{Element, ElementMetadata, ElementReference, ElementTimestamp};

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum SourceChange {
    Insert { element: Element },
    Update { element: Element },
    Delete { metadata: ElementMetadata },
    Future { future_ref: FutureElementRef },
}

impl SourceChange {
    pub fn get_reference(&self) -> &ElementReference {
        match self {
            SourceChange::Insert { element } => element.get_reference(),
            SourceChange::Update { element } => element.get_reference(),
            SourceChange::Delete { metadata } => &metadata.reference,
            SourceChange::Future { future_ref } => &future_ref.element_ref,
        }
    }

    pub fn get_transaction_time(&self) -> ElementTimestamp {
        match self {
            SourceChange::Insert { element } => element.get_effective_from(),
            SourceChange::Update { element } => element.get_effective_from(),
            SourceChange::Delete { metadata } => metadata.effective_from,
            SourceChange::Future { future_ref } => future_ref.original_time,
        }
    }

    pub fn get_realtime(&self) -> ElementTimestamp {
        match self {
            SourceChange::Insert { element } => element.get_effective_from(),
            SourceChange::Update { element } => element.get_effective_from(),
            SourceChange::Delete { metadata } => metadata.effective_from,
            SourceChange::Future { future_ref } => future_ref.due_time,
        }
    }
}
