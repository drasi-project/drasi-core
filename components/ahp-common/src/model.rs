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

//! Graph schema shared by the AHP source and bootstrap provider: labels,
//! element-ID generation, and the projected element representation.
//!
//! Both plugins MUST build elements only through this module so the IDs and
//! property shapes emitted during bootstrap match the ones emitted while
//! streaming.

use std::sync::Arc;

use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
};
use serde_json::{Map, Value};

/// Node labels.
pub mod labels {
    /// An agent provider advertised in the root state (`RootState.agents`).
    pub const AGENT: &str = "Agent";
    /// An agent session (`ahp-session:/<id>`).
    pub const SESSION: &str = "Session";
    /// A chat inside a session (`ahp-chat:/<id>`).
    pub const CHAT: &str = "Chat";
    /// An outstanding request for user input on a session.
    pub const INPUT_REQUEST: &str = "InputRequest";
    /// An automation definition from the `ahp-automations://` catalogue.
    pub const AUTOMATION: &str = "Automation";
    /// A run of an automation (`ahp-automation-run:/<id>`).
    pub const AUTOMATION_RUN: &str = "AutomationRun";
}

/// Relation types.
pub mod relations {
    /// `(Session)-[:USES_AGENT]->(Agent)`
    pub const USES_AGENT: &str = "USES_AGENT";
    /// `(Session)-[:HAS_CHAT]->(Chat)`
    pub const HAS_CHAT: &str = "HAS_CHAT";
    /// `(Session)-[:NEEDS_INPUT]->(InputRequest)`
    pub const NEEDS_INPUT: &str = "NEEDS_INPUT";
    /// `(InputRequest)-[:IN_CHAT]->(Chat)`
    pub const IN_CHAT: &str = "IN_CHAT";
    /// `(Automation)-[:HAS_RUN]->(AutomationRun)`
    pub const HAS_RUN: &str = "HAS_RUN";
    /// `(AutomationRun)-[:CREATED_SESSION]->(Session)`
    pub const CREATED_SESSION: &str = "CREATED_SESSION";
}

/// Well-known channel URI of the automations catalogue.
pub const AUTOMATIONS_URI: &str = "ahp-automations://";

/// Session status bit flags (`SessionStatus` in the AHP specification).
pub mod status {
    /// Session is idle.
    pub const IDLE: u32 = 1;
    /// Session ended with an error.
    pub const ERROR: u32 = 2;
    /// A turn is in progress.
    pub const IN_PROGRESS: u32 = 8;
    /// A turn is in progress and is blocked on user input (includes `IN_PROGRESS`).
    pub const INPUT_NEEDED: u32 = 24;
    /// The session has been read by the user.
    pub const IS_READ: u32 = 32;
    /// The session is archived.
    pub const IS_ARCHIVED: u32 = 64;
}

/// Element ID of an `Agent` node.
pub fn agent_id(provider: &str) -> String {
    format!("ahp-agent:{provider}")
}

/// Element ID of an `InputRequest` node.
pub fn input_request_id(session_uri: &str, request_id: &str) -> String {
    format!("{session_uri}#input:{request_id}")
}

/// Element ID of a relation. Stable for a given `(from, type, to)` triple.
pub fn relation_id(from: &str, rel_type: &str, to: &str) -> String {
    format!("{from}|{rel_type}|{to}")
}

/// Human-readable state derived from the session status flags.
pub fn status_state(status: u32) -> &'static str {
    if status & status::ERROR != 0 {
        "error"
    } else if status & status::INPUT_NEEDED == status::INPUT_NEEDED {
        "input-needed"
    } else if status & status::IN_PROGRESS != 0 {
        "in-progress"
    } else if status & status::IDLE != 0 {
        "idle"
    } else {
        "unknown"
    }
}

/// Insert the raw status plus its decoded flags into a property map.
pub(crate) fn insert_status_props(props: &mut Map<String, Value>, status: u32) {
    props.insert("status".into(), Value::from(status));
    props.insert("state".into(), Value::from(status_state(status)));
    props.insert("isIdle".into(), Value::from(status & status::IDLE != 0));
    props.insert("hasError".into(), Value::from(status & status::ERROR != 0));
    props.insert(
        "isInProgress".into(),
        Value::from(status & status::IN_PROGRESS != 0),
    );
    props.insert(
        "needsInput".into(),
        Value::from(status & status::INPUT_NEEDED == status::INPUT_NEEDED),
    );
    props.insert("isRead".into(), Value::from(status & status::IS_READ != 0));
    props.insert(
        "isArchived".into(),
        Value::from(status & status::IS_ARCHIVED != 0),
    );
}

/// Kind of a projected graph element.
#[derive(Debug, Clone, PartialEq)]
pub enum ProjectedKind {
    /// A node.
    Node,
    /// A relation from `from` to `to` (element IDs).
    Relation {
        /// Start node element ID.
        from: String,
        /// End node element ID.
        to: String,
    },
}

/// Source-independent representation of one graph element derived from AHP
/// state. Diffing two projections yields the `SourceChange`s to emit.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectedElement {
    /// Stable element ID.
    pub id: String,
    /// Single label (node label or relation type).
    pub label: &'static str,
    /// Node or relation.
    pub kind: ProjectedKind,
    /// Properties as JSON.
    pub properties: Map<String, Value>,
}

impl ProjectedElement {
    /// Create a node.
    pub fn node(
        id: impl Into<String>,
        label: &'static str,
        properties: Map<String, Value>,
    ) -> Self {
        Self {
            id: id.into(),
            label,
            kind: ProjectedKind::Node,
            properties,
        }
    }

    /// Create a property-less relation with an ID derived from its endpoints.
    pub fn relation(from: &str, label: &'static str, to: &str) -> Self {
        Self {
            id: relation_id(from, label, to),
            label,
            kind: ProjectedKind::Relation {
                from: from.to_string(),
                to: to.to_string(),
            },
            properties: Map::new(),
        }
    }

    /// Whether this element is a relation.
    pub fn is_relation(&self) -> bool {
        matches!(self.kind, ProjectedKind::Relation { .. })
    }

    /// Whether this element passes a bootstrap label filter. Empty filters
    /// for both nodes and relations select everything.
    pub fn matches_labels(&self, node_labels: &[String], relation_labels: &[String]) -> bool {
        if node_labels.is_empty() && relation_labels.is_empty() {
            return true;
        }
        let labels = if self.is_relation() {
            relation_labels
        } else {
            node_labels
        };
        labels.iter().any(|l| l == self.label)
    }

    fn metadata(&self, source_id: &str, effective_from: u64) -> ElementMetadata {
        ElementMetadata {
            reference: ElementReference::new(source_id, &self.id),
            labels: Arc::from(vec![Arc::from(self.label)]),
            effective_from,
        }
    }

    /// Convert to a Drasi [`Element`].
    pub fn to_element(&self, source_id: &str, effective_from: u64) -> Element {
        let metadata = self.metadata(source_id, effective_from);
        let properties = ElementPropertyMap::from(&self.properties);
        match &self.kind {
            ProjectedKind::Node => Element::Node {
                metadata,
                properties,
            },
            ProjectedKind::Relation { from, to } => Element::Relation {
                metadata,
                in_node: ElementReference::new(source_id, from),
                out_node: ElementReference::new(source_id, to),
                properties,
            },
        }
    }

    /// `SourceChange::Insert` for this element.
    pub fn to_insert(&self, source_id: &str, effective_from: u64) -> SourceChange {
        SourceChange::Insert {
            element: self.to_element(source_id, effective_from),
        }
    }

    /// `SourceChange::Update` for this element.
    pub fn to_update(&self, source_id: &str, effective_from: u64) -> SourceChange {
        SourceChange::Update {
            element: self.to_element(source_id, effective_from),
        }
    }

    /// `SourceChange::Delete` for this element.
    pub fn to_delete(&self, source_id: &str, effective_from: u64) -> SourceChange {
        SourceChange::Delete {
            metadata: self.metadata(source_id, effective_from),
        }
    }
}

/// Session selection rules shared by the source and bootstrap provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionFilter {
    /// Only include sessions from these agent providers. Empty = all.
    pub providers: Vec<String>,
    /// Include sessions whose status has the `IsArchived` flag.
    pub include_archived: bool,
}

impl Default for SessionFilter {
    fn default() -> Self {
        Self {
            providers: Vec::new(),
            include_archived: true,
        }
    }
}

impl SessionFilter {
    /// Whether a session with the given provider and status is selected.
    pub fn includes(&self, provider: &str, status: u32) -> bool {
        if !self.providers.is_empty() && !self.providers.iter().any(|p| p == provider) {
            return false;
        }
        self.include_archived || status & status::IS_ARCHIVED == 0
    }
}

/// Milliseconds since the Unix epoch, for `effective_from`.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_state_prefers_error_then_input() {
        assert_eq!(status_state(status::ERROR | status::IDLE), "error");
        assert_eq!(status_state(status::INPUT_NEEDED), "input-needed");
        assert_eq!(status_state(status::IN_PROGRESS), "in-progress");
        assert_eq!(status_state(status::IDLE | status::IS_READ), "idle");
        assert_eq!(status_state(0), "unknown");
    }

    #[test]
    fn needs_input_requires_both_bits() {
        let mut props = Map::new();
        insert_status_props(&mut props, status::IN_PROGRESS);
        assert_eq!(props["needsInput"], Value::Bool(false));
        assert_eq!(props["isInProgress"], Value::Bool(true));
        insert_status_props(&mut props, status::INPUT_NEEDED);
        assert_eq!(props["needsInput"], Value::Bool(true));
    }

    #[test]
    fn filter_by_provider_and_archive() {
        let filter = SessionFilter {
            providers: vec!["copilot".into()],
            include_archived: false,
        };
        assert!(filter.includes("copilot", status::IDLE));
        assert!(!filter.includes("other", status::IDLE));
        assert!(!filter.includes("copilot", status::IDLE | status::IS_ARCHIVED));
        assert!(SessionFilter::default().includes("any", status::IS_ARCHIVED));
    }

    #[test]
    fn label_filter() {
        let node = ProjectedElement::node("a", labels::SESSION, Map::new());
        let rel = ProjectedElement::relation("a", relations::HAS_CHAT, "b");
        assert!(node.matches_labels(&[], &[]));
        assert!(node.matches_labels(&["Session".into()], &[]));
        assert!(!node.matches_labels(&["Chat".into()], &["HAS_CHAT".into()]));
        assert!(rel.matches_labels(&["Chat".into()], &["HAS_CHAT".into()]));
        assert_eq!(rel.id, "a|HAS_CHAT|b");
    }
}
