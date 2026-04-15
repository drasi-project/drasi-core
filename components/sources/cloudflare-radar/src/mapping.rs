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

use crate::api::{
    AttackLayer3SummaryData, AttackSummaryData, BgpHijackEvent, BgpLeakEvent, DnsLocationEntry,
    DomainRank, HttpSummaryData, OutageAnnotation,
};
use chrono::{DateTime, Utc};
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementTimestamp, ElementValue,
    SourceChange,
};
use std::sync::Arc;

#[derive(Debug, Clone, Copy)]
pub enum ChangeAction {
    Insert,
    Update,
    Delete,
}

fn labels(labels: &[&str]) -> Arc<[Arc<str>]> {
    labels.iter().map(|label| Arc::from(*label)).collect()
}

fn element_reference(source_id: &str, element_id: &str) -> ElementReference {
    ElementReference::new(source_id, element_id)
}

fn node_metadata(
    source_id: &str,
    element_id: &str,
    label_names: &[&str],
    effective_from: u64,
) -> ElementMetadata {
    ElementMetadata {
        reference: element_reference(source_id, element_id),
        labels: labels(label_names),
        effective_from,
    }
}

fn relation_metadata(
    source_id: &str,
    element_id: &str,
    label_names: &[&str],
    effective_from: u64,
) -> ElementMetadata {
    ElementMetadata {
        reference: element_reference(source_id, element_id),
        labels: labels(label_names),
        effective_from,
    }
}

fn insert_json(props: &mut ElementPropertyMap, key: &str, value: serde_json::Value) {
    props.insert(key, ElementValue::from(&value));
}

fn insert_if_some<T: Into<serde_json::Value>>(
    props: &mut ElementPropertyMap,
    key: &str,
    value: Option<T>,
) {
    if let Some(val) = value {
        insert_json(props, key, val.into());
    }
}

fn timestamp_millis_from_rfc3339(input: &Option<String>) -> Option<ElementTimestamp> {
    let value = input.as_ref()?;
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|dt| dt.timestamp_millis() as u64)
}

fn now_millis() -> u64 {
    Utc::now().timestamp_millis() as u64
}

pub fn normalize_id(input: &str) -> String {
    input
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect::<String>()
}

fn build_node(
    source_id: &str,
    element_id: &str,
    label: &str,
    properties: ElementPropertyMap,
    effective_from: u64,
    action: ChangeAction,
) -> SourceChange {
    let metadata = node_metadata(source_id, element_id, &[label], effective_from);
    match action {
        ChangeAction::Insert => SourceChange::Insert {
            element: Element::Node {
                metadata,
                properties,
            },
        },
        ChangeAction::Update => SourceChange::Update {
            element: Element::Node {
                metadata,
                properties,
            },
        },
        ChangeAction::Delete => SourceChange::Delete { metadata },
    }
}

#[allow(clippy::too_many_arguments)]
fn build_relation(
    source_id: &str,
    element_id: &str,
    label: &str,
    from_id: &str,
    to_id: &str,
    properties: ElementPropertyMap,
    effective_from: u64,
    action: ChangeAction,
) -> SourceChange {
    let metadata = relation_metadata(source_id, element_id, &[label], effective_from);
    match action {
        ChangeAction::Insert => SourceChange::Insert {
            element: Element::Relation {
                metadata,
                in_node: element_reference(source_id, from_id),
                out_node: element_reference(source_id, to_id),
                properties,
            },
        },
        ChangeAction::Update => SourceChange::Update {
            element: Element::Relation {
                metadata,
                in_node: element_reference(source_id, from_id),
                out_node: element_reference(source_id, to_id),
                properties,
            },
        },
        ChangeAction::Delete => SourceChange::Delete { metadata },
    }
}

pub fn map_outage(
    source_id: &str,
    outage_id: &str,
    outage: &OutageAnnotation,
    action: ChangeAction,
    include_shared: bool,
) -> Vec<SourceChange> {
    let mut changes = Vec::new();
    let effective_from =
        timestamp_millis_from_rfc3339(&outage.start_date).unwrap_or_else(now_millis);

    let mut props = ElementPropertyMap::new();
    insert_if_some(&mut props, "dataSource", outage.data_source.clone());
    insert_if_some(&mut props, "description", outage.description.clone());
    insert_if_some(&mut props, "scope", outage.scope.clone());
    insert_if_some(&mut props, "startDate", outage.start_date.clone());
    insert_if_some(&mut props, "endDate", outage.end_date.clone());
    insert_if_some(&mut props, "eventType", outage.event_type.clone());
    insert_if_some(&mut props, "linkedUrl", outage.linked_url.clone());
    if let Some(ref details) = outage.outage {
        insert_if_some(&mut props, "outageCause", details.outage_cause.clone());
        insert_if_some(&mut props, "outageType", details.outage_type.clone());
    }

    changes.push(build_node(
        source_id,
        outage_id,
        "Outage",
        props,
        effective_from,
        action,
    ));

    if include_shared && matches!(action, ChangeAction::Insert) {
        for location in &outage.locations {
            let location_id = format!("location-{}", normalize_id(location));
            let mut location_props = ElementPropertyMap::new();
            insert_json(
                &mut location_props,
                "code",
                serde_json::Value::String(location.clone()),
            );
            changes.push(build_node(
                source_id,
                &location_id,
                "Location",
                location_props,
                effective_from,
                ChangeAction::Insert,
            ));
            changes.push(build_relation(
                source_id,
                &format!("{outage_id}-affects-{location_id}"),
                "AFFECTS_LOCATION",
                outage_id,
                &location_id,
                ElementPropertyMap::new(),
                effective_from,
                ChangeAction::Insert,
            ));
        }

        for asn in &outage.asns {
            let asn_id = format!("as-{asn}");
            let mut asn_props = ElementPropertyMap::new();
            insert_json(
                &mut asn_props,
                "asn",
                serde_json::Value::Number((*asn).into()),
            );
            changes.push(build_node(
                source_id,
                &asn_id,
                "AutonomousSystem",
                asn_props,
                effective_from,
                ChangeAction::Insert,
            ));
            changes.push(build_relation(
                source_id,
                &format!("{outage_id}-affects-{asn_id}"),
                "AFFECTS_ASN",
                outage_id,
                &asn_id,
                ElementPropertyMap::new(),
                effective_from,
                ChangeAction::Insert,
            ));
        }
    }

    changes
}

pub fn map_hijack(
    source_id: &str,
    hijack: &BgpHijackEvent,
    action: ChangeAction,
    include_shared: bool,
) -> Vec<SourceChange> {
    let mut changes = Vec::new();
    let hijack_id = format!("bgp-hijack-{}", hijack.id);
    let effective_from =
        timestamp_millis_from_rfc3339(&hijack.min_hijack_ts).unwrap_or_else(now_millis);

    let mut props = ElementPropertyMap::new();
    insert_json(
        &mut props,
        "eventId",
        serde_json::Value::Number(hijack.id.into()),
    );
    insert_if_some(
        &mut props,
        "duration",
        hijack.duration.map(serde_json::Value::from),
    );
    insert_if_some(
        &mut props,
        "confidenceScore",
        hijack.confidence_score.map(serde_json::Value::from),
    );
    insert_if_some(&mut props, "minHijackTs", hijack.min_hijack_ts.clone());
    insert_if_some(&mut props, "maxHijackTs", hijack.max_hijack_ts.clone());
    insert_if_some(
        &mut props,
        "hijackMsgCount",
        hijack.hijack_msgs_count.map(serde_json::Value::from),
    );
    insert_if_some(
        &mut props,
        "peerIpCount",
        hijack.peer_ip_count.map(serde_json::Value::from),
    );
    insert_if_some(
        &mut props,
        "isStale",
        hijack.is_stale.map(serde_json::Value::from),
    );

    changes.push(build_node(
        source_id,
        &hijack_id,
        "BgpHijack",
        props,
        effective_from,
        action,
    ));

    if include_shared && matches!(action, ChangeAction::Insert) {
        if let Some(asn) = hijack.hijacker_asn {
            let asn_id = format!("as-{asn}");
            let mut asn_props = ElementPropertyMap::new();
            insert_json(&mut asn_props, "asn", serde_json::Value::Number(asn.into()));
            changes.push(build_node(
                source_id,
                &asn_id,
                "AutonomousSystem",
                asn_props,
                effective_from,
                ChangeAction::Insert,
            ));
            changes.push(build_relation(
                source_id,
                &format!("{hijack_id}-hijacker-{asn_id}"),
                "HIJACKED_BY",
                &hijack_id,
                &asn_id,
                ElementPropertyMap::new(),
                effective_from,
                ChangeAction::Insert,
            ));
        }

        if let Some(ref victims) = hijack.victim_asns {
            for asn in victims {
                let asn_id = format!("as-{asn}");
                let mut asn_props = ElementPropertyMap::new();
                insert_json(
                    &mut asn_props,
                    "asn",
                    serde_json::Value::Number((*asn).into()),
                );
                changes.push(build_node(
                    source_id,
                    &asn_id,
                    "AutonomousSystem",
                    asn_props,
                    effective_from,
                    ChangeAction::Insert,
                ));
                changes.push(build_relation(
                    source_id,
                    &format!("{hijack_id}-victim-{asn_id}"),
                    "VICTIM_ASN",
                    &hijack_id,
                    &asn_id,
                    ElementPropertyMap::new(),
                    effective_from,
                    ChangeAction::Insert,
                ));
            }
        }

        if let Some(ref prefixes) = hijack.prefixes {
            for prefix in prefixes {
                let prefix_id = format!("prefix-{}", normalize_id(prefix));
                let mut prefix_props = ElementPropertyMap::new();
                insert_json(
                    &mut prefix_props,
                    "prefix",
                    serde_json::Value::String(prefix.clone()),
                );
                changes.push(build_node(
                    source_id,
                    &prefix_id,
                    "Prefix",
                    prefix_props,
                    effective_from,
                    ChangeAction::Insert,
                ));
                changes.push(build_relation(
                    source_id,
                    &format!("{hijack_id}-targets-{prefix_id}"),
                    "TARGETS_PREFIX",
                    &hijack_id,
                    &prefix_id,
                    ElementPropertyMap::new(),
                    effective_from,
                    ChangeAction::Insert,
                ));
            }
        }
    }

    changes
}

pub fn map_leak(
    source_id: &str,
    leak: &BgpLeakEvent,
    action: ChangeAction,
    include_shared: bool,
) -> Vec<SourceChange> {
    let mut changes = Vec::new();
    let leak_id = format!("bgp-leak-{}", leak.id);
    let effective_from =
        timestamp_millis_from_rfc3339(&leak.detected_ts).unwrap_or_else(now_millis);

    let mut props = ElementPropertyMap::new();
    insert_json(
        &mut props,
        "eventId",
        serde_json::Value::Number(leak.id.into()),
    );
    insert_if_some(
        &mut props,
        "leakCount",
        leak.leak_count.map(serde_json::Value::from),
    );
    insert_if_some(
        &mut props,
        "leakType",
        leak.leak_type.map(serde_json::Value::from),
    );
    insert_if_some(&mut props, "detectedTs", leak.detected_ts.clone());
    insert_if_some(&mut props, "minTs", leak.min_ts.clone());
    insert_if_some(&mut props, "maxTs", leak.max_ts.clone());
    insert_if_some(
        &mut props,
        "originCount",
        leak.origin_count.map(serde_json::Value::from),
    );
    insert_if_some(
        &mut props,
        "peerCount",
        leak.peer_count.map(serde_json::Value::from),
    );
    insert_if_some(
        &mut props,
        "prefixCount",
        leak.prefix_count.map(serde_json::Value::from),
    );
    insert_if_some(
        &mut props,
        "finished",
        leak.finished.map(serde_json::Value::from),
    );

    changes.push(build_node(
        source_id,
        &leak_id,
        "BgpLeak",
        props,
        effective_from,
        action,
    ));

    if include_shared && matches!(action, ChangeAction::Insert) {
        if let Some(asn) = leak.leak_asn {
            let asn_id = format!("as-{asn}");
            let mut asn_props = ElementPropertyMap::new();
            insert_json(&mut asn_props, "asn", serde_json::Value::Number(asn.into()));
            changes.push(build_node(
                source_id,
                &asn_id,
                "AutonomousSystem",
                asn_props,
                effective_from,
                ChangeAction::Insert,
            ));
            changes.push(build_relation(
                source_id,
                &format!("{leak_id}-leaked-{asn_id}"),
                "LEAKED_BY",
                &leak_id,
                &asn_id,
                ElementPropertyMap::new(),
                effective_from,
                ChangeAction::Insert,
            ));
        }
    }

    changes
}

/// Returns `(element_id, label)` pairs for all relationships belonging to an outage.
pub fn relation_element_ids_for_outage(
    outage_id: &str,
    outage: &OutageAnnotation,
) -> Vec<(String, String)> {
    let mut ids = Vec::new();
    for location in &outage.locations {
        let location_id = format!("location-{}", normalize_id(location));
        ids.push((
            format!("{outage_id}-affects-{location_id}"),
            "AFFECTS_LOCATION".to_string(),
        ));
    }
    for asn in &outage.asns {
        let asn_id = format!("as-{asn}");
        ids.push((
            format!("{outage_id}-affects-{asn_id}"),
            "AFFECTS_ASN".to_string(),
        ));
    }
    ids
}

/// Returns `(element_id, label)` pairs for all relationships belonging to a hijack.
pub fn relation_element_ids_for_hijack(hijack: &BgpHijackEvent) -> Vec<(String, String)> {
    let hijack_id = format!("bgp-hijack-{}", hijack.id);
    let mut ids = Vec::new();
    if let Some(asn) = hijack.hijacker_asn {
        let asn_id = format!("as-{asn}");
        ids.push((
            format!("{hijack_id}-hijacker-{asn_id}"),
            "HIJACKED_BY".to_string(),
        ));
    }
    if let Some(ref victims) = hijack.victim_asns {
        for asn in victims {
            let asn_id = format!("as-{asn}");
            ids.push((
                format!("{hijack_id}-victim-{asn_id}"),
                "VICTIM_ASN".to_string(),
            ));
        }
    }
    if let Some(ref prefixes) = hijack.prefixes {
        for prefix in prefixes {
            let prefix_id = format!("prefix-{}", normalize_id(prefix));
            ids.push((
                format!("{hijack_id}-targets-{prefix_id}"),
                "TARGETS_PREFIX".to_string(),
            ));
        }
    }
    ids
}

/// Returns `(element_id, label)` pairs for all relationships belonging to a leak.
pub fn relation_element_ids_for_leak(leak: &BgpLeakEvent) -> Vec<(String, String)> {
    let leak_id = format!("bgp-leak-{}", leak.id);
    let mut ids = Vec::new();
    if let Some(asn) = leak.leak_asn {
        let asn_id = format!("as-{asn}");
        ids.push((
            format!("{leak_id}-leaked-{asn_id}"),
            "LEAKED_BY".to_string(),
        ));
    }
    ids
}

/// Generates `SourceChange::Delete` entries for a list of relation `(element_id, label)` pairs.
pub fn delete_relations(source_id: &str, relation_ids: &[(String, String)]) -> Vec<SourceChange> {
    relation_ids
        .iter()
        .map(|(elem_id, label)| SourceChange::Delete {
            metadata: relation_metadata(source_id, elem_id, &[label], now_millis()),
        })
        .collect()
}

pub fn map_http_summary(
    source_id: &str,
    node_id: &str,
    series_name: &str,
    summary: &HttpSummaryData,
    action: ChangeAction,
    effective_from: u64,
) -> SourceChange {
    let mut props = ElementPropertyMap::new();
    insert_json(
        &mut props,
        "series",
        serde_json::Value::String(series_name.to_string()),
    );
    insert_if_some(&mut props, "desktop", summary.desktop.clone());
    insert_if_some(&mut props, "mobile", summary.mobile.clone());
    insert_if_some(&mut props, "other", summary.other.clone());
    build_node(
        source_id,
        node_id,
        "HttpTraffic",
        props,
        effective_from,
        action,
    )
}

pub fn map_attack_l7_summary(
    source_id: &str,
    node_id: &str,
    series_name: &str,
    summary: &AttackSummaryData,
    action: ChangeAction,
    effective_from: u64,
) -> SourceChange {
    let mut props = ElementPropertyMap::new();
    insert_json(
        &mut props,
        "series",
        serde_json::Value::String(series_name.to_string()),
    );
    insert_if_some(&mut props, "ddos", summary.ddos.clone());
    insert_if_some(&mut props, "waf", summary.waf.clone());
    insert_if_some(&mut props, "ipReputation", summary.ip_reputation.clone());
    insert_if_some(&mut props, "accessRules", summary.access_rules.clone());
    insert_if_some(&mut props, "botManagement", summary.bot_management.clone());
    insert_if_some(&mut props, "apiShield", summary.api_shield.clone());
    insert_if_some(
        &mut props,
        "dataLossPrevention",
        summary.data_loss_prevention.clone(),
    );
    build_node(
        source_id,
        node_id,
        "AttackL7",
        props,
        effective_from,
        action,
    )
}

pub fn map_attack_l3_summary(
    source_id: &str,
    node_id: &str,
    series_name: &str,
    summary: &AttackLayer3SummaryData,
    action: ChangeAction,
    effective_from: u64,
) -> SourceChange {
    let mut props = ElementPropertyMap::new();
    insert_json(
        &mut props,
        "series",
        serde_json::Value::String(series_name.to_string()),
    );
    insert_if_some(&mut props, "udp", summary.udp.clone());
    insert_if_some(&mut props, "tcp", summary.tcp.clone());
    insert_if_some(&mut props, "icmp", summary.icmp.clone());
    insert_if_some(&mut props, "gre", summary.gre.clone());
    build_node(
        source_id,
        node_id,
        "AttackL3",
        props,
        effective_from,
        action,
    )
}

pub fn map_domain_ranking(
    source_id: &str,
    domain: &DomainRank,
    action: ChangeAction,
    effective_from: u64,
) -> Vec<SourceChange> {
    let mut changes = Vec::new();
    let domain_id = format!("domain-{}", normalize_id(&domain.domain));
    let ranking_id = format!("ranking-{}", normalize_id(&domain.domain));

    if !matches!(action, ChangeAction::Delete) {
        let mut domain_props = ElementPropertyMap::new();
        insert_json(
            &mut domain_props,
            "name",
            serde_json::Value::String(domain.domain.clone()),
        );
        changes.push(build_node(
            source_id,
            &domain_id,
            "Domain",
            domain_props,
            effective_from,
            ChangeAction::Insert,
        ));
    }

    let mut ranking_props = ElementPropertyMap::new();
    if !matches!(action, ChangeAction::Delete) {
        insert_json(
            &mut ranking_props,
            "rank",
            serde_json::Value::Number(domain.rank.into()),
        );
        insert_json(
            &mut ranking_props,
            "domain",
            serde_json::Value::String(domain.domain.clone()),
        );
    }
    changes.push(build_node(
        source_id,
        &ranking_id,
        "DomainRanking",
        ranking_props,
        effective_from,
        action,
    ));

    let relation_action = if matches!(action, ChangeAction::Delete) {
        ChangeAction::Delete
    } else {
        ChangeAction::Insert
    };
    changes.push(build_relation(
        source_id,
        &format!("{domain_id}-ranked-as-{ranking_id}"),
        "RANKED_AS",
        &domain_id,
        &ranking_id,
        ElementPropertyMap::new(),
        effective_from,
        relation_action,
    ));

    changes
}

pub fn map_dns_summary(
    source_id: &str,
    domain: &str,
    locations: &[DnsLocationEntry],
    action: ChangeAction,
    effective_from: u64,
) -> SourceChange {
    let node_id = format!("dns-summary-{}", normalize_id(domain));
    let mut props = ElementPropertyMap::new();
    insert_json(
        &mut props,
        "domain",
        serde_json::Value::String(domain.to_string()),
    );
    let locations_value =
        serde_json::to_value(locations).unwrap_or(serde_json::Value::Null);
    insert_json(&mut props, "topLocations", locations_value);
    build_node(
        source_id,
        &node_id,
        "DnsQuerySummary",
        props,
        effective_from,
        action,
    )
}
