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

#![cfg(feature = "computation")]
#[allow(dead_code)]
mod computation_support;

use async_trait::async_trait;
use computation_support::*;
use drasi_lib::computation::v1::*;

struct UnavailableSource(ComponentDescriptor);
#[async_trait]
impl ComputationComponent for UnavailableSource {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.0
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        anyhow::bail!("operational failure must not be exported");
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}
#[async_trait]
impl EnvelopeSource for UnavailableSource {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        std::future::pending().await
    }
}

fn graph() -> ComputationGraph {
    ComputationGraph::builder("export")
        .source(Box::new(UnavailableSource(descriptor(
            "source",
            &[],
            &["out"],
        ))))
        .sink(Box::new(CollectSink::new(
            "sink",
            &["in"],
            Received::default(),
        )))
        .bind_stream(endpoint("source", "out"), stream("source"))
        .connect(
            edge("source", "sink"),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()
        .expect("graph")
}

#[tokio::test]
async fn desired_export_is_revision_consistent_and_excludes_all_observed_state() {
    let mut graph = graph();
    let before = graph
        .snapshot()
        .select(GraphSelection::All)
        .expect("desired selection")
        .to_json()
        .expect("export");
    let run = graph.start().expect("scope");
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        let report = control.startup_report().await.expect("startup");
        assert_eq!(report.summary, OperationSummary::CompletedWithFailures);
        let during = control
            .desired_snapshot()
            .select(GraphSelection::All)
            .expect("selection")
            .to_json()
            .expect("export");
        assert_eq!(before, during);
        assert!(!during.contains("operational failure"));
        assert!(!during.contains("health"));
        assert!(!during.contains("generation"));
        assert!(!during.contains("Running"));
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)));
    assert_eq!(
        before,
        graph
            .snapshot()
            .select(GraphSelection::All)
            .expect("selection")
            .to_json()
            .expect("export")
    );
}

#[tokio::test]
async fn imported_external_components_must_be_supplied_and_execute_in_a_fresh_graph() {
    let desired = graph()
        .snapshot()
        .select(GraphSelection::All)
        .expect("selection");
    let json = desired.to_json().expect("export");
    let desired = DesiredTopology::from_json(&json).expect("validated import");
    assert!(
        desired.build(TopologyBindings::default()).is_err(),
        "no imaginary reconstruction from descriptors"
    );
    let received = Received::default();
    let mut bindings = TopologyBindings::default();
    bindings.components.insert(
        "source".into(),
        ConstructedComponent::source(Box::new(FiniteSource::new(
            "source",
            vec![output(root("source", 1, &[5]))],
        ))),
    );
    bindings.components.insert(
        "sink".into(),
        ConstructedComponent::sink(Box::new(CollectSink::new(
            "sink",
            &["in"],
            received.clone(),
        ))),
    );
    let mut imported = desired.build(bindings).expect("explicit external bindings");
    assert_eq!(
        imported.observed().components[&component("source")].realization,
        RealizationState::Pending
    );
    imported
        .start()
        .expect("scope")
        .await
        .expect("fresh execution");
    assert_eq!(values(&received.lock().expect("received")[0].envelope), [5]);
}

#[test]
fn exact_dependency_dependent_and_all_selections_preserve_boundary_relationships() {
    let graph = graph();
    let exact = graph
        .snapshot()
        .select(GraphSelection::Exact(vec![component("source")]))
        .expect("exact");
    assert_eq!(exact.components.len(), 1);
    assert_eq!(exact.boundary_relationships.len(), 1);
    assert!(exact.relationships.is_empty());
    assert!(exact.build(TopologyBindings::default()).is_err());
    for selection in [
        GraphSelection::Dependencies(vec![component("sink")]),
        GraphSelection::Dependents(vec![component("source")]),
        GraphSelection::All,
    ] {
        let selected = graph.snapshot().select(selection).expect("closure");
        assert_eq!(selected.components.len(), 2);
        assert_eq!(selected.relationships.len(), 1);
        assert!(selected.boundary_relationships.is_empty());
        assert_eq!(selected.revision, GraphRevision(1));
    }
}

#[test]
fn topology_deserialization_cannot_bypass_identifier_schema_or_duplicate_port_validation() {
    let json = graph()
        .snapshot()
        .select(GraphSelection::All)
        .expect("selection")
        .to_json()
        .expect("export");
    for case in ["id", "schema-version", "duplicate-port", "operational-field"] {
        let mut value: serde_json::Value = serde_json::from_str(&json).expect("json");
        match case {
            "id" => value["components"][0]["descriptor"]["id"] = serde_json::json!("invalid id"),
            "schema-version" => {
                value["components"][0]["descriptor"]["ports"][0]["schema"]["version"] =
                    serde_json::json!(0)
            }
            "duplicate-port" => {
                let port = value["components"][0]["descriptor"]["ports"][0].clone();
                value["components"][0]["descriptor"]["ports"]
                    .as_array_mut()
                    .expect("ports")
                    .push(port);
            }
            "operational-field" => value["observed"] = serde_json::json!({"status":"Running"}),
            _ => unreachable!(),
        }
        assert!(
            DesiredTopology::from_json(&value.to_string()).is_err(),
            "{case}"
        );
    }
}
