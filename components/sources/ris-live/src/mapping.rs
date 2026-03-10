// Copyright 2025 The Drasi Authors.
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

//! BGP-to-graph mapping logic for RIS Live source messages.

use std::collections::HashSet;
use std::sync::Arc;

use chrono::Utc;
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::messages::{message_timestamp_millis, AsPathSegment, RisMessageData};

/// Runtime graph state for mapping INSERT vs UPDATE vs DELETE.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StreamState {
    /// Known peer node IDs (`{host}|{peer_ip}`).
    pub known_peers: HashSet<String>,
    /// Known prefix node IDs (`{prefix}`).
    pub known_prefixes: HashSet<String>,
    /// Active ROUTES relationship IDs (`{peer_ip}|{prefix}`).
    pub active_routes: HashSet<String>,
}

/// Persisted representation of [`StreamState`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PersistedStreamState {
    pub known_peers: Vec<String>,
    pub known_prefixes: Vec<String>,
    pub active_routes: Vec<String>,
}

impl From<&StreamState> for PersistedStreamState {
    fn from(state: &StreamState) -> Self {
        let mut known_peers = state.known_peers.iter().cloned().collect::<Vec<_>>();
        let mut known_prefixes = state.known_prefixes.iter().cloned().collect::<Vec<_>>();
        let mut active_routes = state.active_routes.iter().cloned().collect::<Vec<_>>();
        known_peers.sort();
        known_prefixes.sort();
        active_routes.sort();
        Self {
            known_peers,
            known_prefixes,
            active_routes,
        }
    }
}

impl From<PersistedStreamState> for StreamState {
    fn from(state: PersistedStreamState) -> Self {
        Self {
            known_peers: state.known_peers.into_iter().collect(),
            known_prefixes: state.known_prefixes.into_iter().collect(),
            active_routes: state.active_routes.into_iter().collect(),
        }
    }
}

/// Converts RIS messages to graph `SourceChange` events.
pub struct GraphMapper {
    source_id: String,
    state: StreamState,
}

impl GraphMapper {
    /// Create a mapper with initial state.
    pub fn new(source_id: impl Into<String>, state: StreamState) -> Self {
        Self {
            source_id: source_id.into(),
            state,
        }
    }

    /// Returns current state.
    pub fn state(&self) -> &StreamState {
        &self.state
    }

    /// Process announcement entries from an UPDATE message.
    pub fn process_announcements(&mut self, message: &RisMessageData) -> Vec<SourceChange> {
        let peer_id = match peer_node_id(message) {
            Some(value) => value,
            None => return Vec::new(),
        };

        let effective_from = effective_from(message);
        let mut changes = Vec::new();

        if self.state.known_peers.insert(peer_id.clone()) {
            changes.push(SourceChange::Insert {
                element: build_peer_node(&self.source_id, &peer_id, message, effective_from),
            });
        }

        for announcement in message.announcements.as_ref().into_iter().flatten() {
            for prefix in &announcement.prefixes {
                if self.state.known_prefixes.insert(prefix.clone()) {
                    changes.push(SourceChange::Insert {
                        element: build_prefix_node(&self.source_id, prefix, effective_from),
                    });
                }

                let route_id = route_id(message.peer.as_deref(), prefix);
                let route_element = build_route_relation(
                    &self.source_id,
                    &peer_id,
                    prefix,
                    &route_id,
                    message,
                    &announcement.next_hop,
                    effective_from,
                );

                if self.state.active_routes.insert(route_id) {
                    changes.push(SourceChange::Insert {
                        element: route_element,
                    });
                } else {
                    changes.push(SourceChange::Update {
                        element: route_element,
                    });
                }
            }
        }

        changes
    }

    /// Process withdrawals from an UPDATE message.
    pub fn process_withdrawals(&mut self, message: &RisMessageData) -> Vec<SourceChange> {
        let effective_from = effective_from(message);
        let Some(peer) = message.peer.as_deref() else {
            return Vec::new();
        };

        let mut changes = Vec::new();
        for prefix in message.withdrawals.as_ref().into_iter().flatten() {
            let route_id = route_id(Some(peer), prefix);
            if self.state.active_routes.remove(&route_id) {
                changes.push(SourceChange::Delete {
                    metadata: relation_metadata(&self.source_id, &route_id, effective_from),
                });
            }
        }
        changes
    }

    /// Process `RIS_PEER_STATE` messages to upsert peer state.
    pub fn process_peer_state(&mut self, message: &RisMessageData) -> Vec<SourceChange> {
        let peer_id = match peer_node_id(message) {
            Some(value) => value,
            None => return Vec::new(),
        };

        let effective_from = effective_from(message);
        let peer_node = build_peer_node(&self.source_id, &peer_id, message, effective_from);

        if self.state.known_peers.insert(peer_id) {
            vec![SourceChange::Insert { element: peer_node }]
        } else {
            vec![SourceChange::Update { element: peer_node }]
        }
    }
}

fn effective_from(message: &RisMessageData) -> u64 {
    message_timestamp_millis(message)
        .and_then(|ts| u64::try_from(ts).ok())
        .unwrap_or_else(|| Utc::now().timestamp_millis().max(0) as u64)
}

fn peer_node_id(message: &RisMessageData) -> Option<String> {
    let host = message.host.as_deref()?;
    let peer = message.peer.as_deref()?;
    Some(format!("{host}|{peer}"))
}

fn route_id(peer: Option<&str>, prefix: &str) -> String {
    match peer {
        Some(peer_ip) => format!("{peer_ip}|{prefix}"),
        None => format!("unknown|{prefix}"),
    }
}

fn relation_metadata(source_id: &str, route_id: &str, effective_from: u64) -> ElementMetadata {
    ElementMetadata {
        reference: ElementReference::new(source_id, route_id),
        labels: Arc::from(vec![Arc::from("ROUTES")]),
        effective_from,
    }
}

fn build_peer_node(
    source_id: &str,
    peer_id: &str,
    message: &RisMessageData,
    effective_from: u64,
) -> Element {
    let mut properties = ElementPropertyMap::new();
    if let Some(peer) = &message.peer {
        properties.insert("peer_ip", ElementValue::String(Arc::from(peer.as_str())));
    }
    if let Some(peer_asn) = &message.peer_asn {
        properties.insert(
            "peer_asn",
            ElementValue::String(Arc::from(peer_asn.as_str())),
        );
    }
    if let Some(host) = &message.host {
        properties.insert("host", ElementValue::String(Arc::from(host.as_str())));
    }
    if let Some(state) = &message.state {
        properties.insert("state", ElementValue::String(Arc::from(state.as_str())));
    }
    if let Some(id) = &message.id {
        properties.insert("msg_id", ElementValue::String(Arc::from(id.as_str())));
    }
    if let Some(timestamp) = message.timestamp {
        properties.insert("timestamp", ElementValue::from(&json!(timestamp)));
    }

    Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new(source_id, peer_id),
            labels: Arc::from(vec![Arc::from("Peer")]),
            effective_from,
        },
        properties,
    }
}

fn build_prefix_node(source_id: &str, prefix: &str, effective_from: u64) -> Element {
    let mut properties = ElementPropertyMap::new();
    properties.insert("prefix", ElementValue::String(Arc::from(prefix)));

    Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new(source_id, prefix),
            labels: Arc::from(vec![Arc::from("Prefix")]),
            effective_from,
        },
        properties,
    }
}

fn build_route_relation(
    source_id: &str,
    peer_id: &str,
    prefix: &str,
    route_id: &str,
    message: &RisMessageData,
    next_hop: &str,
    effective_from: u64,
) -> Element {
    let mut properties = ElementPropertyMap::new();
    properties.insert("next_hop", ElementValue::String(Arc::from(next_hop)));
    properties.insert("prefix", ElementValue::String(Arc::from(prefix)));

    if let Some(peer) = &message.peer {
        properties.insert("peer", ElementValue::String(Arc::from(peer.as_str())));
    }
    if let Some(peer_asn) = &message.peer_asn {
        properties.insert(
            "peer_asn",
            ElementValue::String(Arc::from(peer_asn.as_str())),
        );
    }
    if let Some(host) = &message.host {
        properties.insert("host", ElementValue::String(Arc::from(host.as_str())));
    }
    if let Some(origin) = &message.origin {
        properties.insert("origin", ElementValue::String(Arc::from(origin.as_str())));
    }
    if let Some(timestamp) = message.timestamp {
        properties.insert("timestamp", ElementValue::from(&json!(timestamp)));
    }
    if let Some(id) = &message.id {
        properties.insert("msg_id", ElementValue::String(Arc::from(id.as_str())));
    }

    if let Some(path) = &message.path {
        if let Ok(serialized) = serde_json::to_string(path) {
            properties.insert("path", ElementValue::String(Arc::from(serialized.as_str())));
        }
        properties.insert("path_length", ElementValue::Integer(path_length(path)));
        if let Some(origin_asn) = origin_asn(path) {
            properties.insert("origin_asn", ElementValue::Integer(origin_asn));
        }
    }

    if let Some(community) = &message.community {
        if let Ok(serialized) = serde_json::to_string(community) {
            properties.insert(
                "community",
                ElementValue::String(Arc::from(serialized.as_str())),
            );
        }
    }

    Element::Relation {
        metadata: relation_metadata(source_id, route_id, effective_from),
        in_node: ElementReference::new(source_id, peer_id),
        out_node: ElementReference::new(source_id, prefix),
        properties,
    }
}

fn path_length(path: &[AsPathSegment]) -> i64 {
    path.iter()
        .map(|segment| match segment {
            AsPathSegment::Asn(_) => 1i64,
            AsPathSegment::AsSet(as_set) => i64::try_from(as_set.len()).unwrap_or(i64::MAX),
        })
        .sum()
}

fn origin_asn(path: &[AsPathSegment]) -> Option<i64> {
    for segment in path.iter().rev() {
        match segment {
            AsPathSegment::Asn(asn) => return Some(*asn),
            AsPathSegment::AsSet(as_set) => {
                if let Some(last) = as_set.last() {
                    return Some(*last);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use drasi_core::models::Element;

    use crate::messages::Announcement;

    use super::{GraphMapper, RisMessageData, SourceChange, StreamState};

    fn base_update() -> RisMessageData {
        RisMessageData {
            timestamp: Some(1_773_098_494.83),
            peer: Some("208.80.153.193".to_string()),
            peer_asn: Some("14907".to_string()),
            id: Some("msg-1".to_string()),
            host: Some("rrc00.ripe.net".to_string()),
            msg_type: Some("UPDATE".to_string()),
            path: None,
            origin: Some("IGP".to_string()),
            community: None,
            announcements: Some(vec![Announcement {
                next_hop: "208.80.153.193".to_string(),
                prefixes: vec!["203.0.113.0/24".to_string()],
            }]),
            withdrawals: Some(Vec::new()),
            state: None,
        }
    }

    #[test]
    fn announcement_creates_peer_prefix_and_route() {
        let mut mapper = GraphMapper::new("ris-source", StreamState::default());
        let message = base_update();

        let changes = mapper.process_announcements(&message);
        assert_eq!(changes.len(), 3);

        let peer_inserted = changes.iter().any(|change| {
            matches!(
                change,
                SourceChange::Insert {
                    element: Element::Node { metadata, .. }
                } if metadata.labels.iter().any(|label| label.as_ref() == "Peer")
            )
        });
        assert!(peer_inserted);

        let prefix_inserted = changes.iter().any(|change| {
            matches!(
                change,
                SourceChange::Insert {
                    element: Element::Node { metadata, .. }
                } if metadata.labels.iter().any(|label| label.as_ref() == "Prefix")
            )
        });
        assert!(prefix_inserted);

        let route_inserted = changes.iter().any(|change| {
            matches!(
                change,
                SourceChange::Insert {
                    element: Element::Relation {
                        metadata,
                        in_node,
                        out_node,
                        ..
                    }
                } if metadata.labels.iter().any(|label| label.as_ref() == "ROUTES")
                    && in_node.element_id.as_ref() == "rrc00.ripe.net|208.80.153.193"
                    && out_node.element_id.as_ref() == "203.0.113.0/24"
            )
        });
        assert!(route_inserted);
    }

    #[test]
    fn reannouncement_updates_existing_route() {
        let mut mapper = GraphMapper::new("ris-source", StreamState::default());
        let message = base_update();
        let _ = mapper.process_announcements(&message);

        let mut second = base_update();
        second.id = Some("msg-2".to_string());
        second.announcements = Some(vec![Announcement {
            next_hop: "198.51.100.10".to_string(),
            prefixes: vec!["203.0.113.0/24".to_string()],
        }]);

        let changes = mapper.process_announcements(&second);
        assert_eq!(changes.len(), 1);
        assert!(matches!(
            changes[0],
            SourceChange::Update {
                element: Element::Relation { .. }
            }
        ));
    }

    #[test]
    fn withdrawal_deletes_existing_route() {
        let mut mapper = GraphMapper::new("ris-source", StreamState::default());
        let message = base_update();
        let _ = mapper.process_announcements(&message);

        let mut withdraw = base_update();
        withdraw.announcements = None;
        withdraw.withdrawals = Some(vec!["203.0.113.0/24".to_string()]);

        let changes = mapper.process_withdrawals(&withdraw);
        assert_eq!(changes.len(), 1);
        assert!(matches!(changes[0], SourceChange::Delete { .. }));
    }

    #[test]
    fn peer_state_upserts_peer_node() {
        let mut mapper = GraphMapper::new("ris-source", StreamState::default());
        let mut peer_state = base_update();
        peer_state.msg_type = Some("RIS_PEER_STATE".to_string());
        peer_state.state = Some("down".to_string());
        peer_state.announcements = None;
        peer_state.withdrawals = None;

        let first = mapper.process_peer_state(&peer_state);
        assert!(matches!(
            first[0],
            SourceChange::Insert {
                element: Element::Node { .. }
            }
        ));

        let second = mapper.process_peer_state(&peer_state);
        assert!(matches!(
            second[0],
            SourceChange::Update {
                element: Element::Node { .. }
            }
        ));
    }
}
