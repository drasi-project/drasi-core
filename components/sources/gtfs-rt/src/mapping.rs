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

use crate::config::GtfsRtFeedType;
use anyhow::Result;
use drasi_core::models::{
    validate_effective_from, Element, ElementMetadata, ElementReference, SourceChange,
};
use drasi_lib::sources::convert_json_to_element_properties;
use gtfs_realtime::{
    alert, trip_descriptor, trip_update, vehicle_descriptor, vehicle_position, Alert, FeedMessage,
    TimeRange, TranslatedString, TripDescriptor, TripUpdate, VehicleDescriptor, VehiclePosition,
};
use log::warn;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use xxhash_rust::xxh64::xxh64;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GraphItemKind {
    Node,
    Relation {
        in_node_id: String,
        out_node_id: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GraphItem {
    pub id: String,
    pub labels: Vec<String>,
    pub properties: BTreeMap<String, Value>,
    pub effective_from: u64,
    pub kind: GraphItemKind,
}

impl GraphItem {
    pub fn is_relation(&self) -> bool {
        matches!(self.kind, GraphItemKind::Relation { .. })
    }

    fn normalized_effective_from(&self) -> u64 {
        if let Err(err) = validate_effective_from(self.effective_from) {
            warn!(
                "Invalid effective_from '{}' for item '{}': {}",
                self.effective_from, self.id, err
            );
            0
        } else {
            self.effective_from
        }
    }

    pub fn metadata(&self, source_id: &str) -> ElementMetadata {
        ElementMetadata {
            reference: ElementReference::new(source_id, &self.id),
            labels: Arc::from(
                self.labels
                    .iter()
                    .map(|label| Arc::<str>::from(label.as_str()))
                    .collect::<Vec<_>>(),
            ),
            effective_from: self.normalized_effective_from(),
        }
    }

    pub fn to_element(&self, source_id: &str) -> Result<Element> {
        let metadata = self.metadata(source_id);
        let properties = self.to_element_properties()?;

        let element = match &self.kind {
            GraphItemKind::Node => Element::Node {
                metadata,
                properties,
            },
            GraphItemKind::Relation {
                in_node_id,
                out_node_id,
            } => Element::Relation {
                metadata,
                in_node: ElementReference::new(source_id, in_node_id),
                out_node: ElementReference::new(source_id, out_node_id),
                properties,
            },
        };

        Ok(element)
    }

    fn to_element_properties(&self) -> Result<drasi_core::models::ElementPropertyMap> {
        let json_properties = self
            .properties
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect::<serde_json::Map<String, Value>>();
        convert_json_to_element_properties(&json_properties)
    }

    fn hash(&self) -> u64 {
        match serde_json::to_vec(self) {
            Ok(bytes) => xxh64(&bytes, 0),
            Err(err) => {
                warn!(
                    "Failed to serialize GraphItem '{}' for hashing: {err}. \
                     Using fallback hash which may miss changes.",
                    self.id
                );
                xxh64(&[], 0)
            }
        }
    }

    fn merge_from(&mut self, newer: GraphItem) {
        match (&self.kind, &newer.kind) {
            (GraphItemKind::Node, GraphItemKind::Node) => {
                for (k, v) in newer.properties {
                    self.properties.insert(k, v);
                }
                if newer.effective_from > self.effective_from {
                    self.effective_from = newer.effective_from;
                }
                if self.labels.is_empty() {
                    self.labels = newer.labels;
                }
            }
            (
                GraphItemKind::Relation {
                    in_node_id: old_in,
                    out_node_id: old_out,
                },
                GraphItemKind::Relation {
                    in_node_id: new_in,
                    out_node_id: new_out,
                },
            ) if old_in == new_in && old_out == new_out => {
                for (k, v) in newer.properties {
                    self.properties.insert(k, v);
                }
                if newer.effective_from > self.effective_from {
                    self.effective_from = newer.effective_from;
                }
                if self.labels.is_empty() {
                    self.labels = newer.labels;
                }
            }
            _ => {
                // Kind mismatch or relation endpoints changed — replace entirely
                if !matches!(
                    (&self.kind, &newer.kind),
                    (GraphItemKind::Node, GraphItemKind::Node)
                ) {
                    warn!(
                        "Conflicting graph item for id '{}', replacing with latest item",
                        self.id
                    );
                }
                *self = newer;
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct HashedGraphItem {
    pub hash: u64,
    pub item: GraphItem,
}

#[derive(Debug, Clone, Default)]
pub struct GraphSnapshot {
    pub items: HashMap<String, HashedGraphItem>,
}

impl GraphSnapshot {
    pub fn from_items(items: HashMap<String, GraphItem>) -> Self {
        let items = items
            .into_iter()
            .map(|(id, item)| {
                let hash = item.hash();
                (id, HashedGraphItem { hash, item })
            })
            .collect();
        Self { items }
    }

    fn sorted_ids_for_inserts(&self) -> Vec<String> {
        let mut ids = self.items.keys().cloned().collect::<Vec<_>>();
        ids.sort_by(|a, b| {
            let a_rel = self
                .items
                .get(a)
                .map(|i| i.item.is_relation())
                .unwrap_or_default();
            let b_rel = self
                .items
                .get(b)
                .map(|i| i.item.is_relation())
                .unwrap_or_default();
            a_rel.cmp(&b_rel).then_with(|| a.cmp(b))
        });
        ids
    }
}

#[derive(Default)]
struct SnapshotBuilder {
    items: HashMap<String, GraphItem>,
}

impl SnapshotBuilder {
    fn add_item(&mut self, item: GraphItem) {
        if let Some(existing) = self.items.get_mut(&item.id) {
            existing.merge_from(item);
        } else {
            self.items.insert(item.id.clone(), item);
        }
    }

    fn add_node(
        &mut self,
        id: impl Into<String>,
        labels: Vec<String>,
        properties: BTreeMap<String, Value>,
        effective_from: u64,
    ) {
        self.add_item(GraphItem {
            id: id.into(),
            labels,
            properties,
            effective_from,
            kind: GraphItemKind::Node,
        });
    }

    fn add_relation(
        &mut self,
        id: impl Into<String>,
        labels: Vec<String>,
        properties: BTreeMap<String, Value>,
        in_node_id: impl Into<String>,
        out_node_id: impl Into<String>,
        effective_from: u64,
    ) {
        self.add_item(GraphItem {
            id: id.into(),
            labels,
            properties,
            effective_from,
            kind: GraphItemKind::Relation {
                in_node_id: in_node_id.into(),
                out_node_id: out_node_id.into(),
            },
        });
    }

    fn build(self) -> GraphSnapshot {
        GraphSnapshot::from_items(self.items)
    }
}

pub fn build_graph_snapshot(
    feeds: &[(GtfsRtFeedType, FeedMessage)],
    language: &str,
) -> GraphSnapshot {
    let mut builder = SnapshotBuilder::default();

    for (_feed_type, feed) in feeds {
        let feed_effective_from = seconds_to_millis(feed.header.timestamp);

        for entity in &feed.entity {
            if entity.is_deleted.unwrap_or(false) {
                continue;
            }

            let entity_id = entity.id.as_str();

            if let Some(trip_update) = &entity.trip_update {
                map_trip_update(entity_id, trip_update, feed_effective_from, &mut builder);
            }

            if let Some(vehicle_position) = &entity.vehicle {
                map_vehicle_position(
                    entity_id,
                    vehicle_position,
                    feed_effective_from,
                    &mut builder,
                );
            }

            if let Some(alert) = &entity.alert {
                map_alert(
                    entity_id,
                    alert,
                    feed_effective_from,
                    language,
                    &mut builder,
                );
            }
        }
    }

    builder.build()
}

pub fn snapshot_insert_changes(
    snapshot: &GraphSnapshot,
    source_id: &str,
) -> Result<Vec<SourceChange>> {
    let mut changes = Vec::new();

    for id in snapshot.sorted_ids_for_inserts() {
        if let Some(item) = snapshot.items.get(&id) {
            changes.push(SourceChange::Insert {
                element: item.item.to_element(source_id)?,
            });
        }
    }

    Ok(changes)
}

pub fn diff_graph_snapshots(
    previous: &GraphSnapshot,
    current: &GraphSnapshot,
    source_id: &str,
) -> Result<Vec<SourceChange>> {
    let mut insert_ids = Vec::new();
    let mut update_ids = Vec::new();
    let mut delete_ids = Vec::new();

    for (id, current_item) in &current.items {
        match previous.items.get(id) {
            None => insert_ids.push(id.clone()),
            Some(previous_item) if previous_item.hash != current_item.hash => {
                update_ids.push(id.clone())
            }
            _ => {}
        }
    }

    for id in previous.items.keys() {
        if !current.items.contains_key(id) {
            delete_ids.push(id.clone());
        }
    }

    sort_ids_for_insert_update(&mut insert_ids, current);
    sort_ids_for_insert_update(&mut update_ids, current);
    sort_ids_for_delete(&mut delete_ids, previous);

    let mut changes = Vec::new();

    for id in insert_ids {
        if let Some(item) = current.items.get(&id) {
            changes.push(SourceChange::Insert {
                element: item.item.to_element(source_id)?,
            });
        }
    }

    for id in update_ids {
        if let Some(item) = current.items.get(&id) {
            changes.push(SourceChange::Update {
                element: item.item.to_element(source_id)?,
            });
        }
    }

    for id in delete_ids {
        if let Some(item) = previous.items.get(&id) {
            changes.push(SourceChange::Delete {
                metadata: item.item.metadata(source_id),
            });
        }
    }

    Ok(changes)
}

fn sort_ids_for_insert_update(ids: &mut [String], snapshot: &GraphSnapshot) {
    ids.sort_by(|a, b| {
        let a_rel = snapshot
            .items
            .get(a)
            .map(|i| i.item.is_relation())
            .unwrap_or_default();
        let b_rel = snapshot
            .items
            .get(b)
            .map(|i| i.item.is_relation())
            .unwrap_or_default();
        a_rel.cmp(&b_rel).then_with(|| a.cmp(b))
    });
}

fn sort_ids_for_delete(ids: &mut [String], snapshot: &GraphSnapshot) {
    ids.sort_by(|a, b| {
        let a_rel = snapshot
            .items
            .get(a)
            .map(|i| i.item.is_relation())
            .unwrap_or_default();
        let b_rel = snapshot
            .items
            .get(b)
            .map(|i| i.item.is_relation())
            .unwrap_or_default();
        b_rel.cmp(&a_rel).then_with(|| a.cmp(b))
    });
}

fn map_trip_update(
    entity_id: &str,
    trip_update: &TripUpdate,
    feed_effective_from: u64,
    builder: &mut SnapshotBuilder,
) {
    let effective_from = max_effective_from(
        feed_effective_from,
        seconds_to_millis(trip_update.timestamp),
    );

    let trip_id = trip_update.trip.trip_id.clone();
    let route_id = trip_update.trip.route_id.clone();

    if let Some(route_id) = route_id.clone() {
        add_route_node(&route_id, effective_from, builder);
    }

    if let Some(trip_id) = trip_id.clone() {
        add_trip_node(&trip_id, &trip_update.trip, effective_from, builder);

        if let Some(route_id) = route_id {
            builder.add_relation(
                format!("rel:on_route:{trip_id}"),
                vec!["ON_ROUTE".to_string()],
                BTreeMap::new(),
                format!("trip:{trip_id}"),
                format!("route:{route_id}"),
                effective_from,
            );
        }
    }

    if let Some(vehicle) = &trip_update.vehicle {
        if let Some(vehicle_id) = vehicle_identity(vehicle) {
            add_vehicle_node(&vehicle_id, vehicle, effective_from, builder);

            if let Some(trip_id) = trip_id.clone() {
                builder.add_relation(
                    format!("rel:serves:{vehicle_id}:{trip_id}"),
                    vec!["SERVES".to_string()],
                    BTreeMap::new(),
                    format!("vehicle:{vehicle_id}"),
                    format!("trip:{trip_id}"),
                    effective_from,
                );
            }
        }
    }

    let tu_id = format!("tu:{entity_id}");
    let mut tu_props = BTreeMap::new();
    tu_props.insert(
        "entity_id".to_string(),
        Value::String(entity_id.to_string()),
    );
    if let Some(delay) = trip_update.delay {
        tu_props.insert("delay".to_string(), Value::Number(delay.into()));
    }
    if let Some(timestamp) = trip_update.timestamp {
        tu_props.insert(
            "timestamp".to_string(),
            Value::Number(seconds_to_millis(Some(timestamp)).into()),
        );
    }
    if let Some(trip_id) = trip_update.trip.trip_id.clone() {
        tu_props.insert("trip_id".to_string(), Value::String(trip_id));
    }
    if let Some(route_id) = trip_update.trip.route_id.clone() {
        tu_props.insert("route_id".to_string(), Value::String(route_id));
    }

    builder.add_node(
        tu_id.clone(),
        vec!["TripUpdate".to_string()],
        tu_props,
        effective_from,
    );

    if let Some(trip_id) = trip_id {
        builder.add_relation(
            format!("rel:has_update:{trip_id}:{entity_id}"),
            vec!["HAS_UPDATE".to_string()],
            BTreeMap::new(),
            format!("trip:{trip_id}"),
            tu_id.clone(),
            effective_from,
        );
    }

    for (idx, stop_time_update) in trip_update.stop_time_update.iter().enumerate() {
        map_stop_time_update(
            entity_id,
            idx,
            stop_time_update,
            &tu_id,
            effective_from,
            builder,
        );
    }
}

fn map_stop_time_update(
    entity_id: &str,
    index: usize,
    stop_time_update: &trip_update::StopTimeUpdate,
    trip_update_id: &str,
    effective_from: u64,
    builder: &mut SnapshotBuilder,
) {
    let key = stop_time_update
        .stop_sequence
        .map(|seq| seq.to_string())
        .unwrap_or_else(|| index.to_string());

    let stu_id = format!("stu:{entity_id}:{key}");
    let mut props = BTreeMap::new();

    if let Some(stop_sequence) = stop_time_update.stop_sequence {
        props.insert(
            "stop_sequence".to_string(),
            Value::Number((stop_sequence as u64).into()),
        );
    }

    if let Some(stop_id) = stop_time_update.stop_id.clone() {
        props.insert("stop_id".to_string(), Value::String(stop_id));
    }

    if let Some(schedule_relationship) = enum_name::<
        trip_update::stop_time_update::ScheduleRelationship,
    >(stop_time_update.schedule_relationship)
    {
        props.insert(
            "schedule_relationship".to_string(),
            Value::String(schedule_relationship),
        );
    }

    if let Some(departure_occupancy_status) =
        enum_name::<vehicle_position::OccupancyStatus>(stop_time_update.departure_occupancy_status)
    {
        props.insert(
            "departure_occupancy_status".to_string(),
            Value::String(departure_occupancy_status),
        );
    }

    if let Some(arrival) = stop_time_update.arrival {
        if let Some(delay) = arrival.delay {
            props.insert("arrival_delay".to_string(), Value::Number(delay.into()));
        }
        if let Some(time) = arrival.time {
            props.insert("arrival_time".to_string(), Value::Number(time.into()));
        }
        if let Some(uncertainty) = arrival.uncertainty {
            props.insert(
                "arrival_uncertainty".to_string(),
                Value::Number(uncertainty.into()),
            );
        }
    }

    if let Some(departure) = stop_time_update.departure {
        if let Some(delay) = departure.delay {
            props.insert("departure_delay".to_string(), Value::Number(delay.into()));
        }
        if let Some(time) = departure.time {
            props.insert("departure_time".to_string(), Value::Number(time.into()));
        }
        if let Some(uncertainty) = departure.uncertainty {
            props.insert(
                "departure_uncertainty".to_string(),
                Value::Number(uncertainty.into()),
            );
        }
    }

    builder.add_node(
        stu_id.clone(),
        vec!["StopTimeUpdate".to_string()],
        props,
        effective_from,
    );

    builder.add_relation(
        format!("rel:has_stu:{entity_id}:{key}"),
        vec!["HAS_STOP_TIME_UPDATE".to_string()],
        BTreeMap::new(),
        trip_update_id.to_string(),
        stu_id.clone(),
        effective_from,
    );

    if let Some(stop_id) = stop_time_update.stop_id.as_deref() {
        add_stop_node(stop_id, effective_from, builder);
        builder.add_relation(
            format!("rel:at_stop:{entity_id}:{key}"),
            vec!["AT_STOP".to_string()],
            BTreeMap::new(),
            stu_id,
            format!("stop:{stop_id}"),
            effective_from,
        );
    }
}

fn map_vehicle_position(
    entity_id: &str,
    vehicle_position: &VehiclePosition,
    feed_effective_from: u64,
    builder: &mut SnapshotBuilder,
) {
    let effective_from = max_effective_from(
        feed_effective_from,
        seconds_to_millis(vehicle_position.timestamp),
    );

    let vp_id = format!("vp:{entity_id}");
    let mut props = BTreeMap::new();
    props.insert(
        "entity_id".to_string(),
        Value::String(entity_id.to_string()),
    );

    if let Some(position) = vehicle_position.position {
        if let Some(lat) = serde_json::Number::from_f64(position.latitude as f64) {
            props.insert("latitude".to_string(), Value::Number(lat));
        } else {
            warn!(
                "Skipping invalid latitude {} for vehicle position entity '{entity_id}'",
                position.latitude
            );
        }
        if let Some(lon) = serde_json::Number::from_f64(position.longitude as f64) {
            props.insert("longitude".to_string(), Value::Number(lon));
        } else {
            warn!(
                "Skipping invalid longitude {} for vehicle position entity '{entity_id}'",
                position.longitude
            );
        }
        if let Some(bearing) = position.bearing {
            if let Some(number) = serde_json::Number::from_f64(bearing as f64) {
                props.insert("bearing".to_string(), Value::Number(number));
            }
        }
        if let Some(speed) = position.speed {
            if let Some(number) = serde_json::Number::from_f64(speed as f64) {
                props.insert("speed".to_string(), Value::Number(number));
            }
        }
    }

    if let Some(current_stop_sequence) = vehicle_position.current_stop_sequence {
        props.insert(
            "current_stop_sequence".to_string(),
            Value::Number((current_stop_sequence as u64).into()),
        );
    }

    if let Some(status) =
        enum_name::<vehicle_position::VehicleStopStatus>(vehicle_position.current_status)
    {
        props.insert("current_status".to_string(), Value::String(status));
    }

    if let Some(timestamp) = vehicle_position.timestamp {
        props.insert(
            "timestamp".to_string(),
            Value::Number(seconds_to_millis(Some(timestamp)).into()),
        );
    }

    if let Some(congestion_level) =
        enum_name::<vehicle_position::CongestionLevel>(vehicle_position.congestion_level)
    {
        props.insert(
            "congestion_level".to_string(),
            Value::String(congestion_level),
        );
    }

    if let Some(occupancy_status) =
        enum_name::<vehicle_position::OccupancyStatus>(vehicle_position.occupancy_status)
    {
        props.insert(
            "occupancy_status".to_string(),
            Value::String(occupancy_status),
        );
    }

    if let Some(occupancy_percentage) = vehicle_position.occupancy_percentage {
        props.insert(
            "occupancy_percentage".to_string(),
            Value::Number((occupancy_percentage as u64).into()),
        );
    }

    let trip_id = vehicle_position
        .trip
        .as_ref()
        .and_then(|trip| trip.trip_id.clone());
    let route_id = vehicle_position
        .trip
        .as_ref()
        .and_then(|trip| trip.route_id.clone());

    if let Some(trip_id) = trip_id.clone() {
        props.insert("trip_id".to_string(), Value::String(trip_id.clone()));
        if let Some(trip) = &vehicle_position.trip {
            add_trip_node(&trip_id, trip, effective_from, builder);
        }
    }

    if let Some(route_id) = route_id.clone() {
        props.insert("route_id".to_string(), Value::String(route_id.clone()));
        add_route_node(&route_id, effective_from, builder);
    }

    let vehicle_id = vehicle_position.vehicle.as_ref().and_then(vehicle_identity);

    if let Some(vehicle_id) = vehicle_id.clone() {
        props.insert("vehicle_id".to_string(), Value::String(vehicle_id.clone()));

        if let Some(vehicle) = &vehicle_position.vehicle {
            if let Some(label) = vehicle.label.clone() {
                props.insert("vehicle_label".to_string(), Value::String(label));
            }
            add_vehicle_node(&vehicle_id, vehicle, effective_from, builder);
        }
    }

    if let Some(stop_id) = vehicle_position.stop_id.clone() {
        props.insert("stop_id".to_string(), Value::String(stop_id.clone()));
    }

    builder.add_node(
        vp_id.clone(),
        vec!["VehiclePosition".to_string()],
        props,
        effective_from,
    );

    if let Some(vehicle_id) = vehicle_id.clone() {
        builder.add_relation(
            format!("rel:has_position:{vehicle_id}:{entity_id}"),
            vec!["HAS_POSITION".to_string()],
            BTreeMap::new(),
            format!("vehicle:{vehicle_id}"),
            vp_id.clone(),
            effective_from,
        );
    }

    if let (Some(vehicle_id), Some(trip_id)) = (vehicle_id, trip_id.clone()) {
        builder.add_relation(
            format!("rel:serves:{vehicle_id}:{trip_id}"),
            vec!["SERVES".to_string()],
            BTreeMap::new(),
            format!("vehicle:{vehicle_id}"),
            format!("trip:{trip_id}"),
            effective_from,
        );
    }

    if let (Some(trip_id), Some(route_id)) = (trip_id, route_id) {
        builder.add_relation(
            format!("rel:on_route:{trip_id}"),
            vec!["ON_ROUTE".to_string()],
            BTreeMap::new(),
            format!("trip:{trip_id}"),
            format!("route:{route_id}"),
            effective_from,
        );
    }

    if let Some(stop_id) = vehicle_position.stop_id.as_deref() {
        add_stop_node(stop_id, effective_from, builder);
        builder.add_relation(
            format!("rel:current_stop:{entity_id}"),
            vec!["CURRENT_STOP".to_string()],
            BTreeMap::new(),
            vp_id,
            format!("stop:{stop_id}"),
            effective_from,
        );
    }
}

fn map_alert(
    entity_id: &str,
    alert: &Alert,
    feed_effective_from: u64,
    language: &str,
    builder: &mut SnapshotBuilder,
) {
    let period_start = alert
        .active_period
        .iter()
        .filter_map(|period| period.start)
        .min();
    let period_end = alert
        .active_period
        .iter()
        .filter_map(|period| period.end)
        .max();

    let effective_from = max_effective_from(feed_effective_from, seconds_to_millis(period_start));
    let alert_id = format!("alert:{entity_id}");

    let mut props = BTreeMap::new();
    props.insert(
        "entity_id".to_string(),
        Value::String(entity_id.to_string()),
    );

    if let Some(cause) = enum_name::<alert::Cause>(alert.cause) {
        props.insert("cause".to_string(), Value::String(cause));
    }
    if let Some(effect) = enum_name::<alert::Effect>(alert.effect) {
        props.insert("effect".to_string(), Value::String(effect));
    }
    if let Some(severity_level) = enum_name::<alert::SeverityLevel>(alert.severity_level) {
        props.insert("severity_level".to_string(), Value::String(severity_level));
    }

    if let Some(header_text) = translated_text(alert.header_text.as_ref(), language) {
        props.insert("header_text".to_string(), Value::String(header_text));
    }
    if let Some(description_text) = translated_text(alert.description_text.as_ref(), language) {
        props.insert(
            "description_text".to_string(),
            Value::String(description_text),
        );
    }
    if let Some(url) = translated_text(alert.url.as_ref(), language) {
        props.insert("url".to_string(), Value::String(url));
    }

    if let Some(start) = period_start {
        props.insert(
            "active_period_start".to_string(),
            Value::Number(seconds_to_millis(Some(start)).into()),
        );
    }

    if let Some(end) = period_end {
        props.insert(
            "active_period_end".to_string(),
            Value::Number(seconds_to_millis(Some(end)).into()),
        );
    }

    builder.add_node(
        alert_id.clone(),
        vec!["Alert".to_string()],
        props,
        effective_from,
    );

    for (index, selector) in alert.informed_entity.iter().enumerate() {
        if let Some(agency_id) = selector.agency_id.as_deref() {
            add_agency_node(agency_id, effective_from, builder);
            builder.add_relation(
                format!("rel:affects_agency:{entity_id}:{index}"),
                vec!["AFFECTS_AGENCY".to_string()],
                BTreeMap::new(),
                alert_id.clone(),
                format!("agency:{agency_id}"),
                effective_from,
            );
        }

        let selector_route_id = selector.route_id.clone().or_else(|| {
            selector
                .trip
                .as_ref()
                .and_then(|trip| trip.route_id.clone())
        });

        if let Some(route_id) = selector_route_id {
            add_route_node(&route_id, effective_from, builder);
            builder.add_relation(
                format!("rel:affects_route:{entity_id}:{index}"),
                vec!["AFFECTS_ROUTE".to_string()],
                BTreeMap::new(),
                alert_id.clone(),
                format!("route:{route_id}"),
                effective_from,
            );
        }

        if let Some(stop_id) = selector.stop_id.as_deref() {
            add_stop_node(stop_id, effective_from, builder);
            builder.add_relation(
                format!("rel:affects_stop:{entity_id}:{index}"),
                vec!["AFFECTS_STOP".to_string()],
                BTreeMap::new(),
                alert_id.clone(),
                format!("stop:{stop_id}"),
                effective_from,
            );
        }

        if let Some(trip_descriptor) = selector.trip.as_ref() {
            if let Some(trip_id) = trip_descriptor.trip_id.as_deref() {
                add_trip_node(trip_id, trip_descriptor, effective_from, builder);
            }
        }
    }
}

fn add_route_node(route_id: &str, effective_from: u64, builder: &mut SnapshotBuilder) {
    let mut props = BTreeMap::new();
    props.insert("route_id".to_string(), Value::String(route_id.to_string()));
    builder.add_node(
        format!("route:{route_id}"),
        vec!["Route".to_string()],
        props,
        effective_from,
    );
}

fn add_trip_node(
    trip_id: &str,
    trip: &TripDescriptor,
    effective_from: u64,
    builder: &mut SnapshotBuilder,
) {
    let mut props = BTreeMap::new();
    props.insert("trip_id".to_string(), Value::String(trip_id.to_string()));

    if let Some(route_id) = trip.route_id.clone() {
        props.insert("route_id".to_string(), Value::String(route_id));
    }
    if let Some(direction_id) = trip.direction_id {
        props.insert(
            "direction_id".to_string(),
            Value::Number((direction_id as u64).into()),
        );
    }
    if let Some(start_time) = trip.start_time.clone() {
        props.insert("start_time".to_string(), Value::String(start_time));
    }
    if let Some(start_date) = trip.start_date.clone() {
        props.insert("start_date".to_string(), Value::String(start_date));
    }
    if let Some(schedule_relationship) =
        enum_name::<trip_descriptor::ScheduleRelationship>(trip.schedule_relationship)
    {
        props.insert(
            "schedule_relationship".to_string(),
            Value::String(schedule_relationship),
        );
    }

    builder.add_node(
        format!("trip:{trip_id}"),
        vec!["Trip".to_string()],
        props,
        effective_from,
    );
}

fn add_vehicle_node(
    vehicle_id: &str,
    vehicle: &VehicleDescriptor,
    effective_from: u64,
    builder: &mut SnapshotBuilder,
) {
    let mut props = BTreeMap::new();
    props.insert(
        "vehicle_id".to_string(),
        Value::String(vehicle_id.to_string()),
    );

    if let Some(label) = vehicle.label.clone() {
        props.insert("label".to_string(), Value::String(label));
    }
    if let Some(license_plate) = vehicle.license_plate.clone() {
        props.insert("license_plate".to_string(), Value::String(license_plate));
    }
    if let Some(wheelchair_accessible) =
        enum_name::<vehicle_descriptor::WheelchairAccessible>(vehicle.wheelchair_accessible)
    {
        props.insert(
            "wheelchair_accessible".to_string(),
            Value::String(wheelchair_accessible),
        );
    }

    builder.add_node(
        format!("vehicle:{vehicle_id}"),
        vec!["Vehicle".to_string()],
        props,
        effective_from,
    );
}

fn add_stop_node(stop_id: &str, effective_from: u64, builder: &mut SnapshotBuilder) {
    let mut props = BTreeMap::new();
    props.insert("stop_id".to_string(), Value::String(stop_id.to_string()));

    builder.add_node(
        format!("stop:{stop_id}"),
        vec!["Stop".to_string()],
        props,
        effective_from,
    );
}

fn add_agency_node(agency_id: &str, effective_from: u64, builder: &mut SnapshotBuilder) {
    let mut props = BTreeMap::new();
    props.insert(
        "agency_id".to_string(),
        Value::String(agency_id.to_string()),
    );

    builder.add_node(
        format!("agency:{agency_id}"),
        vec!["Agency".to_string()],
        props,
        effective_from,
    );
}

fn translated_text(value: Option<&TranslatedString>, language: &str) -> Option<String> {
    let value = value?;

    let lang = language.to_ascii_lowercase();
    if let Some(entry) = value.translation.iter().find(|entry| {
        entry
            .language
            .as_ref()
            .map(|l| l.eq_ignore_ascii_case(&lang))
            .unwrap_or(false)
    }) {
        return Some(entry.text.clone());
    }

    if !lang.eq_ignore_ascii_case("en") {
        if let Some(entry) = value.translation.iter().find(|entry| {
            entry
                .language
                .as_ref()
                .map(|l| l.eq_ignore_ascii_case("en"))
                .unwrap_or(false)
        }) {
            return Some(entry.text.clone());
        }
    }

    if let Some(entry) = value
        .translation
        .iter()
        .find(|entry| entry.language.as_ref().is_none())
    {
        return Some(entry.text.clone());
    }

    value.translation.first().map(|entry| entry.text.clone())
}

fn vehicle_identity(vehicle: &VehicleDescriptor) -> Option<String> {
    vehicle
        .id
        .as_ref()
        .cloned()
        .or_else(|| vehicle.label.as_ref().cloned())
}

fn enum_name<E>(value: Option<i32>) -> Option<String>
where
    E: TryFrom<i32>,
    <E as TryFrom<i32>>::Error: std::fmt::Debug,
    E: EnumName,
{
    value
        .and_then(|v| E::try_from(v).ok())
        .map(|v| v.as_str_name().to_string())
}

trait EnumName {
    fn as_str_name(&self) -> &'static str;
}

impl EnumName for alert::Cause {
    fn as_str_name(&self) -> &'static str {
        alert::Cause::as_str_name(self)
    }
}

impl EnumName for alert::Effect {
    fn as_str_name(&self) -> &'static str {
        alert::Effect::as_str_name(self)
    }
}

impl EnumName for alert::SeverityLevel {
    fn as_str_name(&self) -> &'static str {
        alert::SeverityLevel::as_str_name(self)
    }
}

impl EnumName for trip_descriptor::ScheduleRelationship {
    fn as_str_name(&self) -> &'static str {
        trip_descriptor::ScheduleRelationship::as_str_name(self)
    }
}

impl EnumName for trip_update::stop_time_update::ScheduleRelationship {
    fn as_str_name(&self) -> &'static str {
        trip_update::stop_time_update::ScheduleRelationship::as_str_name(self)
    }
}

impl EnumName for vehicle_descriptor::WheelchairAccessible {
    fn as_str_name(&self) -> &'static str {
        vehicle_descriptor::WheelchairAccessible::as_str_name(self)
    }
}

impl EnumName for vehicle_position::VehicleStopStatus {
    fn as_str_name(&self) -> &'static str {
        vehicle_position::VehicleStopStatus::as_str_name(self)
    }
}

impl EnumName for vehicle_position::CongestionLevel {
    fn as_str_name(&self) -> &'static str {
        vehicle_position::CongestionLevel::as_str_name(self)
    }
}

impl EnumName for vehicle_position::OccupancyStatus {
    fn as_str_name(&self) -> &'static str {
        vehicle_position::OccupancyStatus::as_str_name(self)
    }
}

fn seconds_to_millis(seconds: Option<u64>) -> u64 {
    seconds.unwrap_or(0).saturating_mul(1000)
}

fn max_effective_from(primary: u64, secondary: u64) -> u64 {
    if secondary == 0 {
        primary
    } else {
        primary.max(secondary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gtfs_realtime::{
        FeedEntity, FeedHeader, FeedMessage, Position, TimeRange, TranslatedString, TripDescriptor,
        TripUpdate, VehicleDescriptor, VehiclePosition,
    };

    fn test_feed_header() -> FeedHeader {
        FeedHeader {
            gtfs_realtime_version: "2.0".to_string(),
            incrementality: None,
            timestamp: Some(1_720_000_000),
            feed_version: None,
        }
    }

    #[test]
    fn test_trip_update_mapping_creates_nodes_and_relations() {
        let trip_update = TripUpdate {
            trip: TripDescriptor {
                trip_id: Some("trip-1".to_string()),
                route_id: Some("route-7".to_string()),
                direction_id: Some(1),
                start_time: Some("08:00:00".to_string()),
                start_date: Some("20250101".to_string()),
                schedule_relationship: None,
                modified_trip: None,
            },
            vehicle: Some(VehicleDescriptor {
                id: Some("veh-1".to_string()),
                label: Some("Vehicle 1".to_string()),
                license_plate: None,
                wheelchair_accessible: None,
            }),
            stop_time_update: vec![trip_update::StopTimeUpdate {
                stop_sequence: Some(4),
                stop_id: Some("stop-22".to_string()),
                arrival: Some(trip_update::StopTimeEvent {
                    delay: Some(120),
                    time: Some(1_720_000_100),
                    uncertainty: Some(20),
                    scheduled_time: None,
                }),
                departure: None,
                departure_occupancy_status: None,
                schedule_relationship: None,
                stop_time_properties: None,
            }],
            timestamp: Some(1_720_000_050),
            delay: Some(300),
            trip_properties: None,
        };

        let feed = FeedMessage {
            header: test_feed_header(),
            entity: vec![FeedEntity {
                id: "entity-tu-1".to_string(),
                is_deleted: Some(false),
                trip_update: Some(trip_update),
                vehicle: None,
                alert: None,
                shape: None,
                stop: None,
                trip_modifications: None,
            }],
        };

        let snapshot = build_graph_snapshot(&[(GtfsRtFeedType::TripUpdates, feed)], "en");

        assert!(snapshot.items.contains_key("route:route-7"));
        assert!(snapshot.items.contains_key("trip:trip-1"));
        assert!(snapshot.items.contains_key("vehicle:veh-1"));
        assert!(snapshot.items.contains_key("tu:entity-tu-1"));
        assert!(snapshot.items.contains_key("stu:entity-tu-1:4"));
        assert!(snapshot
            .items
            .contains_key("rel:has_update:trip-1:entity-tu-1"));
        assert!(snapshot.items.contains_key("rel:on_route:trip-1"));
        assert!(snapshot.items.contains_key("rel:has_stu:entity-tu-1:4"));
        assert!(snapshot.items.contains_key("rel:at_stop:entity-tu-1:4"));
    }

    #[test]
    fn test_vehicle_position_mapping_creates_position_graph() {
        let feed = FeedMessage {
            header: test_feed_header(),
            entity: vec![FeedEntity {
                id: "entity-vp-1".to_string(),
                is_deleted: Some(false),
                trip_update: None,
                vehicle: Some(VehiclePosition {
                    trip: Some(TripDescriptor {
                        trip_id: Some("trip-2".to_string()),
                        route_id: Some("route-2".to_string()),
                        direction_id: None,
                        start_time: None,
                        start_date: None,
                        schedule_relationship: None,
                        modified_trip: None,
                    }),
                    vehicle: Some(VehicleDescriptor {
                        id: Some("veh-2".to_string()),
                        label: Some("Vehicle 2".to_string()),
                        license_plate: None,
                        wheelchair_accessible: None,
                    }),
                    position: Some(Position {
                        latitude: 39.7,
                        longitude: -104.9,
                        bearing: Some(90.0),
                        odometer: None,
                        speed: Some(12.3),
                    }),
                    current_stop_sequence: Some(10),
                    stop_id: Some("stop-10".to_string()),
                    current_status: None,
                    timestamp: Some(1_720_000_200),
                    congestion_level: None,
                    occupancy_status: None,
                    occupancy_percentage: Some(45),
                    multi_carriage_details: vec![],
                }),
                alert: None,
                shape: None,
                stop: None,
                trip_modifications: None,
            }],
        };

        let snapshot = build_graph_snapshot(&[(GtfsRtFeedType::VehiclePositions, feed)], "en");

        assert!(snapshot.items.contains_key("vehicle:veh-2"));
        assert!(snapshot.items.contains_key("trip:trip-2"));
        assert!(snapshot.items.contains_key("route:route-2"));
        assert!(snapshot.items.contains_key("vp:entity-vp-1"));
        assert!(snapshot
            .items
            .contains_key("rel:has_position:veh-2:entity-vp-1"));
        assert!(snapshot.items.contains_key("rel:current_stop:entity-vp-1"));
    }

    #[test]
    fn test_alert_mapping_creates_alert_graph() {
        let feed = FeedMessage {
            header: test_feed_header(),
            entity: vec![FeedEntity {
                id: "entity-alert-1".to_string(),
                is_deleted: Some(false),
                trip_update: None,
                vehicle: None,
                alert: Some(Alert {
                    active_period: vec![TimeRange {
                        start: Some(1_720_000_000),
                        end: Some(1_720_003_600),
                    }],
                    informed_entity: vec![gtfs_realtime::EntitySelector {
                        agency_id: Some("agency-1".to_string()),
                        route_id: Some("route-9".to_string()),
                        route_type: None,
                        trip: None,
                        stop_id: Some("stop-33".to_string()),
                        direction_id: None,
                    }],
                    cause: Some(alert::Cause::Weather as i32),
                    effect: Some(alert::Effect::Detour as i32),
                    url: None,
                    header_text: Some(TranslatedString {
                        translation: vec![gtfs_realtime::translated_string::Translation {
                            text: "Storm detour".to_string(),
                            language: Some("en".to_string()),
                        }],
                    }),
                    description_text: None,
                    tts_header_text: None,
                    tts_description_text: None,
                    severity_level: Some(alert::SeverityLevel::Severe as i32),
                    image: None,
                    image_alternative_text: None,
                    cause_detail: None,
                    effect_detail: None,
                }),
                shape: None,
                stop: None,
                trip_modifications: None,
            }],
        };

        let snapshot = build_graph_snapshot(&[(GtfsRtFeedType::Alerts, feed)], "en");

        assert!(snapshot.items.contains_key("alert:entity-alert-1"));
        assert!(snapshot.items.contains_key("route:route-9"));
        assert!(snapshot.items.contains_key("stop:stop-33"));
        assert!(snapshot.items.contains_key("agency:agency-1"));
        assert!(snapshot
            .items
            .contains_key("rel:affects_route:entity-alert-1:0"));
        assert!(snapshot
            .items
            .contains_key("rel:affects_stop:entity-alert-1:0"));
        assert!(snapshot
            .items
            .contains_key("rel:affects_agency:entity-alert-1:0"));
    }

    #[test]
    fn test_diff_graph_snapshot_insert_update_delete() {
        let mut old_items = HashMap::new();
        old_items.insert(
            "node:a".to_string(),
            GraphItem {
                id: "node:a".to_string(),
                labels: vec!["A".to_string()],
                properties: BTreeMap::from([("value".to_string(), Value::Number(1.into()))]),
                effective_from: 1000,
                kind: GraphItemKind::Node,
            },
        );

        let mut new_items = HashMap::new();
        new_items.insert(
            "node:a".to_string(),
            GraphItem {
                id: "node:a".to_string(),
                labels: vec!["A".to_string()],
                properties: BTreeMap::from([("value".to_string(), Value::Number(2.into()))]),
                effective_from: 2000,
                kind: GraphItemKind::Node,
            },
        );
        new_items.insert(
            "node:b".to_string(),
            GraphItem {
                id: "node:b".to_string(),
                labels: vec!["B".to_string()],
                properties: BTreeMap::new(),
                effective_from: 2000,
                kind: GraphItemKind::Node,
            },
        );

        let old_snapshot = GraphSnapshot::from_items(old_items);
        let new_snapshot = GraphSnapshot::from_items(new_items);

        let changes = diff_graph_snapshots(&old_snapshot, &new_snapshot, "source-1").unwrap();
        assert_eq!(changes.len(), 2);

        assert!(changes
            .iter()
            .any(|change| matches!(change, SourceChange::Insert { .. })));
        assert!(changes
            .iter()
            .any(|change| matches!(change, SourceChange::Update { .. })));
    }
}
