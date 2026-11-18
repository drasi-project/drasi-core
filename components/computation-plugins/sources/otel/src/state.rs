// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The source's projection state: which element IDs are live, how many of
//! each capped category exist, and when each time-limited element expires.
//!
//! The state decides whether a projected element is an insert or an update,
//! enforces the cardinality caps per group, and produces the deletes for
//! expired elements. It is volatile: it lives in the component object, so it
//! survives Stop and Start of that object but not reconstruction.

use crate::config::Projection;
use drasi_core_types::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

/// The category of a projected element, used for cardinality caps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
    /// The live-element cap for capped categories.
    fn cap(self, settings: &Projection) -> Option<usize> {
        match self {
            Self::Service => Some(settings.max_services),
            Self::Metric => Some(settings.max_metrics),
            Self::DependsOn => Some(settings.max_dependencies),
            Self::LogEvent => Some(settings.max_log_events),
            Self::Heartbeat | Self::Reports | Self::HeartbeatRel | Self::Emits => None,
        }
    }
}

/// A projected graph element before insert-or-update is decided.
#[derive(Debug, Clone)]
pub struct ProjectedElement {
    pub id: String,
    pub labels: Vec<String>,
    pub properties: ElementPropertyMap,
    pub kind: ProjectedKind,
    pub effective_from: u64,
    pub category: ElementCategory,
    /// Lifetime from receipt; `None` lives until reconstruction.
    pub ttl_secs: Option<u64>,
    /// Elements sharing a group are admitted or dropped together.
    pub group: u32,
}

/// Node or relation payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectedKind {
    Node,
    Relation { from: String, to: String },
}

/// A scheduled expiry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TtlRecord {
    pub id: String,
    pub labels: Vec<String>,
    pub expires_at: u64,
}

/// The state changes one admitted group made, so they can be undone if the
/// group's changes are then refused by admission.
#[derive(Debug, Default)]
pub struct GroupUndo {
    inserted: Vec<(String, ElementCategory)>,
    ttl_previous: Vec<(String, Option<TtlRecord>)>,
}

/// The result of applying one group.
#[derive(Debug)]
pub enum GroupOutcome {
    /// A new element would exceed its category's cap; nothing changed.
    Dropped,
    /// The group's changes, and how to undo the state they recorded.
    Admitted(Vec<SourceChange>, GroupUndo),
}

/// Live element IDs, category counts and the TTL schedule.
#[derive(Debug, Default)]
pub struct ProjectionState {
    seen: HashMap<String, ElementCategory>,
    counts: HashMap<ElementCategory, usize>,
    ttl_by_id: HashMap<String, u64>,
    ttls: BTreeMap<u64, Vec<TtlRecord>>,
}

/// Splits projected elements into their groups, in first-appearance order.
pub fn groups(elements: Vec<ProjectedElement>) -> Vec<Vec<ProjectedElement>> {
    let mut order: Vec<u32> = Vec::new();
    let mut by_group: HashMap<u32, Vec<ProjectedElement>> = HashMap::new();
    for element in elements {
        by_group
            .entry(element.group)
            .or_insert_with(|| {
                order.push(element.group);
                Vec::new()
            })
            .push(element);
    }
    order
        .into_iter()
        .filter_map(|group| by_group.remove(&group))
        .collect()
}

impl ProjectionState {
    /// Number of live elements.
    pub fn len(&self) -> usize {
        self.seen.len()
    }

    /// Whether no element is live.
    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }

    /// Whether `id` is live.
    pub fn contains(&self, id: &str) -> bool {
        self.seen.contains_key(id)
    }

    /// Live elements in a category.
    pub fn count(&self, category: ElementCategory) -> usize {
        self.counts.get(&category).copied().unwrap_or(0)
    }

    /// Applies one group: a first observation is an insert, a later one an
    /// update that sets the projected properties (so a Service keeps the
    /// `registeredAt` stamped on its insert). TTLs are scheduled from receipt
    /// time, never from the OTLP event time, so a late batch cannot expire
    /// immediately.
    pub fn apply_group(
        &mut self,
        namespace: &str,
        settings: &Projection,
        group: Vec<ProjectedElement>,
        received_at_millis: u64,
    ) -> GroupOutcome {
        if self.exceeds_caps(settings, &group) {
            return GroupOutcome::Dropped;
        }
        let mut undo = GroupUndo::default();
        let mut changes = Vec::with_capacity(group.len());
        for mut item in group {
            let inserted = !self.seen.contains_key(&item.id);
            if inserted {
                self.seen.insert(item.id.clone(), item.category);
                *self.counts.entry(item.category).or_insert(0) += 1;
                undo.inserted.push((item.id.clone(), item.category));
                if item.category == ElementCategory::Service {
                    stamp_registered_at(&mut item);
                }
            }
            // A first observation also clears an expiry left for an element
            // that is not live, such as a redelivered delete after Stop.
            if inserted || item.ttl_secs.is_some() {
                let previous = self.take_ttl(&item.id);
                if previous.is_some() || item.ttl_secs.is_some() {
                    undo.ttl_previous.push((item.id.clone(), previous));
                }
            }
            if let Some(ttl_secs) = item.ttl_secs {
                self.schedule_ttl(&item, ttl_secs, received_at_millis);
            }
            let element = item.into_element(namespace);
            changes.push(if inserted {
                SourceChange::Insert { element }
            } else {
                SourceChange::update(element)
            });
        }
        GroupOutcome::Admitted(changes, undo)
    }

    /// Restores the state from before [`Self::apply_group`].
    pub fn undo(&mut self, undo: GroupUndo) {
        for (id, previous) in undo.ttl_previous.into_iter().rev() {
            self.take_ttl(&id);
            if let Some(record) = previous {
                self.ttl_by_id.insert(record.id.clone(), record.expires_at);
                self.ttls.entry(record.expires_at).or_default().push(record);
            }
        }
        for (id, category) in undo.inserted {
            if self.seen.remove(&id).is_some() {
                decrement(&mut self.counts, category);
            }
        }
    }

    /// Reconciles the state with changes that were admitted but discarded
    /// undelivered by Stop, given oldest first. The graph's view of an
    /// element is decided by its first discarded change: after a discarded
    /// insert the graph never had it, so a still-live element is forgotten
    /// and its next observation is inserted again; after a discarded update
    /// or delete the graph still has it, so an element the state already
    /// forgot gets a delete scheduled for the next sweep. Returns the number
    /// of elements reconciled.
    pub fn reconcile_discarded(&mut self, discarded: &[SourceChange]) -> usize {
        let mut order: Vec<&str> = Vec::new();
        let mut first: HashMap<&str, (bool, &ElementMetadata)> = HashMap::new();
        for change in discarded {
            let (in_graph, metadata) = match change {
                SourceChange::Insert { element } => (false, element.get_metadata()),
                SourceChange::Update { element, .. } | SourceChange::Replace { element } => {
                    (true, element.get_metadata())
                }
                SourceChange::Delete { metadata } => (true, metadata),
                SourceChange::Future { .. } => continue,
            };
            let id = metadata.reference.element_id.as_ref();
            first.entry(id).or_insert_with(|| {
                order.push(id);
                (in_graph, metadata)
            });
        }
        let mut reconciled = 0;
        for id in order {
            let (in_graph, metadata) = first[id];
            match (in_graph, self.seen.get(id).copied()) {
                (false, Some(category)) => {
                    self.seen.remove(id);
                    decrement(&mut self.counts, category);
                    self.take_ttl(id);
                    reconciled += 1;
                }
                (true, None) => {
                    self.take_ttl(id);
                    self.ttl_by_id.insert(id.to_string(), 0);
                    self.ttls.entry(0).or_default().push(TtlRecord {
                        id: id.to_string(),
                        labels: metadata.labels.iter().map(|l| l.to_string()).collect(),
                        expires_at: 0,
                    });
                    reconciled += 1;
                }
                _ => {}
            }
        }
        reconciled
    }

    fn exceeds_caps(&self, settings: &Projection, group: &[ProjectedElement]) -> bool {
        let mut extra: HashMap<ElementCategory, usize> = HashMap::new();
        let mut new_ids = HashSet::new();
        for item in group {
            if self.seen.contains_key(&item.id) || !new_ids.insert(item.id.as_str()) {
                continue;
            }
            let Some(cap) = item.category.cap(settings) else {
                continue;
            };
            let pending = extra.entry(item.category).or_insert(0);
            if self.count(item.category) + *pending >= cap {
                return true;
            }
            *pending += 1;
        }
        false
    }

    /// The scheduled expiries due at `now_millis`, earliest first.
    pub fn due(&self, now_millis: u64) -> Vec<TtlRecord> {
        self.ttls
            .range(..=now_millis)
            .flat_map(|(expires, records)| {
                records
                    .iter()
                    .filter(move |record| self.ttl_by_id.get(&record.id) == Some(expires))
            })
            .cloned()
            .collect()
    }

    /// Forgets expired elements whose deletes were admitted. A record that was
    /// refreshed in the meantime is kept.
    pub fn expire(&mut self, records: &[TtlRecord]) {
        for record in records {
            if self.ttl_by_id.get(&record.id) != Some(&record.expires_at) {
                continue;
            }
            self.take_ttl(&record.id);
            if let Some(category) = self.seen.remove(&record.id) {
                decrement(&mut self.counts, category);
            }
        }
    }

    fn schedule_ttl(&mut self, item: &ProjectedElement, ttl_secs: u64, received_at_millis: u64) {
        let expires_at = received_at_millis.saturating_add(ttl_secs.saturating_mul(1000));
        self.ttl_by_id.insert(item.id.clone(), expires_at);
        self.ttls.entry(expires_at).or_default().push(TtlRecord {
            id: item.id.clone(),
            labels: item.labels.clone(),
            expires_at,
        });
    }

    fn take_ttl(&mut self, id: &str) -> Option<TtlRecord> {
        let expires_at = self.ttl_by_id.remove(id)?;
        let bucket = self.ttls.get_mut(&expires_at)?;
        let index = bucket.iter().position(|record| record.id == id)?;
        let record = bucket.remove(index);
        if bucket.is_empty() {
            self.ttls.remove(&expires_at);
        }
        Some(record)
    }
}

/// The delete for an expired element, effective at `now_millis`.
pub fn delete_change(namespace: &str, record: &TtlRecord, now_millis: u64) -> SourceChange {
    SourceChange::Delete {
        metadata: ElementMetadata {
            reference: ElementReference::new(namespace, &record.id),
            labels: labels(&record.labels),
            effective_from: now_millis,
        },
    }
}

impl ProjectedElement {
    fn into_element(self, namespace: &str) -> Element {
        let metadata = ElementMetadata {
            reference: ElementReference::new(namespace, &self.id),
            labels: labels(&self.labels),
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
                in_node: ElementReference::new(namespace, &from),
                out_node: ElementReference::new(namespace, &to),
            },
        }
    }
}

fn labels(labels: &[String]) -> Arc<[Arc<str>]> {
    labels
        .iter()
        .map(|label| Arc::from(label.as_str()))
        .collect()
}

fn decrement(counts: &mut HashMap<ElementCategory, usize>, category: ElementCategory) {
    if let Some(count) = counts.get_mut(&category) {
        *count = count.saturating_sub(1);
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
#[path = "state_tests.rs"]
mod tests;
