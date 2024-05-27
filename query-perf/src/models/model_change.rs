use drasi_query_core::models::{Element, SourceChange};

#[derive(Debug)]
pub enum ModelChangeType {
    InsertNode,
    UpdateNode,
    DeleteNode,
}

#[derive(Debug)]
pub struct ModelChangeResult {
    pub change_type: ModelChangeType,
    pub node_before: Option<Element>,
    pub node_after: Option<Element>,
    pub node_source_change: Option<SourceChange>,
}

impl ModelChangeResult {
    pub fn new_insert_node(node_after: Element) -> ModelChangeResult {
        ModelChangeResult {
            change_type: ModelChangeType::InsertNode,
            node_before: None,
            node_after: Some(node_after.clone()),
            node_source_change: Some(SourceChange::Insert {
                element: node_after,
            }),
        }
    }

    pub fn new_update_node(node_before: Element, node_after: Element) -> ModelChangeResult {
        ModelChangeResult {
            change_type: ModelChangeType::UpdateNode,
            node_before: Some(node_before),
            node_after: Some(node_after.clone()),
            node_source_change: Some(SourceChange::Update {
                element: node_after,
            }),
        }
    }

    pub fn new_delete_node(node_before: Element) -> ModelChangeResult {
        ModelChangeResult {
            change_type: ModelChangeType::DeleteNode,
            node_before: Some(node_before.clone()),
            node_after: None,
            node_source_change: Some(SourceChange::Delete {
                metadata: node_before.get_metadata().clone(),
            }),
        }
    }
}
