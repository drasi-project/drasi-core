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

use drasi_core::models::{Element, SourceChange};

#[derive(Debug)]
pub enum ModelChangeType {
    Insert,
    Update,
    Delete,
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
            change_type: ModelChangeType::Insert,
            node_before: None,
            node_after: Some(node_after.clone()),
            node_source_change: Some(SourceChange::Insert {
                element: node_after,
            }),
        }
    }

    pub fn new_update_node(node_before: Element, node_after: Element) -> ModelChangeResult {
        ModelChangeResult {
            change_type: ModelChangeType::Update,
            node_before: Some(node_before),
            node_after: Some(node_after.clone()),
            node_source_change: Some(SourceChange::Update {
                element: node_after,
            }),
        }
    }

    pub fn new_delete_node(node_before: Element) -> ModelChangeResult {
        ModelChangeResult {
            change_type: ModelChangeType::Delete,
            node_before: Some(node_before.clone()),
            node_after: None,
            node_source_change: Some(SourceChange::Delete {
                metadata: node_before.get_metadata().clone(),
            }),
        }
    }
}
