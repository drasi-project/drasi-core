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
    /// Apply projected elements, returning graph changes and how many were dropped.
    pub fn apply(
        &mut self,
        source_id: &str,
        config: &OtelSourceConfig,
        projected: Vec<ProjectedElement>,
    ) -> (Vec<SourceChange>, usize) {
        let mut changes = Vec::new();
        let mut dropped = 0usize;

        for item in projected {
            if !self.seen.contains(&item.id) {
                if let Some(cap) = item.category.cap(config) {
                    let current = self.counts.get(&item.category).copied().unwrap_or(0);
                    if current >= cap {
                        dropped += 1;
                        continue;
                    }
                }
            }

            let is_insert = self.seen.insert(item.id.clone());
            if is_insert {
                *self.counts.entry(item.category).or_insert(0) += 1;
            }

            if let Some(ttl_secs) = item.ttl_secs {
                self.schedule_ttl(&item, ttl_secs);
            }

            let element = item.into_element(source_id);
            if is_insert {
                changes.push(SourceChange::Insert { element });
            } else {
                changes.push(SourceChange::Update { element });
            }
        }

        (changes, dropped)
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
                        if record.labels.iter().any(|l| l == "DEPENDS_ON") {
                            decrement(&mut self.counts, ElementCategory::DependsOn);
                        } else if record.labels.iter().any(|l| l == "LogEvent") {
                            decrement(&mut self.counts, ElementCategory::LogEvent);
                        } else if record.labels.iter().any(|l| l == "EMITS") {
                            decrement(&mut self.counts, ElementCategory::Emits);
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

    fn schedule_ttl(&mut self, item: &ProjectedElement, ttl_secs: u64) {
        let expires_at = item
            .effective_from
            .saturating_add(ttl_secs.saturating_mul(1000));
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
        });
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
        }
    }

    #[test]
    fn first_observation_is_insert_second_is_update() {
        let mut state = LifecycleState::default();
        let config = OtelSourceConfig::default();
        let (first, dropped) = state.apply(
            "s",
            &config,
            vec![node("svc:a", ElementCategory::Service, None)],
        );
        assert_eq!(dropped, 0);
        assert!(matches!(first[0], SourceChange::Insert { .. }));
        let (second, _) = state.apply(
            "s",
            &config,
            vec![node("svc:a", ElementCategory::Service, None)],
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
        );
        let (changes, dropped) = state.apply(
            "s",
            &config,
            vec![node("svc:b", ElementCategory::Service, None)],
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
        item.effective_from = 1_000;
        let _ = state.apply("s", &config, vec![item]);
        assert!(state.expire("s", 1_500).is_empty());
        let expired = state.expire("s", 2_100);
        assert_eq!(expired.len(), 1);
        assert!(matches!(expired[0], SourceChange::Delete { .. }));
    }
}
