// Copyright 2026 The Drasi Authors.
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

use std::{collections::HashSet, sync::Arc};

use drasi_core::models::{Element, SourceChange, SourceChangeNormalizer};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LabelSelection {
    Labels(HashSet<String>),
    AllExcept(HashSet<String>),
}

impl LabelSelection {
    pub fn matches(&self, labels: &[Arc<str>]) -> bool {
        match self {
            Self::Labels(included) => labels.iter().any(|label| included.contains(label.as_ref())),
            Self::AllExcept(excluded) => {
                labels.is_empty()
                    || labels
                        .iter()
                        .any(|label| !excluded.contains(label.as_ref()))
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        matches!(self, Self::Labels(labels) if labels.is_empty())
    }

    /// Legacy bootstrap requests cannot express exclusions or "none". Request a
    /// superset and enforce the exact selection on every event at query ingestion.
    pub fn bootstrap_labels(&self) -> HashSet<String> {
        match self {
            Self::Labels(labels) => labels.clone(),
            Self::AllExcept(_) => HashSet::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceSelection {
    pub nodes: LabelSelection,
    pub relations: LabelSelection,
}

impl SourceSelection {
    pub fn normalize(&self, change: SourceChange) -> SourceChange {
        let element = match &change {
            SourceChange::Insert { element } | SourceChange::Update { element } => element,
            SourceChange::Delete { .. } | SourceChange::Future { .. } => return change,
        };
        let selection = match element {
            Element::Node { .. } => &self.nodes,
            Element::Relation { .. } => &self.relations,
        };
        if selection.matches(&element.get_metadata().labels) {
            change
        } else {
            SourceChange::Delete {
                metadata: element.get_metadata().clone(),
            }
        }
    }
}

impl SourceChangeNormalizer for SourceSelection {
    fn normalize(&self, change: SourceChange) -> SourceChange {
        self.normalize(change)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_core::models::{ElementMetadata, ElementPropertyMap, ElementReference};

    fn labels(values: &[&str]) -> Vec<Arc<str>> {
        values.iter().map(|value| Arc::from(*value)).collect()
    }

    fn selection() -> SourceSelection {
        SourceSelection {
            nodes: LabelSelection::AllExcept(HashSet::from(["Foreign".into()])),
            relations: LabelSelection::Labels(HashSet::from(["R".into()])),
        }
    }

    fn metadata(node_labels: &[&str]) -> ElementMetadata {
        ElementMetadata {
            reference: ElementReference::new("source", "node"),
            labels: labels(node_labels).into(),
            effective_from: 20,
        }
    }

    #[test]
    fn none_and_all_are_distinct() {
        let none = LabelSelection::Labels(HashSet::new());
        let all = LabelSelection::AllExcept(HashSet::new());
        assert!(none.is_empty());
        assert!(!all.is_empty());
        assert!(!none.matches(&[]));
        assert!(!none.matches(&labels(&["Start"])));
        assert!(all.matches(&[]));
        assert!(all.matches(&labels(&["Start"])));
    }

    #[test]
    fn wildcard_preserves_eligible_labels_and_unlabeled_nodes() {
        let selected = selection();
        assert!(!selected.nodes.matches(&labels(&["Foreign"])));
        assert!(selected.nodes.matches(&labels(&["Transit"])));
        assert!(selected.nodes.matches(&labels(&["Foreign", "Transit"])));
        assert!(selected.nodes.matches(&[]));
        assert!(!selected.relations.matches(&labels(&["Other"])));
        assert!(selected.relations.matches(&labels(&["R"])));
    }

    #[test]
    fn leaving_selection_becomes_a_deletion() {
        for update in [false, true] {
            let element = Element::Node {
                metadata: metadata(&["Foreign"]),
                properties: ElementPropertyMap::new(),
            };
            let change = if update {
                SourceChange::Update { element }
            } else {
                SourceChange::Insert { element }
            };
            let normalized = selection().normalize(change);
            let SourceChange::Delete { metadata: deleted } = normalized else {
                panic!("excluded data must remove an earlier admitted version");
            };
            assert_eq!(deleted.reference, ElementReference::new("source", "node"));
            assert_eq!(deleted.effective_from, 20);
        }
    }

    #[test]
    fn admitted_updates_and_label_less_deletes_pass_through() {
        let update = selection().normalize(SourceChange::Update {
            element: Element::Node {
                metadata: metadata(&["Transit"]),
                properties: ElementPropertyMap::new(),
            },
        });
        assert!(matches!(update, SourceChange::Update { .. }));
        let deleted = selection().normalize(SourceChange::Delete {
            metadata: metadata(&[]),
        });
        assert!(matches!(deleted, SourceChange::Delete { .. }));
    }
}
