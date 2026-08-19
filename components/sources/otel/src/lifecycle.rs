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

//! Seen-id, cardinality, and TTL tracking for projected graph elements.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use drasi_core::models::{Element, ElementMetadata, ElementReference, SourceChange};
use serde::{Deserialize, Serialize};

use crate::config::OtelSourceConfig;

/// Category used for cardinality caps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ElementCategory {
    Service,
    Metric,
    Heartbeat,
    LogEvent,
    Reports,
    HeartbeatRel,
    DependsOn,
    Emits,
}

impl ElementCategory {
    fn cap(self, config: &OtelSourceConfig) -> Option<usize> {
        match self {
            Self::Service => Some(config.max_services),
            Self::Metric => Some(config.max_metrics),
            Self::DependsOn => Some(config.max_dependencies),
            Self::LogEvent => Some(config.max_log_events),
            Self::Heartbeat | Self::Reports | Self::HeartbeatRel | Self::Emits => None,
        }
    }

    fn is_capped(self) -> bool {
        matches!(
            self,
            Self::Service | Self::Metric | Self::DependsOn | Self::LogEvent
        )
    }
}

/// A mapped graph element before Insert vs Update is decided.
#[derive(Debug, Clone)]
pub struct ProjectedElement {
    pub id: String,
    pub labels: Vec<String>,
    pub properties: drasi_core::models::ElementPropertyMap,
    pub kind: ProjectedKind,
    pub effective_from: u64,
    pub category: ElementCategory,
    pub ttl_secs: Option<u64>,
    /// Elements that share a group are admitted or dropped together.
    pub group: u32,
}

/// Node vs relation payload.
#[derive(Debug, Clone)]
pub enum ProjectedKind {
    Node,
    Relation { from: String, to: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TtlRecord {
    id: String,
    labels: Vec<String>,
    expires_at: u64,
    #[serde(default)]
    category: Option<ElementCategory>,
}

/// Mutations that can be rolled back if WAL append fails after apply.
#[derive(Debug, Default)]
pub struct ApplyUndo {
    inserted: Vec<(String, ElementCategory)>,
    ttl_previous: Vec<(String, Option<TtlRecord>)>,
}

/// Source-internal lifecycle store. Not a bootstrap snapshot.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LifecycleState {
    seen: HashSet<String>,
    counts: HashMap<ElementCategory, usize>,
    ttl_by_id: HashMap<String, u64>,
    ttls: BTreeMap<u64, Vec<TtlRecord>>,
}

impl LifecycleState {
    /// Apply projected elements, returning graph changes and how many groups were dropped.
    ///
    /// TTL is scheduled from `received_at_millis` so late OTLP event timestamps do not
    /// expire edges immediately. `registeredAt` is set only on Service insert.
    pub fn apply(
        &mut self,
        source_id: &str,
        config: &OtelSourceConfig,
        projected: Vec<ProjectedElement>,
        received_at_millis: u64,
    ) -> (Vec<SourceChange>, usize, ApplyUndo) {
        let mut order = Vec::new();
        let mut groups: HashMap<u32, Vec<ProjectedElement>> = HashMap::new();
        for item in projected {
            groups
                .entry(item.group)
                .or_insert_with(|| {
                    order.push(item.group);
                    Vec::new()
                })
                .push(item);
        }

        let mut changes = Vec::new();
        let mut dropped = 0usize;
        let mut undo = ApplyUndo::default();

        for group_id in order {
            let Some(group) = groups.remove(&group_id) else {
                continue;
            };
            if self.group_exceeds_caps(config, &group) {
                dropped += 1;
                continue;
            }
            for mut item in group {
                let is_insert = self.seen.insert(item.id.clone());
                if is_insert {
                    *self.counts.entry(item.category).or_insert(0) += 1;
                    undo.inserted.push((item.id.clone(), item.category));
                    if item.category == ElementCategory::Service {
                        stamp_registered_at(&mut item);
                    }
                }
                if let Some(ttl_secs) = item.ttl_secs {
                    let previous = self.take_ttl_record(&item.id);
                    undo.ttl_previous.push((item.id.clone(), previous));
                    self.schedule_ttl(&item, ttl_secs, received_at_millis);
                }
                let element = item.into_element(source_id);
                if is_insert {
                    changes.push(SourceChange::Insert { element });
                } else {
                    changes.push(SourceChange::Update { element });
                }
            }
        }

        (changes, dropped, undo)
    }

    /// Roll back [`Self::apply`] after a failed WAL append.
    pub fn revert_apply(&mut self, undo: ApplyUndo) {
        for (id, previous) in undo.ttl_previous.into_iter().rev() {
            self.clear_ttl(&id);
            if let Some(record) = previous {
                self.ttl_by_id.insert(record.id.clone(), record.expires_at);
                self.ttls.entry(record.expires_at).or_default().push(record);
            }
        }
        for (id, category) in undo.inserted {
            if self.seen.remove(&id) {
                decrement(&mut self.counts, category);
            }
        }
    }

    fn group_exceeds_caps(&self, config: &OtelSourceConfig, group: &[ProjectedElement]) -> bool {
        let mut extra: HashMap<ElementCategory, usize> = HashMap::new();
        for item in group {
            if self.seen.contains(&item.id) {
                continue;
            }
            let Some(cap) = item.category.cap(config) else {
                continue;
            };
            let current = self.counts.get(&item.category).copied().unwrap_or(0)
                + extra.get(&item.category).copied().unwrap_or(0);
            if current >= cap {
                return true;
            }
            *extra.entry(item.category).or_insert(0) += 1;
        }
        false
    }

    /// Graph deletes that would be emitted by [`Self::expire`] without mutating state.
    pub fn preview_expire(&self, source_id: &str, now_millis: u64) -> Vec<SourceChange> {
        self.due_records(now_millis)
            .into_iter()
            .map(|record| delete_change(source_id, &record, now_millis))
            .collect()
    }

    fn due_records(&self, now_millis: u64) -> Vec<TtlRecord> {
        let mut due = Vec::new();
        for (expires, records) in &self.ttls {
            if *expires > now_millis {
                break;
            }
            for record in records {
                if self.ttl_by_id.get(&record.id) == Some(expires) {
                    due.push(record.clone());
                }
            }
        }
        due
    }

    /// Delete elements whose TTL has elapsed at `now_millis`.
    pub fn expire(&mut self, source_id: &str, now_millis: u64) -> Vec<SourceChange> {
        let due: Vec<u64> = self
            .ttls
            .keys()
            .copied()
            .take_while(|expires| *expires <= now_millis)
            .collect();

        let mut changes = Vec::new();
        for expires in due {
            if let Some(records) = self.ttls.remove(&expires) {
                for record in records {
                    if self.ttl_by_id.get(&record.id) != Some(&expires) {
                        continue;
                    }
                    self.ttl_by_id.remove(&record.id);
                    if self.seen.remove(&record.id) {
                        let category = record
                            .category
                            .or_else(|| category_from_labels(&record.labels));
                        if let Some(category) = category.filter(|c| c.is_capped()) {
                            decrement(&mut self.counts, category);
                        }
                    }
                    changes.push(SourceChange::Delete {
                        metadata: ElementMetadata {
                            reference: ElementReference {
                                source_id: Arc::from(source_id),
                                element_id: Arc::from(record.id.as_str()),
                            },
                            labels: Arc::from(
                                record
                                    .labels
                                    .into_iter()
                                    .map(|l| Arc::from(l.as_str()))
                                    .collect::<Vec<_>>(),
                            ),
                            effective_from: now_millis,
                        },
                    });
                }
            }
        }
        changes
    }

    fn schedule_ttl(&mut self, item: &ProjectedElement, ttl_secs: u64, received_at_millis: u64) {
        let expires_at = received_at_millis.saturating_add(ttl_secs.saturating_mul(1000));
        if let Some(previous) = self.ttl_by_id.insert(item.id.clone(), expires_at) {
            if let Some(bucket) = self.ttls.get_mut(&previous) {
                bucket.retain(|r| r.id != item.id);
                if bucket.is_empty() {
                    self.ttls.remove(&previous);
                }
            }
        }
        self.ttls.entry(expires_at).or_default().push(TtlRecord {
            id: item.id.clone(),
            labels: item.labels.clone(),
            expires_at,
            category: Some(item.category),
        });
    }

    fn take_ttl_record(&mut self, id: &str) -> Option<TtlRecord> {
        let expires_at = self.ttl_by_id.remove(id)?;
        let bucket = self.ttls.get_mut(&expires_at)?;
        let index = bucket.iter().position(|r| r.id == *id)?;
        let record = bucket.remove(index);
        if bucket.is_empty() {
            self.ttls.remove(&expires_at);
        }
        Some(record)
    }

    fn clear_ttl(&mut self, id: &str) {
        let _ = self.take_ttl_record(id);
    }

    pub fn to_bytes(&self) -> anyhow::Result<Vec<u8>> {
        Ok(serde_json::to_vec(self)?)
    }

    pub fn from_bytes(bytes: &[u8]) -> anyhow::Result<Self> {
        Ok(serde_json::from_slice(bytes)?)
    }
}

impl ProjectedElement {
    fn into_element(self, source_id: &str) -> Element {
        let metadata = ElementMetadata {
            reference: ElementReference {
                source_id: Arc::from(source_id),
                element_id: Arc::from(self.id.as_str()),
            },
            labels: Arc::from(
                self.labels
                    .iter()
                    .map(|l| Arc::from(l.as_str()))
                    .collect::<Vec<_>>(),
            ),
            effective_from: self.effective_from,
        };
        match self.kind {
            ProjectedKind::Node => Element::Node {
                metadata,
                properties: self.properties,
            },
            ProjectedKind::Relation { from, to } => Element::Relation {
                metadata,
                properties: self.properties,
                in_node: ElementReference {
                    source_id: Arc::from(source_id),
                    element_id: Arc::from(from.as_str()),
                },
                out_node: ElementReference {
                    source_id: Arc::from(source_id),
                    element_id: Arc::from(to.as_str()),
                },
            },
        }
    }
}

fn decrement(counts: &mut HashMap<ElementCategory, usize>, category: ElementCategory) {
    if let Some(value) = counts.get_mut(&category) {
        *value = value.saturating_sub(1);
    }
}

fn category_from_labels(labels: &[String]) -> Option<ElementCategory> {
    labels.iter().find_map(|label| match label.as_str() {
        "Service" => Some(ElementCategory::Service),
        "Metric" => Some(ElementCategory::Metric),
        "Heartbeat" => Some(ElementCategory::Heartbeat),
        "LogEvent" => Some(ElementCategory::LogEvent),
        "REPORTS" => Some(ElementCategory::Reports),
        "HEARTBEAT" => Some(ElementCategory::HeartbeatRel),
        "DEPENDS_ON" => Some(ElementCategory::DependsOn),
        "EMITS" => Some(ElementCategory::Emits),
        _ => None,
    })
}

fn delete_change(source_id: &str, record: &TtlRecord, now_millis: u64) -> SourceChange {
    SourceChange::Delete {
        metadata: ElementMetadata {
            reference: ElementReference {
                source_id: Arc::from(source_id),
                element_id: Arc::from(record.id.as_str()),
            },
            labels: Arc::from(
                record
                    .labels
                    .iter()
                    .map(|l| Arc::from(l.as_str()))
                    .collect::<Vec<_>>(),
            ),
            effective_from: now_millis,
        },
    }
}

fn stamp_registered_at(item: &mut ProjectedElement) {
    if item.properties.get("registeredAt").is_some() {
        return;
    }
    if let Some(last_seen) = item.properties.get("lastSeen").cloned() {
        item.properties.insert("registeredAt", last_seen);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_core::models::ElementPropertyMap;

    fn node(id: &str, category: ElementCategory, ttl: Option<u64>) -> ProjectedElement {
        ProjectedElement {
            id: id.to_string(),
            labels: vec!["Service".to_string()],
            properties: ElementPropertyMap::new(),
            kind: ProjectedKind::Node,
            effective_from: 1_000,
            category,
            ttl_secs: ttl,
            group: 0,
        }
    }

    #[test]
    fn first_observation_is_insert_second_is_update() {
        let mut state = LifecycleState::default();
        let config = OtelSourceConfig::default();
        let (first, dropped, _) = state.apply(
            "s",
            &config,
            vec![node("svc:a", ElementCategory::Service, None)],
            1_000,
        );
        assert_eq!(dropped, 0);
        assert!(matches!(first[0], SourceChange::Insert { .. }));
        let (second, _, _) = state.apply(
            "s",
            &config,
            vec![node("svc:a", ElementCategory::Service, None)],
            1_000,
        );
        assert!(matches!(second[0], SourceChange::Update { .. }));
    }

    #[test]
    fn cardinality_cap_drops_new_ids() {
        let mut state = LifecycleState::default();
        let config = OtelSourceConfig {
            max_services: 1,
            ..OtelSourceConfig::default()
        };
        let _ = state.apply(
            "s",
            &config,
            vec![node("svc:a", ElementCategory::Service, None)],
            1_000,
        );
        let (changes, dropped, _) = state.apply(
            "s",
            &config,
            vec![node("svc:b", ElementCategory::Service, None)],
            1_000,
        );
        assert!(changes.is_empty());
        assert_eq!(dropped, 1);
    }

    #[test]
    fn ttl_emits_delete_after_expiry() {
        let mut state = LifecycleState::default();
        let config = OtelSourceConfig::default();
        let mut item = node("dep:a:b", ElementCategory::DependsOn, Some(1));
        item.labels = vec!["DEPENDS_ON".to_string()];
        item.effective_from = 1;
        let _ = state.apply("s", &config, vec![item], 1_000);
        assert!(state.expire("s", 1_500).is_empty());
        let expired = state.expire("s", 2_100);
        assert_eq!(expired.len(), 1);
        assert!(matches!(expired[0], SourceChange::Delete { .. }));
    }

    #[test]
    fn late_event_time_does_not_expire_immediately() {
        let mut state = LifecycleState::default();
        let config = OtelSourceConfig::default();
        let mut item = node("dep:a:b", ElementCategory::DependsOn, Some(5));
        item.labels = vec!["DEPENDS_ON".to_string()];
        item.effective_from = 1;
        let received_at = 10_000;
        let _ = state.apply("s", &config, vec![item], received_at);
        assert!(state.expire("s", received_at + 1_000).is_empty());
        let expired = state.expire("s", received_at + 5_000);
        assert_eq!(expired.len(), 1);
    }

    #[test]
    fn group_is_dropped_together_at_cap() {
        let mut state = LifecycleState::default();
        let config = OtelSourceConfig {
            max_services: 1,
            ..OtelSourceConfig::default()
        };
        let _ = state.apply(
            "s",
            &config,
            vec![node("svc:a", ElementCategory::Service, None)],
            1_000,
        );
        let mut service = node("svc:b", ElementCategory::Service, None);
        let mut metric = node("metric:b", ElementCategory::Metric, None);
        service.group = 7;
        metric.group = 7;
        let (changes, dropped, _) = state.apply("s", &config, vec![service, metric], 1_000);
        assert!(changes.is_empty());
        assert_eq!(dropped, 1);
    }

    #[test]
    fn revert_apply_restores_seen_and_counts() {
        let mut state = LifecycleState::default();
        let config = OtelSourceConfig::default();
        let (_, _, undo) = state.apply(
            "s",
            &config,
            vec![node("svc:a", ElementCategory::Service, None)],
            1_000,
        );
        assert!(state.seen.contains("svc:a"));
        state.revert_apply(undo);
        assert!(!state.seen.contains("svc:a"));
        assert_eq!(
            state
                .counts
                .get(&ElementCategory::Service)
                .copied()
                .unwrap_or(0),
            0
        );
    }
}
