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
