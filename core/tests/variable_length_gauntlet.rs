//! Independent-oracle gauntlet for bounded variable-length MATCH.

#![allow(clippy::unwrap_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use drasi_core::{
    evaluation::{
        context::{QueryPartEvaluationContext, QueryVariables},
        functions::{FunctionRegistry, RegisterAggregationFunctions},
    },
    models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
    },
    query::{ContinuousQuery, QueryBuilder, VariableLengthMatchLimits},
};
use drasi_query_cypher::CypherParser;
use serde_json::{json, Value};

const SOURCE: &str = "g";

#[derive(Clone, Copy, Debug)]
enum Dir {
    Right,
    Left,
    Either,
}

impl Dir {
    fn arrows(self) -> (&'static str, &'static str) {
        match self {
            Dir::Right => ("-", "->"),
            Dir::Left => ("<-", "-"),
            Dir::Either => ("-", "-"),
        }
    }
}

#[derive(Clone, Debug)]
struct Segment {
    min: usize,
    max: usize,
    dir: Dir,
    rel_label: &'static str,
    enabled_only: bool,
}

#[derive(Clone, Debug)]
struct Pattern {
    start_label: Option<&'static str>,
    end_label: Option<&'static str>,
    segments: Vec<Segment>,
    where_neq: bool,
    tail_list: bool,
}

#[derive(Clone, Debug)]
enum Expect {
    Reject,
    Rows,
}

#[derive(Clone, Debug)]
struct Scenario {
    id: usize,
    family: &'static str,
    query: String,
    pattern: Option<Pattern>,
    changes: Vec<SourceChange>,
    expect: Expect,
}

struct Graph {
    nodes: BTreeMap<String, Vec<String>>,
    rels: BTreeMap<String, Rel>,
}

struct Rel {
    from: String,
    to: String,
    labels: Vec<String>,
    enabled: bool,
}

impl Graph {
    fn new() -> Self {
        Self {
            nodes: BTreeMap::new(),
            rels: BTreeMap::new(),
        }
    }

    fn apply(&mut self, change: &SourceChange) {
        match change {
            SourceChange::Insert { element } | SourceChange::Update { element } => match element {
                Element::Node { metadata, .. } => {
                    self.nodes.insert(
                        metadata.reference.element_id.to_string(),
                        metadata
                            .labels
                            .iter()
                            .map(|label| label.as_ref().to_string())
                            .collect(),
                    );
                }
                Element::Relation {
                    metadata,
                    in_node,
                    out_node,
                    properties,
                    ..
                } => {
                    let enabled = match properties.get("enabled") {
                        Some(ElementValue::Bool(value)) => *value,
                        _ => true,
                    };
                    self.rels.insert(
                        metadata.reference.element_id.to_string(),
                        Rel {
                            from: in_node.element_id.to_string(),
                            to: out_node.element_id.to_string(),
                            labels: metadata
                                .labels
                                .iter()
                                .map(|label| label.as_ref().to_string())
                                .collect(),
                            enabled,
                        },
                    );
                }
            },
            SourceChange::Delete { metadata } => {
                self.nodes.remove(metadata.reference.element_id.as_ref());
                self.rels.remove(metadata.reference.element_id.as_ref());
            }
            SourceChange::Future { .. } => {}
        }
    }

    fn rows(&self, pattern: &Pattern) -> Vec<String> {
        let mut rows = Vec::new();
        for start in self.nodes.keys() {
            if !self.node_ok(start, pattern.start_label) {
                continue;
            }
            self.walk(
                pattern,
                start,
                start,
                0,
                0,
                0,
                &mut BTreeSet::new(),
                &mut Vec::new(),
                &mut rows,
            );
        }
        rows.sort();
        rows
    }

    fn node_ok(&self, id: &str, label: Option<&str>) -> bool {
        match (self.nodes.get(id), label) {
            (None, _) => false,
            (_, None) => true,
            (Some(labels), Some(want)) => labels.iter().any(|label| label == want),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn walk(
        &self,
        pattern: &Pattern,
        start: &str,
        current: &str,
        seg: usize,
        hops: usize,
        first_len: usize,
        used: &mut BTreeSet<String>,
        edges: &mut Vec<String>,
        rows: &mut Vec<String>,
    ) {
        if seg == pattern.segments.len() {
            return;
        }
        let segment = &pattern.segments[seg];
        if hops >= segment.min && hops <= segment.max {
            if seg + 1 == pattern.segments.len() {
                if self.node_ok(current, pattern.end_label)
                    && (!pattern.where_neq || start != current)
                {
                    rows.push(project_row(pattern, start, current, edges, first_len));
                }
            } else {
                self.walk(
                    pattern,
                    start,
                    current,
                    seg + 1,
                    0,
                    edges.len(),
                    used,
                    edges,
                    rows,
                );
            }
        }
        if hops >= segment.max {
            return;
        }
        let candidates: Vec<(String, String)> = self
            .rels
            .iter()
            .filter_map(|(id, rel)| {
                if used.contains(id) {
                    return None;
                }
                if !rel.labels.iter().any(|label| label == segment.rel_label) {
                    return None;
                }
                if segment.enabled_only && !rel.enabled {
                    return None;
                }
                let next = match segment.dir {
                    Dir::Right if rel.from == current => Some(rel.to.clone()),
                    Dir::Left if rel.to == current => Some(rel.from.clone()),
                    Dir::Either if rel.from == current => Some(rel.to.clone()),
                    Dir::Either if rel.to == current => Some(rel.from.clone()),
                    _ => None,
                }?;
                if !self.nodes.contains_key(&next) {
                    return None;
                }
                Some((id.clone(), next))
            })
            .collect();
        for (id, next) in candidates {
            used.insert(id.clone());
            edges.push(id.clone());
            self.walk(
                pattern,
                start,
                &next,
                seg,
                hops + 1,
                first_len,
                used,
                edges,
                rows,
            );
            edges.pop();
            used.remove(&id);
        }
    }
}

fn canon(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<_> = map.keys().cloned().collect();
            keys.sort();
            let fields = keys
                .iter()
                .map(|key| format!("\"{}\":{}", key, canon(&map[key])))
                .collect::<Vec<_>>()
                .join(",");
            format!("{{{fields}}}")
        }
        Value::Array(items) => {
            let inner = items.iter().map(canon).collect::<Vec<_>>().join(",");
            format!("[{inner}]")
        }
        other => other.to_string(),
    }
}

fn project_row(
    pattern: &Pattern,
    start: &str,
    end: &str,
    edges: &[String],
    first_len: usize,
) -> String {
    let mut map = serde_json::Map::new();
    map.insert("start".to_string(), json!(start));
    map.insert("end".to_string(), json!(end));
    if pattern.segments.len() == 1 {
        map.insert("edges".to_string(), json!(edges));
    } else {
        map.insert("first".to_string(), json!(&edges[..first_len]));
        if pattern.tail_list {
            map.insert("last".to_string(), json!(&edges[first_len..]));
        } else {
            map.insert("last".to_string(), json!(edges[first_len]));
        }
    }
    canon(&Value::Object(map))
}

fn engine_row(vars: &QueryVariables) -> String {
    let mut object = serde_json::Map::new();
    for (key, value) in vars.iter() {
        object.insert(key.to_string(), value.clone().into());
    }
    canon(&Value::Object(object))
}

fn metadata(id: &str, labels: &[&str], effective_from: u64) -> ElementMetadata {
    ElementMetadata {
        reference: ElementReference::new(SOURCE, id),
        labels: labels.iter().map(|label| Arc::from(*label)).collect(),
        effective_from,
    }
}

fn node(id: &str, label: &str) -> SourceChange {
    SourceChange::Insert {
        element: Element::Node {
            metadata: metadata(id, &[label], 1),
            properties: ElementPropertyMap::from(json!({"id": id})),
        },
    }
}

fn node_labels(id: &str, labels: &[&str]) -> SourceChange {
    SourceChange::Insert {
        element: Element::Node {
            metadata: metadata(id, labels, 1),
            properties: ElementPropertyMap::from(json!({"id": id})),
        },
    }
}

fn rel(id: &str, from: &str, to: &str) -> SourceChange {
    rel_labeled(id, from, to, "R", true)
}

fn rel_labeled(id: &str, from: &str, to: &str, label: &str, enabled: bool) -> SourceChange {
    SourceChange::Insert {
        element: Element::Relation {
            metadata: metadata(id, &[label], 1),
            properties: ElementPropertyMap::from(json!({"id": id, "enabled": enabled})),
            in_node: ElementReference::new(SOURCE, from),
            out_node: ElementReference::new(SOURCE, to),
        },
    }
}

fn delete(id: &str) -> SourceChange {
    SourceChange::Delete {
        metadata: metadata(id, &[], 2),
    }
}

fn update_node_label(id: &str, label: &str) -> SourceChange {
    SourceChange::Update {
        element: Element::Node {
            metadata: metadata(id, &[label], 3),
            properties: ElementPropertyMap::from(json!({"id": id})),
        },
    }
}

fn update_rel_enabled(id: &str, from: &str, to: &str, enabled: bool) -> SourceChange {
    SourceChange::Update {
        element: Element::Relation {
            metadata: metadata(id, &["R"], 3),
            properties: ElementPropertyMap::from(json!({"id": id, "enabled": enabled})),
            in_node: ElementReference::new(SOURCE, from),
            out_node: ElementReference::new(SOURCE, to),
        },
    }
}

fn rewire(id: &str, from: &str, to: &str) -> SourceChange {
    SourceChange::Update {
        element: Element::Relation {
            metadata: metadata(id, &["R"], 4),
            properties: ElementPropertyMap::from(json!({"id": id, "enabled": true})),
            in_node: ElementReference::new(SOURCE, from),
            out_node: ElementReference::new(SOURCE, to),
        },
    }
}

fn quantifier(min: usize, max: usize) -> String {
    if min == max {
        format!("*{min}")
    } else if min == 1 {
        format!("*..{max}")
    } else {
        format!("*{min}..{max}")
    }
}

fn match_query(pattern: &Pattern) -> String {
    let mut cypher = String::from("MATCH (a");
    if let Some(label) = pattern.start_label {
        cypher.push(':');
        cypher.push_str(label);
    }
    cypher.push(')');
    for (index, segment) in pattern.segments.iter().enumerate() {
        let (left, right) = segment.dir.arrows();
        let name = if index == 0 { "rs" } else { "t" };
        cypher.push_str(left);
        cypher.push('[');
        cypher.push_str(name);
        cypher.push(':');
        cypher.push_str(segment.rel_label);
        cypher.push_str(&quantifier(segment.min, segment.max));
        if segment.enabled_only {
            cypher.push_str(" {enabled: true}");
        }
        cypher.push(']');
        cypher.push_str(right);
        if index + 1 == pattern.segments.len() {
            cypher.push('(');
            cypher.push('b');
            if let Some(label) = pattern.end_label {
                cypher.push(':');
                cypher.push_str(label);
            }
            cypher.push(')');
        } else {
            cypher.push_str("()");
        }
    }
    if pattern.where_neq {
        cypher.push_str(" WHERE a.id <> b.id");
    }
    if pattern.segments.len() == 1 {
        cypher.push_str(" RETURN a.id AS start, b.id AS end, [r IN rs | r.id] AS edges");
    } else {
        cypher.push_str(
            " RETURN a.id AS start, b.id AS end, [r IN rs | r.id] AS first, [r IN t | r.id] AS last",
        );
    }
    cypher
}

fn graphs() -> Vec<(&'static str, Vec<SourceChange>)> {
    vec![
        (
            "nodes_only",
            vec![
                node("a", "N"),
                node("b", "N"),
                node("c", "N"),
                node("d", "N"),
            ],
        ),
        (
            "chain3",
            vec![
                node("a", "N"),
                node("b", "N"),
                node("c", "N"),
                rel("ab", "a", "b"),
                rel("bc", "b", "c"),
            ],
        ),
        (
            "chain4",
            vec![
                node("a", "N"),
                node("b", "N"),
                node("c", "N"),
                node("d", "N"),
                rel("ab", "a", "b"),
                rel("bc", "b", "c"),
                rel("cd", "c", "d"),
            ],
        ),
        (
            "cycle3",
            vec![
                node("a", "N"),
                node("b", "N"),
                node("c", "N"),
                rel("ab", "a", "b"),
                rel("bc", "b", "c"),
                rel("ca", "c", "a"),
            ],
        ),
        (
            "star",
            vec![
                node("a", "N"),
                node("b", "N"),
                node("c", "N"),
                node("d", "N"),
                rel("ab", "a", "b"),
                rel("ac", "a", "c"),
                rel("ad", "a", "d"),
            ],
        ),
        (
            "diamond",
            vec![
                node("a", "N"),
                node("b", "N"),
                node("c", "N"),
                node("d", "N"),
                rel("ab", "a", "b"),
                rel("ac", "a", "c"),
                rel("bd", "b", "d"),
                rel("cd", "c", "d"),
            ],
        ),
        (
            "parallel",
            vec![
                node("a", "N"),
                node("b", "N"),
                rel("ab1", "a", "b"),
                rel("ab2", "a", "b"),
            ],
        ),
        (
            "loop",
            vec![
                node("a", "N"),
                node("b", "N"),
                rel("loop", "a", "a"),
                rel("ab", "a", "b"),
            ],
        ),
        (
            "back_edge",
            vec![
                node("a", "N"),
                node("b", "N"),
                node("c", "N"),
                rel("ab", "a", "b"),
                rel("bc", "b", "c"),
                rel("ba", "b", "a"),
            ],
        ),
        (
            "two_components",
            vec![
                node("a", "N"),
                node("b", "N"),
                node("c", "N"),
                node("d", "N"),
                rel("ab", "a", "b"),
                rel("cd", "c", "d"),
            ],
        ),
        (
            "k3",
            vec![
                node("a", "N"),
                node("b", "N"),
                node("c", "N"),
                rel("ab", "a", "b"),
                rel("bc", "b", "c"),
                rel("ca", "c", "a"),
                rel("ba", "b", "a"),
                rel("cb", "c", "b"),
                rel("ac", "a", "c"),
            ],
        ),
        (
            "disabled_edge",
            vec![
                node("a", "N"),
                node("b", "N"),
                node("c", "N"),
                rel_labeled("ab", "a", "b", "R", true),
                rel_labeled("bc", "b", "c", "R", false),
            ],
        ),
        (
            "comma_s",
            vec![
                node("a", "N"),
                node("b", "N"),
                node("c", "N"),
                node("d", "N"),
                rel_labeled("ab", "a", "b", "R", true),
                rel_labeled("bc", "b", "c", "R", true),
                rel_labeled("cd", "c", "d", "S", true),
            ],
        ),
    ]
}

fn bounds() -> Vec<(usize, usize)> {
    vec![
        (0, 0),
        (0, 1),
        (0, 2),
        (1, 1),
        (1, 2),
        (1, 3),
        (2, 2),
        (2, 3),
    ]
}

fn dirs() -> [Dir; 3] {
    [Dir::Right, Dir::Left, Dir::Either]
}

fn rejects() -> Vec<&'static str> {
    vec![
        "MATCH (a)-[:R*]->(b) RETURN a",
        "MATCH (a)-[:R*-1]->(b) RETURN a",
        "MATCH (a)-[:R*3..2]->(b) RETURN a",
        "MATCH (a)-[:R*1..]->(b) RETURN a",
        "MATCH (a)-[:R*..]->(b) RETURN a",
        "OPTIONAL MATCH (a)-[:R*1..2]->(b) RETURN a",
        "MATCH (a)-[:R*1..2]->(b) OPTIONAL MATCH (b)-[:S]->(c) RETURN a",
        "MATCH (a)-[:R*1..2]->(b), (c:Disconnected) RETURN a",
        "MATCH (a)-[:R*1 {config: [{enabled: a.enabled}]}]->(b) RETURN b",
        "MATCH (a)-[:R*1 {config: [{enabled: toBoolean('true')}]}]->(b) RETURN b",
        "MATCH (a)-[rs:R*1..2]->(b)-[rs]->(c) RETURN a",
        "MATCH (a)-[:R*65]->(b) RETURN a",
        "MATCH (a)-[:R*0..65]->(b) RETURN a",
        "MATCH (a)-[:R*1..2]->(b) SET a.x = 1 RETURN a",
        "MATCH (a)-[:R*1..2]->(b) DELETE a RETURN a",
        "MATCH (a)-[:R*1..2]->(b) WITH a MATCH (a)-[:S]->(c) RETURN a",
        "MATCH (a)-[:R*]->(b)-[:S*]->(c) RETURN a",
        "MATCH ()-[:R*]->() RETURN 1",
        "MATCH (a)-[:R*100]->(b) RETURN a",
        "MATCH (a)-[rs:R*]-(b) RETURN a",
        "MATCH (a)<-[:R*]-(b) RETURN a",
        "MATCH (a)-[:R*..100]->(b) RETURN a",
        "MATCH (a)-[:R*2..1]->(b) RETURN a",
        "MATCH (a)-[:R*-2..3]->(b) RETURN a",
        "MATCH (a)-[:R*1..2]->(b) MATCH (c)-[:S]->(d) RETURN a",
        "MATCH p = (a)-[:R*1..2]->(b) RETURN p",
        "MATCH (a)-[:R*1..2 | S]->(b) RETURN a",
        "MATCH (a)-[rs:R*1..2]->(b) UNWIND rs AS r RETURN r",
        "MATCH (a)-[:R*1..2]->(b) CREATE (c) RETURN a",
        "MATCH (a)-[:R*1..2]->(b) MERGE (c:N) RETURN a",
        "MATCH (a)-[:R*1..2]->(b) RETURN a UNION MATCH (c) RETURN c",
    ]
}

fn mutation_scripts() -> Vec<(&'static str, Vec<SourceChange>)> {
    vec![
        (
            "grow_then_cut",
            vec![
                node("a", "N"),
                node("b", "N"),
                node("c", "N"),
                rel("ab", "a", "b"),
                rel("bc", "b", "c"),
                delete("bc"),
                rel("bc", "b", "c"),
                delete("b"),
                node("b", "N"),
            ],
        ),
        (
            "relabel_endpoint",
            vec![
                node("a", "Start"),
                node("b", "Transit"),
                node("c", "End"),
                rel("ab", "a", "b"),
                rel("bc", "b", "c"),
                update_node_label("c", "Transit"),
                update_node_label("c", "End"),
            ],
        ),
        (
            "toggle_enabled",
            vec![
                node("a", "N"),
                node("b", "N"),
                node("c", "N"),
                rel_labeled("ab", "a", "b", "R", true),
                rel_labeled("bc", "b", "c", "R", true),
                update_rel_enabled("bc", "b", "c", false),
                update_rel_enabled("bc", "b", "c", true),
            ],
        ),
        (
            "rewire_middle",
            vec![
                node("a", "N"),
                node("b", "N"),
                node("c", "N"),
                node("d", "N"),
                rel("ab", "a", "b"),
                rel("bc", "b", "c"),
                rewire("bc", "b", "d"),
                rewire("bc", "b", "c"),
            ],
        ),
        (
            "type_change",
            vec![
                node("a", "N"),
                node("b", "N"),
                rel_labeled("ab", "a", "b", "R", true),
                SourceChange::Update {
                    element: Element::Relation {
                        metadata: metadata("ab", &["S"], 5),
                        properties: ElementPropertyMap::from(json!({"id": "ab", "enabled": true})),
                        in_node: ElementReference::new(SOURCE, "a"),
                        out_node: ElementReference::new(SOURCE, "b"),
                    },
                },
                SourceChange::Update {
                    element: Element::Relation {
                        metadata: metadata("ab", &["R"], 6),
                        properties: ElementPropertyMap::from(json!({"id": "ab", "enabled": true})),
                        in_node: ElementReference::new(SOURCE, "a"),
                        out_node: ElementReference::new(SOURCE, "b"),
                    },
                },
            ],
        ),
        (
            "both_labels",
            vec![
                node_labels("a", &["Start", "End"]),
                node("b", "End"),
                rel("ab", "a", "b"),
            ],
        ),
    ]
}

fn scenarios() -> Vec<Scenario> {
    let mut out = Vec::new();
    let mut id = 0usize;
    let next_id = |id: &mut usize| {
        let value = *id;
        *id += 1;
        value
    };

    for query in rejects() {
        out.push(Scenario {
            id: next_id(&mut id),
            family: "reject",
            query: query.to_string(),
            pattern: None,
            changes: Vec::new(),
            expect: Expect::Reject,
        });
    }

    for (name, changes) in graphs() {
        for dir in dirs() {
            for (min, max) in bounds() {
                let pattern = Pattern {
                    start_label: Some("N"),
                    end_label: Some("N"),
                    segments: vec![Segment {
                        min,
                        max,
                        dir,
                        rel_label: "R",
                        enabled_only: false,
                    }],
                    where_neq: false,
                    tail_list: true,
                };
                out.push(Scenario {
                    id: next_id(&mut id),
                    family: name,
                    query: match_query(&pattern),
                    pattern: Some(pattern),
                    changes: changes.clone(),
                    expect: Expect::Rows,
                });
            }
        }
    }

    for dir in dirs() {
        for (min, max) in [(1, 1), (1, 2), (2, 2)] {
            for (name, changes) in graphs().into_iter().take(6) {
                let pattern = Pattern {
                    start_label: Some("N"),
                    end_label: Some("N"),
                    segments: vec![
                        Segment {
                            min,
                            max,
                            dir,
                            rel_label: "R",
                            enabled_only: false,
                        },
                        Segment {
                            min: 1,
                            max: 1,
                            dir,
                            rel_label: "R",
                            enabled_only: false,
                        },
                    ],
                    where_neq: false,
                    tail_list: true,
                };
                out.push(Scenario {
                    id: next_id(&mut id),
                    family: "mixed_segments",
                    query: match_query(&pattern),
                    pattern: Some(pattern),
                    changes,
                    expect: Expect::Rows,
                });
                let _ = name;
            }
        }
    }

    for dir in dirs() {
        for (min, max) in [(0, 2), (1, 2)] {
            let pattern = Pattern {
                start_label: Some("N"),
                end_label: Some("N"),
                segments: vec![Segment {
                    min,
                    max,
                    dir,
                    rel_label: "R",
                    enabled_only: false,
                }],
                where_neq: true,
                tail_list: true,
            };
            out.push(Scenario {
                id: next_id(&mut id),
                family: "where_neq",
                query: match_query(&pattern),
                pattern: Some(pattern),
                changes: graphs()
                    .into_iter()
                    .find(|(name, _)| *name == "cycle3")
                    .unwrap()
                    .1,
                expect: Expect::Rows,
            });
        }
    }

    for dir in dirs() {
        let pattern = Pattern {
            start_label: Some("N"),
            end_label: Some("N"),
            segments: vec![Segment {
                min: 1,
                max: 2,
                dir,
                rel_label: "R",
                enabled_only: true,
            }],
            where_neq: false,
            tail_list: true,
        };
        out.push(Scenario {
            id: next_id(&mut id),
            family: "enabled_filter",
            query: match_query(&pattern),
            pattern: Some(pattern),
            changes: graphs()
                .into_iter()
                .find(|(name, _)| *name == "disabled_edge")
                .unwrap()
                .1,
            expect: Expect::Rows,
        });
    }

    for (name, changes) in mutation_scripts() {
        for dir in [Dir::Right, Dir::Either] {
            let (start, end, enabled_only) = match name {
                "relabel_endpoint" => (Some("Start"), Some("End"), false),
                "toggle_enabled" => (Some("N"), Some("N"), true),
                "both_labels" => (Some("Start"), Some("End"), false),
                _ => (Some("N"), Some("N"), false),
            };
            let pattern = Pattern {
                start_label: start,
                end_label: end,
                segments: vec![Segment {
                    min: 0,
                    max: 2,
                    dir,
                    rel_label: "R",
                    enabled_only,
                }],
                where_neq: false,
                tail_list: true,
            };
            out.push(Scenario {
                id: next_id(&mut id),
                family: name,
                query: match_query(&pattern),
                pattern: Some(pattern),
                changes: changes.clone(),
                expect: Expect::Rows,
            });
        }
    }

    let unlabeled = Pattern {
        start_label: None,
        end_label: None,
        segments: vec![Segment {
            min: 1,
            max: 2,
            dir: Dir::Right,
            rel_label: "R",
            enabled_only: false,
        }],
        where_neq: false,
        tail_list: true,
    };
    out.push(Scenario {
        id: next_id(&mut id),
        family: "unlabeled",
        query: match_query(&unlabeled),
        pattern: Some(unlabeled),
        changes: graphs()
            .into_iter()
            .find(|(name, _)| *name == "chain4")
            .unwrap()
            .1,
        expect: Expect::Rows,
    });

    let incoming = Pattern {
        start_label: Some("N"),
        end_label: Some("N"),
        segments: vec![Segment {
            min: 1,
            max: 2,
            dir: Dir::Left,
            rel_label: "R",
            enabled_only: false,
        }],
        where_neq: false,
        tail_list: true,
    };
    {
        let pattern = Pattern {
            start_label: Some("N"),
            end_label: Some("N"),
            segments: vec![
                Segment {
                    min: 1,
                    max: 2,
                    dir: Dir::Right,
                    rel_label: "R",
                    enabled_only: false,
                },
                Segment {
                    min: 1,
                    max: 1,
                    dir: Dir::Right,
                    rel_label: "S",
                    enabled_only: false,
                },
            ],
            where_neq: false,
            tail_list: false,
        };
        out.push(Scenario {
            id: next_id(&mut id),
            family: "comma_connected",
            query: "MATCH (a:N)-[rs:R*1..2]->(b:N), (b)-[t:S]->(c:N) RETURN a.id AS start, c.id AS end, [r IN rs | r.id] AS first, t.id AS last".to_string(),
            pattern: Some(pattern),
            changes: graphs()
                .into_iter()
                .find(|(name, _)| *name == "comma_s")
                .unwrap()
                .1,
            expect: Expect::Rows,
        });
    }

    out.push(Scenario {
        id: next_id(&mut id),
        family: "incoming_chain",
        query: match_query(&incoming),
        pattern: Some(incoming),
        changes: graphs()
            .into_iter()
            .find(|(name, _)| *name == "chain4")
            .unwrap()
            .1,
        expect: Expect::Rows,
    });

    for (name, changes) in graphs() {
        if !matches!(
            name,
            "chain4" | "diamond" | "star" | "cycle3" | "k3" | "back_edge"
        ) {
            continue;
        }
        for dir in dirs() {
            for (min, max) in [(0, 2), (1, 2), (2, 2)] {
                let pattern = Pattern {
                    start_label: Some("N"),
                    end_label: Some("N"),
                    segments: vec![Segment {
                        min,
                        max,
                        dir,
                        rel_label: "R",
                        enabled_only: false,
                    }],
                    where_neq: true,
                    tail_list: true,
                };
                out.push(Scenario {
                    id: next_id(&mut id),
                    family: "where_neq_more",
                    query: match_query(&pattern),
                    pattern: Some(pattern),
                    changes: changes.clone(),
                    expect: Expect::Rows,
                });
            }
        }
    }

    for dir in dirs() {
        for (min, max) in [(1, 2), (2, 2)] {
            let pattern = Pattern {
                start_label: Some("N"),
                end_label: Some("N"),
                segments: vec![
                    Segment {
                        min,
                        max,
                        dir,
                        rel_label: "R",
                        enabled_only: false,
                    },
                    Segment {
                        min: 1,
                        max: 2,
                        dir,
                        rel_label: "R",
                        enabled_only: false,
                    },
                ],
                where_neq: false,
                tail_list: true,
            };
            out.push(Scenario {
                id: next_id(&mut id),
                family: "mixed_range_range",
                query: match_query(&pattern),
                pattern: Some(pattern),
                changes: graphs()
                    .into_iter()
                    .find(|(name, _)| *name == "diamond")
                    .unwrap()
                    .1,
                expect: Expect::Rows,
            });
        }
    }

    out.truncate(500);
    while out.len() < 500 {
        let dir = dirs()[out.len() % 3];
        let (min, max) = bounds()[out.len() % bounds().len()];
        let (name, changes) = graphs()[out.len() % graphs().len()].clone();
        let pattern = Pattern {
            start_label: Some("N"),
            end_label: Some("N"),
            segments: vec![Segment {
                min,
                max,
                dir,
                rel_label: "R",
                enabled_only: false,
            }],
            where_neq: out.len() % 5 == 0,
            tail_list: true,
        };
        out.push(Scenario {
            id: next_id(&mut id),
            family: name,
            query: match_query(&pattern),
            pattern: Some(pattern),
            changes,
            expect: Expect::Rows,
        });
    }
    out.truncate(500);
    out
}

async fn try_build(query: &str) -> Result<ContinuousQuery, String> {
    try_build_ex(query, false, None).await
}

async fn try_build_ex(
    query: &str,
    aggregate: bool,
    limits: Option<VariableLengthMatchLimits>,
) -> Result<ContinuousQuery, String> {
    let registry = Arc::new(FunctionRegistry::new());
    if aggregate {
        registry.register_aggregation_functions();
    }
    let parser = Arc::new(CypherParser::new(registry.clone()));
    let mut builder = QueryBuilder::new(query, parser).with_function_registry(registry);
    if let Some(limits) = limits {
        builder = builder.with_variable_length_match_limits(limits);
    }
    builder
        .try_build()
        .await
        .map_err(|err| format!("build: {err}"))
}

fn apply_engine(
    live: &mut BTreeMap<String, usize>,
    contexts: Vec<QueryPartEvaluationContext>,
) -> Result<(), String> {
    for context in contexts {
        match context {
            QueryPartEvaluationContext::Adding { after, .. } => {
                *live.entry(engine_row(&after)).or_insert(0) += 1;
            }
            QueryPartEvaluationContext::Updating { before, after, .. } => {
                let before = engine_row(&before);
                match live.get_mut(&before) {
                    Some(count) if *count > 1 => *count -= 1,
                    Some(count) if *count == 1 => {
                        live.remove(&before);
                    }
                    _ => return Err(format!("update missed before row {before}")),
                }
                *live.entry(engine_row(&after)).or_insert(0) += 1;
            }
            QueryPartEvaluationContext::Removing { before, .. } => {
                let before = engine_row(&before);
                match live.get_mut(&before) {
                    Some(count) if *count > 1 => *count -= 1,
                    Some(_) => {
                        live.remove(&before);
                    }
                    None => return Err(format!("remove missed row {before}")),
                }
            }
            QueryPartEvaluationContext::Noop => {}
            other => return Err(format!("unexpected context {other:?}")),
        }
    }
    Ok(())
}

fn live_rows(live: &BTreeMap<String, usize>) -> Vec<String> {
    let mut rows = Vec::new();
    for (row, count) in live {
        for _ in 0..*count {
            rows.push(row.clone());
        }
    }
    rows.sort();
    rows
}

#[derive(Debug)]
struct Failure {
    id: usize,
    family: &'static str,
    query: String,
    step: usize,
    change: String,
    expected: Vec<String>,
    actual: Vec<String>,
    error: Option<String>,
}

#[tokio::test(flavor = "current_thread")]
async fn five_hundred_variable_length_scenarios() {
    let all = scenarios();
    assert_eq!(all.len(), 500, "gauntlet must hold 500 scenarios");

    let mut failures = Vec::new();
    let mut reject_pass = 0usize;
    let mut reject_fail = 0usize;
    let mut row_pass = 0usize;
    let mut family_fail: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut family_pass: BTreeMap<&'static str, usize> = BTreeMap::new();

    for scenario in &all {
        match scenario.expect {
            Expect::Reject => match try_build(&scenario.query).await {
                Ok(_) => {
                    reject_fail += 1;
                    *family_fail.entry(scenario.family).or_insert(0) += 1;
                    failures.push(Failure {
                        id: scenario.id,
                        family: scenario.family,
                        query: scenario.query.clone(),
                        step: 0,
                        change: "build".to_string(),
                        expected: vec!["reject".to_string()],
                        actual: vec!["accepted".to_string()],
                        error: None,
                    });
                }
                Err(_) => {
                    reject_pass += 1;
                    *family_pass.entry(scenario.family).or_insert(0) += 1;
                }
            },
            Expect::Rows => {
                let query = match try_build(&scenario.query).await {
                    Ok(query) => query,
                    Err(err) => {
                        *family_fail.entry(scenario.family).or_insert(0) += 1;
                        failures.push(Failure {
                            id: scenario.id,
                            family: scenario.family,
                            query: scenario.query.clone(),
                            step: 0,
                            change: "build".to_string(),
                            expected: vec!["build ok".to_string()],
                            actual: Vec::new(),
                            error: Some(err),
                        });
                        continue;
                    }
                };
                let mut graph = Graph::new();
                let mut live = BTreeMap::new();
                let mut failed = false;
                for (step, change) in scenario.changes.iter().enumerate() {
                    graph.apply(change);
                    let expected = scenario
                        .pattern
                        .as_ref()
                        .map(|pattern| graph.rows(pattern))
                        .unwrap_or_default();
                    match query.process_source_change(change.clone()).await {
                        Ok(contexts) => {
                            if let Err(err) = apply_engine(&mut live, contexts) {
                                failed = true;
                                *family_fail.entry(scenario.family).or_insert(0) += 1;
                                failures.push(Failure {
                                    id: scenario.id,
                                    family: scenario.family,
                                    query: scenario.query.clone(),
                                    step,
                                    change: format!("{change:?}"),
                                    expected,
                                    actual: live_rows(&live),
                                    error: Some(err),
                                });
                                break;
                            }
                        }
                        Err(err) => {
                            failed = true;
                            *family_fail.entry(scenario.family).or_insert(0) += 1;
                            failures.push(Failure {
                                id: scenario.id,
                                family: scenario.family,
                                query: scenario.query.clone(),
                                step,
                                change: format!("{change:?}"),
                                expected,
                                actual: live_rows(&live),
                                error: Some(err.to_string()),
                            });
                            break;
                        }
                    }
                    let actual = live_rows(&live);
                    if actual != expected {
                        failed = true;
                        *family_fail.entry(scenario.family).or_insert(0) += 1;
                        failures.push(Failure {
                            id: scenario.id,
                            family: scenario.family,
                            query: scenario.query.clone(),
                            step,
                            change: format!("{change:?}"),
                            expected,
                            actual,
                            error: None,
                        });
                        break;
                    }
                }
                if failed {
                    continue;
                }
                row_pass += 1;
                *family_pass.entry(scenario.family).or_insert(0) += 1;
            }
        }
    }

    let mut report = String::new();
    report.push_str("# Variable-length MATCH gauntlet\n\n");
    report.push_str(&format!(
        "- scenarios: 500\n- reject pass: {reject_pass}\n- reject fail (accepted): {reject_fail}\n- row pass: {row_pass}\n- failures: {}\n\n",
        failures.len()
    ));
    report.push_str("## Family pass/fail\n\n");
    let mut families: BTreeSet<&'static str> = BTreeSet::new();
    families.extend(family_pass.keys().copied());
    families.extend(family_fail.keys().copied());
    for family in families {
        report.push_str(&format!(
            "- {family}: pass {} / fail {}\n",
            family_pass.get(family).copied().unwrap_or(0),
            family_fail.get(family).copied().unwrap_or(0)
        ));
    }
    report.push_str("\n## Failures\n\n");
    for failure in &failures {
        report.push_str(&format!(
            "### #{} {} step {}\n\n```\n{}\n```\n\nchange: `{}`\n\n",
            failure.id, failure.family, failure.step, failure.query, failure.change
        ));
        if let Some(error) = &failure.error {
            report.push_str(&format!("error: `{error}`\n\n"));
        }
        report.push_str(&format!(
            "expected ({}):\n```\n{}\n```\n\nactual ({}):\n```\n{}\n```\n\n",
            failure.expected.len(),
            failure.expected.join("\n"),
            failure.actual.len(),
            failure.actual.join("\n")
        ));
    }
    println!(
        "gauntlet done: pass {} fail {}",
        reject_pass + row_pass,
        failures.len()
    );
    println!("reject pass {reject_pass} reject-accepted {reject_fail} row pass {row_pass}");
    for (family, count) in &family_fail {
        println!(
            "FAIL family {family}: {count} (pass {})",
            family_pass.get(family).copied().unwrap_or(0)
        );
    }
    assert!(
        failures.is_empty(),
        "{} scenarios broke\n{report}",
        failures.len()
    );
}

fn extra_graphs() -> Vec<(&'static str, Vec<SourceChange>, Pattern)> {
    let chain = Pattern {
        start_label: Some("N"),
        end_label: Some("N"),
        segments: vec![Segment {
            min: 1,
            max: 2,
            dir: Dir::Right,
            rel_label: "R",
            enabled_only: false,
        }],
        where_neq: false,
        tail_list: true,
    };
    let diamond = chain.clone();
    vec![
        (
            "chain",
            vec![
                node("a", "N"),
                node("b", "N"),
                node("c", "N"),
                rel("ab", "a", "b"),
                rel("bc", "b", "c"),
            ],
            chain,
        ),
        (
            "diamond",
            vec![
                node("a", "N"),
                node("b", "N"),
                node("c", "N"),
                node("d", "N"),
                rel("ab", "a", "b"),
                rel("ac", "a", "c"),
                rel("bd", "b", "d"),
                rel("cd", "c", "d"),
            ],
            diamond,
        ),
        (
            "parallel",
            vec![
                node("a", "N"),
                node("b", "N"),
                rel("ab1", "a", "b"),
                rel("ab2", "a", "b"),
            ],
            Pattern {
                start_label: Some("N"),
                end_label: Some("N"),
                segments: vec![Segment {
                    min: 1,
                    max: 1,
                    dir: Dir::Right,
                    rel_label: "R",
                    enabled_only: false,
                }],
                where_neq: false,
                tail_list: true,
            },
        ),
    ]
}

#[tokio::test(flavor = "current_thread")]
async fn aggregation_gauntlet() {
    let mut failures = Vec::new();
    for (name, changes, pattern) in extra_graphs() {
        let query_text = format!(
            "{} RETURN count(b) AS paths",
            match_query(&pattern).split(" RETURN ").next().unwrap()
        );
        let query = match try_build_ex(&query_text, true, None).await {
            Ok(query) => query,
            Err(err) => {
                failures.push(format!("{name} build: {err}"));
                continue;
            }
        };
        let mut graph = Graph::new();
        let mut count = 0i64;
        for (step, change) in changes.iter().enumerate() {
            graph.apply(change);
            let expected = graph.rows(&pattern).len() as i64;
            match query.process_source_change(change.clone()).await {
                Ok(contexts) => {
                    for context in contexts {
                        match context {
                            QueryPartEvaluationContext::Aggregation { after, .. } => {
                                count = after
                                    .get("paths")
                                    .and_then(|value| value.as_i64())
                                    .unwrap_or(-1);
                            }
                            QueryPartEvaluationContext::Noop => {}
                            other => {
                                failures.push(format!("{name} step {step}: unexpected {other:?}"));
                            }
                        }
                    }
                }
                Err(err) => {
                    failures.push(format!("{name} step {step}: {err}"));
                    break;
                }
            }
            if count != expected {
                failures.push(format!(
                    "{name} step {step}: count {count} expected {expected}"
                ));
                break;
            }
        }
    }
    println!("aggregation failures: {}", failures.len());
    for failure in &failures {
        println!("FAIL {failure}");
    }
    assert!(failures.is_empty(), "{failures:?}");
}

#[tokio::test(flavor = "current_thread")]
async fn timer_gauntlet() {
    let mut failures = Vec::new();
    for (name, changes, pattern) in extra_graphs() {
        let query_text = format!(
            "{} WHERE drasi.trueLater(true, 10) RETURN a.id AS start, b.id AS end, [r IN rs | r.id] AS edges",
            match_query(&pattern).split(" WHERE ").next().unwrap().split(" RETURN ").next().unwrap()
        );
        let query = match try_build(&query_text).await {
            Ok(query) => query,
            Err(err) => {
                failures.push(format!("{name} build: {err}"));
                continue;
            }
        };
        let mut graph = Graph::new();
        let mut live = BTreeMap::new();
        for (step, change) in changes.iter().enumerate() {
            graph.apply(change);
            match query.process_source_change(change.clone()).await {
                Ok(contexts) => {
                    if let Err(err) = apply_engine(&mut live, contexts) {
                        failures.push(format!("{name} insert {step}: {err}"));
                        break;
                    }
                }
                Err(err) => {
                    failures.push(format!("{name} insert {step}: {err}"));
                    break;
                }
            }
            if !live.is_empty() {
                failures.push(format!(
                    "{name} insert {step}: rows before due {:?}",
                    live_rows(&live)
                ));
                break;
            }
        }
        if failures.iter().any(|failure| failure.starts_with(name)) {
            continue;
        }
        let mut wakes = 0;
        loop {
            match query.process_due_futures().await {
                Ok(Some(due)) => {
                    wakes += 1;
                    if wakes > 16 {
                        failures.push(format!("{name}: too many wakes"));
                        break;
                    }
                    if let Err(err) = apply_engine(&mut live, due.results) {
                        failures.push(format!("{name} wake {wakes}: {err}"));
                        break;
                    }
                }
                Ok(None) => break,
                Err(err) => {
                    failures.push(format!("{name} wake: {err}"));
                    break;
                }
            }
        }
        let expected = graph.rows(&pattern);
        let actual = live_rows(&live);
        if actual != expected {
            failures.push(format!(
                "{name} after due: expected {} got {} ({:?} vs {:?})",
                expected.len(),
                actual.len(),
                expected,
                actual
            ));
        }
    }
    println!("timer failures: {}", failures.len());
    for failure in &failures {
        println!("FAIL {failure}");
    }
    assert!(failures.is_empty(), "{failures:?}");
}
