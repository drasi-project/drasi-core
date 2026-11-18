// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use crate::component::GeometryTransformer;
use anyhow::{Context, Result};
use drasi_computation_contracts::{
    ComponentId, GraphProducerProgress, InputEnvelope, OutputReadError, SnapshotProvider, StreamId,
};
use drasi_computation_plugin_sdk::UpdatePlanner;
use drasi_computation_plugin_sdk::{
    lifecycle::{DisposeReason, StateIntent, StopMode, UpdateBoundary},
    testing::{contexts, update_request},
    ApplyUpdateRequest, ExportedPlugin, NativeComponent, NativeTransformer,
};
use drasi_core_types::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

const ID: &str = "5b0f0c2e-4f0a-4a55-9b6e-0d6f1f2b9a10";

// ------------------------------------------------------------------ harness

fn create(configuration: Value) -> Result<GeometryTransformer> {
    GeometryTransformer::new(
        GeometryFactory
            .metadata()
            .descriptor(ComponentId::try_new(ID)?)?,
        configuration,
        None,
    )
}

struct Harness {
    transformer: GeometryTransformer,
    sequence: u64,
    time: u64,
}

impl Harness {
    async fn new(configuration: Value) -> Result<Self> {
        let mut transformer = create(configuration)?;
        transformer.initialize(&contexts::initialize()).await?;
        transformer.start(&mut contexts::start()).await?;
        Ok(Self {
            transformer,
            sequence: 0,
            time: 0,
        })
    }

    fn input(&mut self, changes: &[SourceChange]) -> Result<InputEnvelope> {
        self.sequence += 1;
        Ok(InputEnvelope {
            port: PortId::try_new("in")?,
            envelope: GraphChangeCodec::encode_changes(
                changes,
                StreamId::try_new("scene/out")?,
                self.sequence,
                None,
            )?,
        })
    }

    /// Sends one batch and returns the emitted changes.
    async fn send(&mut self, changes: Vec<SourceChange>) -> Result<Vec<SourceChange>> {
        let input = self.input(&changes)?;
        let mut outputs = self.transformer.transform(input).await?;
        assert_eq!(outputs.len(), 1);
        let output = outputs.pop().context("missing output")?;
        assert_eq!(output.port.as_str(), "out");
        let progress = GraphProducerProgress::from_envelope(&output.envelope)?
            .context("missing geometry progress")?;
        assert!(!progress.identity().persistent());
        assert_eq!(progress.identity().component_id().as_str(), ID);
        Ok(GraphChangeCodec::decode_changes(&output.envelope)?)
    }

    async fn upsert(&mut self, elements: Vec<Element>) -> Result<Vec<SourceChange>> {
        self.send(
            elements
                .into_iter()
                .map(|element| SourceChange::Insert { element })
                .collect(),
        )
        .await
    }

    async fn delete(&mut self, ids: &[&str]) -> Result<Vec<SourceChange>> {
        self.time += 1;
        let time = self.time;
        self.send(
            ids.iter()
                .map(|id| SourceChange::Delete {
                    metadata: ElementMetadata {
                        reference: ElementReference::new("scene", id),
                        labels: Arc::from([]),
                        effective_from: time,
                    },
                })
                .collect(),
        )
        .await
    }

    fn node(&mut self, label: &str, id: &str, properties: Value) -> Element {
        self.time += 1;
        Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("scene", id),
                labels: Arc::from([Arc::from(label)]),
                effective_from: self.time,
            },
            properties: ElementPropertyMap::from(properties),
        }
    }

    /// The published snapshot as `label:element id` → properties.
    async fn snapshot(&self) -> Result<BTreeMap<String, Map<String, Value>>> {
        let snapshot = reader(&self.transformer)?.snapshot().await?;
        assert_eq!(snapshot.cursor().sequence(), self.sequence);
        snapshot
            .records()
            .values()
            .map(|record| {
                let element = Element::try_from(GraphChangeCodec::decode_element(record)?)?;
                let metadata = element.get_metadata();
                Ok((
                    format!("{}:{}", metadata.labels[0], metadata.reference.element_id),
                    Map::from(element.get_properties()),
                ))
            })
            .collect()
    }

    /// The `id` property of every snapshot record with `label`.
    async fn ids(&self, label: &str) -> Result<Vec<String>> {
        Ok(self
            .snapshot()
            .await?
            .into_iter()
            .filter(|(key, _)| key.starts_with(&format!("{label}:")))
            .map(|(_, properties)| properties["id"].as_str().unwrap_or_default().to_string())
            .collect())
    }
}

fn reader(transformer: &GeometryTransformer) -> Result<Arc<dyn SnapshotProvider>> {
    transformer
        .output_readers()
        .ports()
        .get(&PortId::try_new("out")?)
        .and_then(|readers| readers.snapshot())
        .cloned()
        .context("missing snapshot reader")
}

/// (operation, id property or element id) of each change, in emitted order.
fn summary(changes: &[SourceChange]) -> Vec<(&'static str, String)> {
    changes
        .iter()
        .map(|change| match change {
            SourceChange::Insert { element } => ("insert", id_of(element)),
            SourceChange::Replace { element } => ("replace", id_of(element)),
            SourceChange::Update { element, .. } => ("update", id_of(element)),
            SourceChange::Delete { metadata } => {
                ("delete", metadata.reference.element_id.to_string())
            }
            SourceChange::Future { .. } => ("future", String::new()),
        })
        .collect()
}

fn id_of(element: &Element) -> String {
    match element.get_properties().get("id") {
        Some(drasi_core_types::models::ElementValue::String(id)) => id.to_string(),
        _ => element.get_reference().element_id.to_string(),
    }
}

fn relation_element_id(relation: &str, left: &str, right: &str) -> String {
    json!(["relation", relation, left, right]).to_string()
}

// ------------------------------------------------------- move-a-wall parity

/// The demo's obstruction rule expressed purely as configuration. The
/// upstream queries flatten Journey⋈Cart into `JourneyFootprint` rows and
/// active obstacles into `ObstacleFootprint` rows (see the README).
fn move_a_wall() -> Value {
    json!({
        "stream": "move-a-wall/obstructions",
        "sets": [
            {"name": "journeys", "label": "JourneyFootprint", "key": "journey_id",
             "geometry": {"type": "lineString", "property": "points"}},
            {"name": "obstacles", "label": "ObstacleFootprint", "key": "obstacle_id",
             "geometry": {"type": "polygon", "property": "vertices"}}
        ],
        "relations": [{
            "name": "obstruction", "left": "journeys", "right": "obstacles",
            "predicate": "withinDistance",
            "threshold": {"left": ["radius", "clearance"]},
            "label": "Obstruction",
            "properties": {"id": "id", "leftKey": "journey_id", "rightKey": "obstacle_id",
                           "distance": "distance_m", "threshold": "required_m"},
            "copyLeft": {"cart_id": "cart_id"}
        }]
    })
}

const GRACE: [[f64; 2]; 25] = [
    [2.0, 14.0],
    [2.599, 13.321],
    [3.246, 12.689],
    [3.938, 12.105],
    [4.671, 11.575],
    [5.441, 11.099],
    [6.244, 10.682],
    [7.076, 10.325],
    [7.931, 10.03],
    [8.806, 9.799],
    [9.696, 9.633],
    [10.596, 9.533],
    [11.5, 9.5],
    [12.404, 9.533],
    [13.304, 9.633],
    [14.194, 9.799],
    [15.069, 10.03],
    [15.924, 10.325],
    [16.756, 10.682],
    [17.559, 11.099],
    [18.329, 11.575],
    [19.062, 12.105],
    [19.754, 12.689],
    [20.401, 13.321],
    [21.0, 14.0],
];

fn wall(y: f64) -> Vec<[f64; 2]> {
    vec![[10.0, y], [13.0, y], [13.0, y + 1.0], [10.0, y + 1.0]]
}

/// The demo scene: (journey id, cart id, radius, clearance, path) and (obstacle id, polygon).
type Journeys = Vec<(&'static str, &'static str, f64, f64, Vec<[f64; 2]>)>;
type Obstacles = Vec<(&'static str, Vec<[f64; 2]>)>;

fn fixture() -> (Journeys, Obstacles) {
    (
        vec![
            (
                "journey-ada",
                "ada",
                0.45,
                0.2,
                vec![[2.0, 5.0], [21.0, 5.0]],
            ),
            ("journey-grace", "grace", 0.4, 0.2, GRACE.to_vec()),
        ],
        vec![
            (
                "wall",
                vec![[10.0, 7.0], [13.0, 7.0], [13.0, 8.0], [10.0, 8.0]],
            ),
            (
                "pallet",
                vec![[5.0, 1.0], [7.0, 1.0], [7.0, 2.5], [5.0, 2.5]],
            ),
            (
                "maintenance",
                vec![[18.0, 8.0], [21.0, 8.0], [22.0, 10.0], [19.0, 10.0]],
            ),
        ],
    )
}

/// The demo's `geometry::calculate`, verbatim in substance, as the oracle.
fn oracle(journeys: &Journeys, obstacles: &Obstacles) -> BTreeMap<String, (f64, f64)> {
    use geo::{Distance, Euclidean, LineString, Polygon};
    let mut results = BTreeMap::new();
    for (journey, _, radius, clearance, points) in journeys {
        let line = LineString::from(points.iter().map(|p| (p[0], p[1])).collect::<Vec<_>>());
        for (obstacle, vertices) in obstacles {
            let polygon = Polygon::new(
                LineString::from(vertices.iter().map(|p| (p[0], p[1])).collect::<Vec<_>>()),
                vec![],
            );
            let distance = Euclidean.distance(&line, &polygon);
            if distance <= radius + clearance + 1e-7 {
                results.insert(
                    format!("{journey}/{obstacle}"),
                    (distance, radius + clearance),
                );
            }
        }
    }
    results
}

impl Harness {
    fn journey(
        &mut self,
        (id, cart, radius, clearance, points): &(&str, &str, f64, f64, Vec<[f64; 2]>),
    ) -> Element {
        self.node(
            "JourneyFootprint",
            &format!("row-{id}"),
            json!({"journey_id": id, "cart_id": cart, "radius": radius,
                   "clearance": clearance, "points": points}),
        )
    }

    fn obstacle(&mut self, (id, vertices): &(&str, Vec<[f64; 2]>)) -> Element {
        self.node(
            "ObstacleFootprint",
            &format!("row-{id}"),
            json!({"obstacle_id": id, "vertices": vertices}),
        )
    }

    fn scene(&mut self, journeys: &Journeys, obstacles: &Obstacles) -> Vec<Element> {
        let mut rows: Vec<_> = journeys
            .iter()
            .map(|journey| self.journey(journey))
            .collect();
        rows.extend(obstacles.iter().map(|obstacle| self.obstacle(obstacle)));
        rows
    }

    async fn obstructions(&self) -> Result<BTreeMap<String, (f64, f64)>> {
        Ok(self
            .snapshot()
            .await?
            .into_iter()
            .filter(|(key, _)| key.starts_with("Obstruction:"))
            .map(|(_, p)| {
                (
                    p["id"].as_str().unwrap_or_default().to_string(),
                    (
                        p["distance_m"].as_f64().unwrap_or(f64::NAN),
                        p["required_m"].as_f64().unwrap_or(f64::NAN),
                    ),
                )
            })
            .collect())
    }
}

#[tokio::test]
async fn move_a_wall_obstructions_are_reproduced_by_configuration() -> Result<()> {
    let mut h = Harness::new(move_a_wall()).await?;
    let (journeys, mut obstacles) = fixture();
    let rows = h.scene(&journeys, &obstacles);
    assert!(h.upsert(rows).await?.is_empty(), "the fixture is clear");
    assert!(h.obstructions().await?.is_empty());

    // Sweep the wall across both paths, including the tangent and just-clear
    // positions, and compare with the demo's own calculation each step.
    let mut seen = BTreeMap::new();
    for y in [
        4.5, 5.5, 5.65, 5.65001, 7.0, 8.4, 8.5, 8.9, 9.0, 9.5, 12.0, 3.5,
    ] {
        obstacles[0].1 = wall(y);
        let row = h.obstacle(&obstacles[0]);
        h.upsert(vec![row]).await?;
        let expected = oracle(&journeys, &obstacles);
        let actual = h.obstructions().await?;
        assert_eq!(
            actual.keys().collect::<Vec<_>>(),
            expected.keys().collect::<Vec<_>>(),
            "wall at y={y}"
        );
        for (id, (distance, required)) in &expected {
            assert!((actual[id].0 - distance).abs() < 1e-12, "{id} at y={y}");
            assert_eq!(actual[id].1, *required, "{id} at y={y}");
        }
        seen.insert(y.to_string(), actual.into_keys().collect::<Vec<_>>());
    }
    assert_eq!(seen["4.5"], ["journey-ada/wall"]);
    assert_eq!(seen["5.5"], ["journey-ada/wall"]);
    assert_eq!(seen["5.65"], ["journey-ada/wall"], "tangency obstructs");
    assert!(seen["5.65001"].is_empty());
    assert_eq!(seen["9"], ["journey-grace/wall"]);

    // The emitted record matches the demo's Obstruction node.
    obstacles[0].1 = wall(4.5);
    let row = h.obstacle(&obstacles[0]);
    h.upsert(vec![row]).await?;
    let snapshot = h.snapshot().await?;
    let record = &snapshot[&format!(
        "Obstruction:{}",
        relation_element_id("obstruction", "journey-ada", "wall")
    )];
    assert_eq!(
        Value::Object(record.clone()),
        json!({"id": "journey-ada/wall", "journey_id": "journey-ada", "cart_id": "ada",
               "obstacle_id": "wall", "distance_m": 0.0, "required_m": 0.65})
    );

    // A second identical wall adds one obstruction; deactivating the first
    // (its footprint row leaves the upstream query) retracts only its own.
    let second = ("second", wall(4.5));
    let row = h.obstacle(&second);
    let changes = h.upsert(vec![row]).await?;
    assert_eq!(summary(&changes), [("insert", "journey-ada/second".into())]);
    let changes = h.delete(&["row-wall"]).await?;
    assert_eq!(
        summary(&changes),
        [(
            "delete",
            relation_element_id("obstruction", "journey-ada", "wall")
        )]
    );
    assert_eq!(h.ids("Obstruction").await?, ["journey-ada/second"]);

    // Deactivating a cart removes its journey's footprint row.
    let changes = h.delete(&["row-journey-ada"]).await?;
    assert_eq!(changes.len(), 1);
    assert!(h.obstructions().await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn move_a_wall_corner_and_cart_changes_match_the_demo() -> Result<()> {
    let mut h = Harness::new(move_a_wall()).await?;
    let (mut journeys, mut obstacles) = fixture();
    journeys[1].4 = vec![[2.0, 11.0], [14.0, 11.0], [14.0, 14.0], [21.0, 14.0]];
    obstacles[0].1 = vec![[14.3, 10.2], [15.0, 10.2], [15.0, 10.7], [14.3, 10.7]];
    let rows = h.scene(&journeys, &obstacles);
    h.upsert(rows).await?;
    assert_eq!(h.obstructions().await?.len(), 1);
    assert_eq!(
        h.obstructions().await?.into_keys().collect::<Vec<_>>(),
        oracle(&journeys, &obstacles)
            .into_keys()
            .collect::<Vec<_>>()
    );

    // A smaller cart (new radius on the joined row) clears the corner.
    journeys[1].2 = 0.05;
    journeys[1].3 = 0.05;
    let row = h.journey(&journeys[1]);
    let changes = h.upsert(vec![row]).await?;
    assert_eq!(summary(&changes)[0].0, "delete");
    assert_eq!(oracle(&journeys, &obstacles).len(), 0);
    assert!(h.obstructions().await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn demo_invalid_polygons_are_rejected() -> Result<()> {
    let mut h = Harness::new(move_a_wall()).await?;
    for vertices in [
        json!([[0, 0], [2, 2], [2, 0], [0, 2]]),
        json!([[0, 0], [4, 4], [4, 0], [0, 3]]),
        json!([[0, 0], [1, 0], [2, 0]]),
        json!([[0, 0], [1, 0], [1, 0], [0, 1]]),
        json!([[0, 0], [1, "x"], [0, 1]]),
    ] {
        let row = h.node(
            "ObstacleFootprint",
            "bad",
            json!({"obstacle_id": "bad", "vertices": vertices}),
        );
        assert!(h.upsert(vec![row]).await.is_err(), "{vertices}");
        h.sequence -= 1;
    }
    assert!(h.snapshot().await?.is_empty());
    Ok(())
}

// ---------------------------------------------------------- incremental

#[tokio::test]
async fn changes_are_incremental_and_unchanged_records_are_not_reemitted() -> Result<()> {
    let mut h = Harness::new(move_a_wall()).await?;
    let (journeys, obstacles) = fixture();
    let rows = h.scene(&journeys, &obstacles);
    h.upsert(rows).await?;
    let blocking = ("block", wall(4.5));
    let row = h.obstacle(&blocking);
    assert_eq!(
        summary(&h.upsert(vec![row]).await?),
        [("insert", "journey-ada/block".into())]
    );

    // Re-sending identical geometry (a new effective time) emits nothing.
    let row = h.obstacle(&blocking);
    assert!(h.upsert(vec![row]).await?.is_empty());

    // Moving it closer replaces the record with the new distance.
    let row = h.obstacle(&("block", wall(5.5)));
    let changes = h.upsert(vec![row]).await?;
    assert_eq!(summary(&changes), [("replace", "journey-ada/block".into())]);
    assert!((h.obstructions().await?["journey-ada/block"].0 - 0.5).abs() < 1e-12);

    // A patch carrying only the moved vertices updates the retained image.
    h.time += 1;
    let patch = Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("scene", "row-block"),
            labels: Arc::from([Arc::from("ObstacleFootprint")]),
            effective_from: h.time,
        },
        properties: ElementPropertyMap::from(json!({"vertices": wall(30.0)})),
    };
    let changes = h
        .send(vec![SourceChange::Update {
            element: patch,
            removed_properties: Default::default(),
        }])
        .await?;
    assert_eq!(
        summary(&changes),
        [(
            "delete",
            relation_element_id("obstruction", "journey-ada", "block")
        )]
    );

    // A delete of an unknown element is ignored.
    assert!(h.delete(&["never-seen"]).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn retractions_precede_insertions_and_output_is_deterministic() -> Result<()> {
    let config = move_a_wall();
    let (journeys, mut obstacles) = fixture();
    obstacles.push(("block-a", wall(4.5)));
    obstacles.push(("block-b", wall(4.6)));
    let mut snapshots = Vec::new();
    for reverse in [false, true] {
        let mut h = Harness::new(config.clone()).await?;
        let mut rows = h.scene(&journeys, &obstacles);
        if reverse {
            rows.reverse();
        }
        let changes = h.upsert(rows).await?;
        assert_eq!(
            summary(&changes),
            [
                ("insert", "journey-ada/block-a".into()),
                ("insert", "journey-ada/block-b".into())
            ]
        );
        // One batch that retracts block-a and adds block-c.
        let row = h.obstacle(&("block-c", wall(4.4)));
        h.time += 1;
        let changes = h
            .send(vec![
                SourceChange::Insert { element: row },
                SourceChange::Delete {
                    metadata: ElementMetadata {
                        reference: ElementReference::new("scene", "row-block-a"),
                        labels: Arc::from([]),
                        effective_from: h.time,
                    },
                },
            ])
            .await?;
        assert_eq!(summary(&changes)[0].0, "delete");
        assert_eq!(
            summary(&changes)[1],
            ("insert", "journey-ada/block-c".into())
        );
        snapshots.push(
            h.snapshot()
                .await?
                .into_iter()
                .map(|(key, mut properties)| {
                    properties.remove("distance_m");
                    (key, properties)
                })
                .collect::<Vec<_>>(),
        );
    }
    assert_eq!(snapshots[0], snapshots[1]);
    Ok(())
}

// --------------------------------------------------- geometry and predicates

fn zones(relations: Value) -> Value {
    json!({
        "stream": "zones/out",
        "sets": [
            {"name": "zones", "label": "Zone", "geometry": {"type": "geoJson", "property": "shape"}},
            {"name": "assets", "label": "Asset", "where": {"kind": "tracked"},
             "geometry": {"type": "point", "x": "x", "y": "y"}}
        ],
        "relations": relations
    })
}

#[tokio::test]
async fn point_geojson_and_topological_predicates() -> Result<()> {
    let mut h = Harness::new(zones(json!([
        {"name": "inside", "left": "zones", "right": "assets", "predicate": "contains",
         "label": "Inside", "properties": {"leftKey": "zone", "rightKey": "asset"},
         "copyRight": {"asset_name": "name"}},
        {"name": "touching", "left": "assets", "right": "zones", "predicate": "intersects",
         "label": "Touching"},
        {"name": "within", "left": "assets", "right": "zones", "predicate": "within",
         "label": "Within"},
        {"name": "near", "left": "assets", "right": "zones", "predicate": "withinDistance",
         "threshold": {"constant": 1.0, "left": ["reach"]}, "label": "Near",
         "properties": {"distance": "d"}}
    ])))
    .await?;
    let square = json!({"type": "Polygon",
        "coordinates": [[[0, 0], [10, 0], [10, 10], [0, 10], [0, 0]]]});
    let rows = vec![
        // GeoJSON as an object and as JSON text inside a Feature.
        h.node("Zone", "z1", json!({"shape": square})),
        h.node(
            "Zone",
            "z2",
            json!({"shape": json!({"type": "Feature", "properties": {},
                "geometry": {"type": "LineString", "coordinates": [[20, 0], [20, 10]]}})
                .to_string()}),
        ),
        h.node(
            "Asset",
            "a-in",
            json!({"kind": "tracked", "x": 5, "y": 5, "name": "in", "reach": 0}),
        ),
        h.node(
            "Asset",
            "a-edge",
            json!({"kind": "tracked", "x": 10, "y": 3, "name": "edge", "reach": 0}),
        ),
        h.node(
            "Asset",
            "a-near",
            json!({"kind": "tracked", "x": 18.5, "y": 3, "name": "near", "reach": 0.5}),
        ),
        h.node(
            "Asset",
            "a-far",
            json!({"kind": "tracked", "x": 15, "y": 3, "name": "far", "reach": 0}),
        ),
        // Filtered out by `where`, so its missing geometry is not an error.
        h.node("Asset", "a-ignored", json!({"kind": "untracked"})),
    ];
    h.upsert(rows).await?;
    assert_eq!(h.ids("Inside").await?, ["z1/a-in"]);
    assert_eq!(h.ids("Within").await?, ["a-in/z1"]);
    assert_eq!(h.ids("Touching").await?, ["a-edge/z1", "a-in/z1"]);
    assert_eq!(
        h.ids("Near").await?,
        ["a-edge/z1", "a-in/z1", "a-near/z2"],
        "a-near is 1.5 from z2: constant 1 + reach 0.5"
    );
    let snapshot = h.snapshot().await?;
    let inside = &snapshot[&format!("Inside:{}", relation_element_id("inside", "z1", "a-in"))];
    assert_eq!(
        Value::Object(inside.clone()),
        json!({"id": "z1/a-in", "zone": "z1", "asset": "a-in", "asset_name": "in"})
    );

    // Changing the filter property removes the asset from the set.
    let row = h.node("Asset", "a-in", json!({"kind": "retired", "x": 5, "y": 5}));
    h.upsert(vec![row]).await?;
    assert!(h.ids("Inside").await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn polygons_with_holes_and_ring_closure() -> Result<()> {
    let config = |closure: &str| {
        json!({
            "stream": "s",
            "ringClosure": closure,
            "sets": [
                {"name": "areas", "label": "Area", "geometry": {"type": "polygon", "property": "rings"}},
                {"name": "points", "label": "P", "geometry": {"type": "point", "property": "at"}}
            ],
            "relations": [{"name": "in", "left": "areas", "right": "points",
                           "predicate": "contains", "label": "In"}]
        })
    };
    let donut = json!([
        [[0, 0], [10, 0], [10, 10], [0, 10], [0, 0]],
        [[4, 4], [6, 4], [6, 6], [4, 6], [4, 4]]
    ]);
    let mut h = Harness::new(config("required")).await?;
    let rows = vec![
        h.node("Area", "donut", json!({"rings": donut})),
        h.node("P", "hole", json!({"at": {"x": 5, "y": 5}})),
        h.node("P", "ring", json!({"at": [2, 2]})),
    ];
    h.upsert(rows).await?;
    assert_eq!(h.ids("In").await?, ["donut/ring"]);
    let open = h.node("Area", "open", json!({"rings": [[0, 0], [1, 0], [0, 1]]}));
    assert!(h.upsert(vec![open]).await.is_err(), "required closure");

    let mut h = Harness::new(config("auto")).await?;
    let rows = vec![
        h.node("Area", "open", json!({"rings": [[0, 0], [3, 0], [0, 3]]})),
        h.node("P", "p", json!({"at": [1, 1]})),
    ];
    h.upsert(rows).await?;
    assert_eq!(h.ids("In").await?, ["open/p"]);
    Ok(())
}

// -------------------------------------------------------- error policies

fn with(mut config: Value, extra: Value) -> Value {
    for (key, value) in extra.as_object().cloned().unwrap_or_default() {
        config[key] = value;
    }
    config
}

#[tokio::test]
async fn fail_policy_rejects_the_batch_and_keeps_state() -> Result<()> {
    let mut h = Harness::new(move_a_wall()).await?;
    let (journeys, obstacles) = fixture();
    let good = h.journey(&journeys[0]);
    let blocking = h.obstacle(&("block", wall(4.5)));
    h.upsert(vec![good, blocking]).await?;
    let before = h.snapshot().await?;
    let moved = h.obstacle(&("block", wall(40.0)));
    let bad = h.node(
        "JourneyFootprint",
        "row-bad",
        json!({"journey_id": "bad", "cart_id": "c", "radius": -1, "clearance": 0,
               "points": [[0, 0], [1, 0]]}),
    );
    let error = h.upsert(vec![moved, bad]).await.unwrap_err();
    assert!(
        format!("{error:#}").contains("must not be negative"),
        "{error:#}"
    );
    h.sequence -= 1;
    assert_eq!(h.snapshot().await?, before);
    let _ = obstacles;
    Ok(())
}

#[tokio::test]
async fn skip_and_report_policies_isolate_invalid_records() -> Result<()> {
    for policy in ["skip", "report"] {
        let mut h = Harness::new(with(move_a_wall(), json!({"invalidGeometry": policy}))).await?;
        let (journeys, _) = fixture();
        let journey = h.journey(&journeys[0]);
        let blocking = h.obstacle(&("block", wall(4.5)));
        h.upsert(vec![journey, blocking]).await?;
        assert_eq!(h.ids("Obstruction").await?, ["journey-ada/block"]);

        // Breaking the obstacle's geometry retracts its relation.
        let broken = h.node(
            "ObstacleFootprint",
            "row-block",
            json!({"obstacle_id": "block", "vertices": [[0, 0], [4, 4], [4, 0], [0, 3]]}),
        );
        let changes = h.upsert(vec![broken]).await?;
        assert!(h.ids("Obstruction").await?.is_empty());
        let errors: Vec<_> = h
            .snapshot()
            .await?
            .into_iter()
            .filter(|(key, _)| key.starts_with("GeometryError:"))
            .collect();
        if policy == "skip" {
            assert_eq!(summary(&changes).len(), 1);
            assert!(errors.is_empty());
        } else {
            assert_eq!(summary(&changes).len(), 2);
            assert_eq!(errors.len(), 1);
            let (key, properties) = &errors[0];
            assert_eq!(
                key,
                &format!(
                    "GeometryError:{}",
                    json!(["error", "obstacles", "scene", "row-block"])
                )
            );
            assert_eq!(properties["set"], "obstacles");
            assert_eq!(properties["element"], "row-block");
            assert!(
                properties["error"]
                    .as_str()
                    .is_some_and(|e| e.contains("self-intersection") || e.contains("intersect")),
                "{properties:?}"
            );
        }

        // Fixing it retracts the error and restores the relation.
        let fixed = h.obstacle(&("block", wall(4.5)));
        h.upsert(vec![fixed]).await?;
        assert_eq!(h.ids("Obstruction").await?, ["journey-ada/block"]);
        assert_eq!(h.snapshot().await?.len(), 1);
    }
    Ok(())
}

#[tokio::test]
async fn duplicate_keys_are_per_record_errors() -> Result<()> {
    let mut h = Harness::new(with(move_a_wall(), json!({"invalidGeometry": "report"}))).await?;
    let first = h.obstacle(&("block", wall(4.5)));
    h.upsert(vec![first]).await?;
    let twin = h.node(
        "ObstacleFootprint",
        "row-twin",
        json!({"obstacle_id": "block", "vertices": wall(4.5)}),
    );
    h.upsert(vec![twin]).await?;
    let snapshot = h.snapshot().await?;
    assert_eq!(snapshot.len(), 1);
    let error = snapshot.values().next().context("error record")?;
    assert!(error["error"]
        .as_str()
        .is_some_and(|e| e.contains("duplicate key 'block'")));

    let mut h = Harness::new(move_a_wall()).await?;
    let first = h.obstacle(&("block", wall(4.5)));
    let twin = h.node(
        "ObstacleFootprint",
        "row-twin",
        json!({"obstacle_id": "block", "vertices": wall(4.5)}),
    );
    assert!(h.upsert(vec![first, twin]).await.is_err(), "fail policy");
    Ok(())
}

#[tokio::test]
async fn limits_bound_retained_inputs_vertices_and_outputs() -> Result<()> {
    let (journeys, _) = fixture();
    let mut h = Harness::new(with(move_a_wall(), json!({"limits": {"maxElements": 2}}))).await?;
    let rows = vec![
        h.journey(&journeys[0]),
        h.obstacle(&("a", wall(30.0))),
        h.obstacle(&("b", wall(31.0))),
    ];
    assert!(h.upsert(rows).await.is_err());
    h.sequence -= 1;
    // Elements outside every set are not retained or counted.
    let rows = vec![
        h.journey(&journeys[0]),
        h.obstacle(&("a", wall(30.0))),
        h.node("Other", "x", json!({})),
    ];
    h.upsert(rows).await?;

    let mut h = Harness::new(with(move_a_wall(), json!({"limits": {"maxVertices": 10}}))).await?;
    let row = h.journey(&journeys[1]);
    let error = h.upsert(vec![row]).await.unwrap_err();
    assert!(format!("{error:#}").contains("maxVertices"), "{error:#}");

    let mut h = Harness::new(with(move_a_wall(), json!({"limits": {"maxOutputs": 1}}))).await?;
    let rows = vec![
        h.journey(&journeys[0]),
        h.obstacle(&("a", wall(4.5))),
        h.obstacle(&("b", wall(4.6))),
    ];
    let error = h.upsert(rows).await.unwrap_err();
    assert!(format!("{error:#}").contains("maxOutputs"), "{error:#}");
    Ok(())
}

#[tokio::test]
async fn patches_without_a_retained_image_fail_only_for_relevant_elements() -> Result<()> {
    let mut h = Harness::new(move_a_wall()).await?;
    let unrelated = h.node("Other", "x", json!({"a": 1}));
    h.send(vec![SourceChange::Update {
        element: unrelated,
        removed_properties: Default::default(),
    }])
    .await?;
    let relevant = h.node("ObstacleFootprint", "y", json!({"obstacle_id": "y"}));
    assert!(h
        .send(vec![SourceChange::Update {
            element: relevant,
            removed_properties: Default::default(),
        }])
        .await
        .is_err());
    Ok(())
}

// ------------------------------------------------------- configuration

#[test]
fn metadata_is_valid() -> Result<()> {
    ExportedPlugin::new(plugin()?)?;
    let metadata = GeometryFactory.metadata();
    assert_eq!(&*metadata.implementation.name, GEOMETRY);
    assert!(metadata.capabilities.management && metadata.capabilities.erases_owned_state);
    Ok(())
}

#[test]
fn configuration_round_trips_through_create() -> Result<()> {
    let metadata = GeometryFactory.metadata();
    let details = GeometryFactory.configuration_details();
    details.validate_schema(&metadata.configuration)?;
    for configuration in [
        move_a_wall(),
        with(
            move_a_wall(),
            json!({"epsilon": 0.001, "ringClosure": "required", "invalidGeometry": "report",
                   "errorLabel": "Bad", "limits": {"maxElements": 5, "maxVertices": 6, "maxOutputs": 7}}),
        ),
        zones(json!([{"name": "n", "left": "assets", "right": "zones",
                      "predicate": "intersects", "label": "N"}])),
    ] {
        let exposed = create(configuration.clone())?.configuration()?;
        assert!(
            details.equivalent_with_defaults(&metadata.configuration, &configuration, &exposed)?,
            "{configuration} vs {exposed}"
        );
        assert_eq!(create(exposed.clone())?.configuration()?, exposed);
    }
    Ok(())
}

#[test]
fn invalid_configurations_are_rejected() {
    let relation = |extra: Value| {
        let mut config = move_a_wall();
        for (key, value) in extra.as_object().cloned().unwrap_or_default() {
            config["relations"][0][key] = value;
        }
        config
    };
    for config in [
        with(move_a_wall(), json!({"unknown": 1})),
        with(move_a_wall(), json!({"stream": ""})),
        with(move_a_wall(), json!({"epsilon": -1})),
        with(move_a_wall(), json!({"limits": {"maxOutputs": 0}})),
        with(move_a_wall(), json!({"relations": []})),
        with(
            move_a_wall(),
            json!({"invalidGeometry": "report", "errorLabel": "Obstruction"}),
        ),
        relation(json!({"right": "missing"})),
        relation(json!({"threshold": null})),
        relation(json!({"predicate": "intersects"})),
        relation(json!({"copyLeft": {"id": "cart_id"}})),
        relation(json!({"threshold": {"constant": -1}})),
        relation(json!({"properties": {"id": "x", "distance": "x"}})),
        {
            let mut config = move_a_wall();
            config["sets"][0]["geometry"] = json!({"type": "point", "x": "a"});
            config
        },
        {
            let mut config = move_a_wall();
            config["sets"][0]["geometry"]["extra"] = json!(1);
            config
        },
        {
            let mut config = move_a_wall();
            config["sets"][1]["name"] = json!("journeys");
            config
        },
        {
            let mut config = move_a_wall();
            config["sets"]
                .as_array_mut()
                .expect("sets")
                .push(json!({"name": "unused", "label": "U",
                             "geometry": {"type": "point", "property": "p"}}));
            config
        },
    ] {
        assert!(create(config.clone()).is_err(), "{config}");
    }
}

// ------------------------------------------------------------ lifecycle

#[tokio::test]
async fn lifecycle_pause_restart_and_erase() -> Result<()> {
    let mut h = Harness::new(move_a_wall()).await?;
    let (journeys, _) = fixture();
    let rows = vec![h.journey(&journeys[0]), h.obstacle(&("block", wall(4.5)))];
    h.upsert(rows).await?;

    h.transformer.pause(&contexts::pause()).await?;
    let row = h.obstacle(&("other", wall(4.5)));
    assert!(h.upsert(vec![row]).await.is_err(), "paused");
    h.sequence -= 1;
    h.transformer.resume(&contexts::resume()).await?;

    let reader = reader(&h.transformer)?;
    h.transformer.stop(&contexts::stop(StopMode::Drain)).await?;
    assert!(matches!(
        reader.snapshot().await,
        Err(OutputReadError::NotReady(_))
    ));
    h.transformer
        .dispose(&contexts::dispose(
            DisposeReason::Shutdown,
            StateIntent::Preserve,
            1,
        )?)
        .await?;
    h.transformer.start(&mut contexts::start()).await?;
    assert_eq!(
        h.ids("Obstruction").await?,
        ["journey-ada/block"],
        "retained"
    );
    assert!(h.transformer.start(&mut contexts::start()).await.is_err());

    assert!(
        h.transformer
            .dispose(&contexts::dispose(
                DisposeReason::Removal,
                StateIntent::Erase,
                1
            )?)
            .await
            .is_err(),
        "must stop first"
    );
    h.transformer.stop(&contexts::stop(StopMode::Drain)).await?;
    h.transformer
        .dispose(&contexts::dispose(
            DisposeReason::Removal,
            StateIntent::Erase,
            1,
        )?)
        .await?;
    assert!(matches!(
        reader.snapshot().await,
        Err(OutputReadError::Retired)
    ));
    assert!(h.transformer.start(&mut contexts::start()).await.is_err());

    let mut cancelled = create(move_a_wall())?;
    assert!(cancelled
        .start(&mut contexts::cancelled_start())
        .await
        .is_err());
    Ok(())
}

// ----------------------------------------------------------- management

fn request(current: Value, proposed: Value) -> Result<drasi_computation_plugin_sdk::UpdateRequest> {
    Ok(update_request(
        &GeometryFactory,
        &ComponentId::try_new(ID)?,
        current,
        proposed,
    ))
}

fn boundary(assessment: Result<Assessment>) -> Option<UpdateBoundary> {
    match assessment {
        Ok(Assessment::Supported { boundary, .. }) => Some(boundary),
        _ => None,
    }
}

#[test]
fn assessment_matrix() -> Result<()> {
    let current = move_a_wall();
    let mut renamed = move_a_wall();
    renamed["relations"][0]["label"] = json!("Blocked");
    for (proposed, expected) in [
        (current.clone(), Some(UpdateBoundary::Pause)),
        (
            with(current.clone(), json!({"limits": {"maxOutputs": 5}})),
            Some(UpdateBoundary::Pause),
        ),
        (
            with(current.clone(), json!({"invalidGeometry": "skip"})),
            Some(UpdateBoundary::Pause),
        ),
        (
            with(current.clone(), json!({"invalidGeometry": "report"})),
            Some(UpdateBoundary::Replace),
        ),
        (
            with(current.clone(), json!({"epsilon": 0.01})),
            Some(UpdateBoundary::Replace),
        ),
        (renamed, Some(UpdateBoundary::Replace)),
        (with(current.clone(), json!({"stream": "other"})), None),
        (with(current.clone(), json!({"bogus": true})), None),
    ] {
        assert_eq!(
            boundary(GeometryFactory.assess(&request(current.clone(), proposed.clone())?)),
            expected,
            "{proposed}"
        );
    }
    let parsed = config::GeometryConfig::parse(current)?;
    assert_eq!(
        boundary(Ok(config::plan(&parsed, &parsed, true))),
        Some(UpdateBoundary::Replace),
        "changed input bindings"
    );
    Ok(())
}

#[tokio::test]
async fn in_place_update_applies_at_a_pause_and_respects_retained_state() -> Result<()> {
    let mut h = Harness::new(move_a_wall()).await?;
    let (journeys, _) = fixture();
    let rows = vec![
        h.journey(&journeys[0]),
        h.obstacle(&("a", wall(4.5))),
        h.obstacle(&("b", wall(4.6))),
    ];
    h.upsert(rows).await?;

    // Tighter limits than the retained state need a rebuild.
    let tight = with(move_a_wall(), json!({"limits": {"maxOutputs": 1}}));
    let assessment =
        NativeManagement::assess(&h.transformer, &request(move_a_wall(), tight.clone())?)?;
    assert_eq!(boundary(Ok(assessment)), Some(UpdateBoundary::Replace));

    let proposed = with(
        move_a_wall(),
        json!({"limits": {"maxOutputs": 2}, "invalidGeometry": "skip"}),
    );
    let request = request(move_a_wall(), proposed.clone())?;
    let assessment = NativeManagement::assess(&h.transformer, &request)?;
    let update = ApplyUpdateRequest {
        operation_id: "op".into(),
        request: request.clone(),
        plan: assessment.plan().context("plan")?,
        fingerprint: None,
    };
    let ctx = contexts::apply(UpdateBoundary::Pause);
    let management = h.transformer.management().context("management")?;
    assert!(management.apply(&ctx, &update).await.is_err(), "not paused");
    h.transformer.pause(&contexts::pause()).await?;
    let mut stale = update.clone();
    stale.request.current.configuration = tight;
    let management = h.transformer.management().context("management")?;
    assert!(management.apply(&ctx, &stale).await.is_err(), "stale");
    management.apply(&ctx, &update).await?;
    h.transformer.resume(&contexts::resume()).await?;
    assert_eq!(h.transformer.configuration()?["limits"]["maxOutputs"], 2);

    // The new limit applies to later input; retained records are kept.
    let row = h.obstacle(&("c", wall(4.7)));
    assert!(h.upsert(vec![row]).await.is_err());
    h.sequence -= 1;
    assert_eq!(h.ids("Obstruction").await?.len(), 2);
    Ok(())
}

use drasi_computation_plugin_sdk::NativeManagement;
