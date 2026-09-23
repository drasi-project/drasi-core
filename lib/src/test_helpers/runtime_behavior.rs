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

use super::{managers, wait_for_component_status};
use crate::{
    channels::{ComponentStatus, QueryResult, ResultDiff},
    queries::Query,
    sources::tests::TestMockSource,
    DrasiLib,
};
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
};
use futures::StreamExt;
use serde_json::json;
use std::{collections::HashMap, sync::Arc, time::Duration};

async fn pipeline(text: &str, capacity: usize) -> (Arc<DrasiLib>, Arc<dyn Query>) {
    let core = Arc::new(
        DrasiLib::builder()
            .with_id("runtime-parity")
            .with_source(TestMockSource::new("source".into()).unwrap())
            .with_query(
                crate::Query::cypher("query")
                    .query(text)
                    .from_source("source")
                    .enable_bootstrap(false)
                    .with_dispatch_buffer_capacity(capacity)
                    .build(),
            )
            .build()
            .await
            .unwrap(),
    );
    let mut events = core.subscribe_all_component_events();
    core.start().await.unwrap();
    wait_for_component_status(
        &mut events,
        "query",
        ComponentStatus::Running,
        Duration::from_secs(5),
    )
    .await;
    let query = managers::QueryManager(core.clone())
        .get_query_instance("query")
        .await
        .unwrap();

    {
        use crate::computation::{runtime::QueryInstance, v1::*};
        let native = query.as_any().downcast_ref::<QueryInstance>().unwrap();
        let snapshot = native.inspector().snapshot();
        let id = ComponentId::try_new("query").unwrap();
        let specification = &snapshot.desired.specifications[&id];
        assert_eq!(specification.role, ComponentRole::Query);
        assert_eq!(
            specification.implementation.name.as_ref(),
            "drasi/continuous-query"
        );
        assert_eq!(
            snapshot.observed.components[&id].realization,
            RealizationState::Created
        );
        assert_eq!(
            snapshot.observed.components[&id].lifecycle,
            ComponentLifecycle::Running
        );
    }
    (core, query)
}

async fn insert(core: &DrasiLib, id: &str, name: &str) {
    let source = core.source_instance("source").await.unwrap();
    source
        .as_any()
        .downcast_ref::<TestMockSource>()
        .unwrap()
        .inject_event(SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("source", id),
                    labels: vec!["Person".into()].into(),
                    effective_from: 1_000,
                },
                properties: ElementPropertyMap::from(json!({ "name": name })),
            },
        })
        .await
        .unwrap();
}

async fn receive(
    receiver: &mut dyn crate::channels::ChangeReceiver<QueryResult>,
) -> Arc<QueryResult> {
    tokio::time::timeout(Duration::from_secs(5), receiver.recv())
        .await
        .expect("subscriber must make progress")
        .unwrap()
}

#[tokio::test]
async fn native_runtime_preserves_ordered_diffs_metadata_snapshots_and_outbox() {
    use std::collections::BTreeMap;

    fn normalized(result: &QueryResult) -> serde_json::Value {
        let mut value = serde_json::to_value(result).unwrap();
        value["timestamp"] = json!("<wall-clock>");
        if let Some(profile) = value
            .get_mut("profiling")
            .and_then(serde_json::Value::as_object_mut)
        {
            for (field, stamp) in profile {
                assert!(
                    field.ends_with("_ns"),
                    "unknown profiling field requires an explicit comparison policy: {field}"
                );
                if !stamp.is_null() {
                    assert!(stamp.is_u64(), "profiling clock must remain a timestamp");
                    *stamp = json!("<clock>");
                }
            }
        }
        value
    }

    fn person(id: &str, name: &str, time: u64) -> Element {
        Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("source", id),
                labels: vec!["Person".into()].into(),
                effective_from: time,
            },
            properties: ElementPropertyMap::from(json!({"name": name})),
        }
    }

    async fn trace() -> serde_json::Value {
        let (core, query) = pipeline("MATCH (n:Person) RETURN n.name AS name", 8).await;
        let mut output = query.subscribe("capture".into()).await.unwrap().receiver;
        let source = core.source_instance("source").await.unwrap();
        let source = source.as_any().downcast_ref::<TestMockSource>().unwrap();
        let changes = [
            SourceChange::Insert {
                element: person("one", "same", 1000),
            },
            SourceChange::Insert {
                element: person("two", "same", 1001),
            },
            SourceChange::Update {
                element: person("one", "changed", 1002),
            },
            SourceChange::Delete {
                metadata: person("two", "same", 1003).get_metadata().clone(),
            },
            SourceChange::Delete {
                metadata: person("one", "changed", 1004).get_metadata().clone(),
            },
        ];
        let mut emissions = Vec::new();
        let mut snapshots = Vec::new();
        let mut identities = Vec::new();
        for (index, change) in changes.into_iter().enumerate() {
            source.inject_event(change).await.unwrap();
            let result = receive(output.as_mut()).await;
            assert_eq!(result.sequence, index as u64 + 1);
            assert_eq!(result.query_id, "query");
            assert_eq!(result.results.len(), 1);
            if index < 2 {
                let ResultDiff::Add { row_signature, .. } = result.results[0] else {
                    panic!("inserting a matching row must produce Add");
                };
                identities.push(row_signature);
                if index == 1 {
                    assert_ne!(
                        identities[0], identities[1],
                        "equal values are distinct rows"
                    );
                }
            }
            let expected = match index {
                0 | 1 => ResultDiff::Add {
                    data: json!({"name": "same"}),
                    row_signature: identities[index],
                },
                2 => ResultDiff::Update {
                    data: json!({"name": "changed"}),
                    before: json!({"name": "same"}),
                    after: json!({"name": "changed"}),
                    grouping_keys: None,
                    row_signature: identities[0],
                },
                3 => ResultDiff::Delete {
                    data: json!({"name": "same"}),
                    row_signature: identities[1],
                },
                4 => ResultDiff::Delete {
                    data: json!({"name": "changed"}),
                    row_signature: identities[0],
                },
                _ => unreachable!(),
            };
            assert_eq!(result.results, vec![expected], "input {index}");
            assert_eq!(
                result.metadata,
                HashMap::from([
                    ("source_id".into(), json!("source")),
                    ("processed_by".into(), json!("drasi-core")),
                    ("result_count".into(), json!(1)),
                ])
            );
            let profile = result.profiling.as_ref().expect("query profiling");
            assert!(profile.query_receive_ns.is_some());
            assert!(profile.query_core_call_ns.is_some());
            assert!(profile.query_core_return_ns.is_some());
            assert!(profile.query_send_ns.is_some());
            emissions.push(normalized(&result));
            let snapshot = query.fetch_snapshot().await.unwrap();
            assert_eq!(snapshot.as_of_sequence, result.sequence);
            let rows: BTreeMap<_, _> = snapshot.stream_keyed().collect().await;
            let mut expected_rows = BTreeMap::new();
            if index < 4 {
                expected_rows.insert(
                    identities[0],
                    json!({"name": if index < 2 { "same" } else { "changed" }}),
                );
            }
            if (1..=2).contains(&index) {
                expected_rows.insert(identities[1], json!({"name": "same"}));
            }
            assert_eq!(rows, expected_rows, "snapshot after input {index}");
            snapshots.push(serde_json::to_value(rows).unwrap());
        }
        let outbox = query.fetch_outbox(0).await.unwrap();
        let replay: Vec<_> = outbox
            .results
            .iter()
            .map(|result| normalized(result))
            .collect();
        assert_eq!(replay, emissions);
        core.shutdown().await.unwrap();
        json!({"emissions": emissions, "snapshots": snapshots, "outbox": replay})
    }

    let first = trace().await;
    let rebuilt = trace().await;
    assert_eq!(rebuilt, first);
}

async fn bounded_fanout() {
    let (core, query) = pipeline("MATCH (n:Person) RETURN n.name AS name", 1).await;
    let mut slow = query.subscribe("slow".into()).await.unwrap().receiver;
    let mut fast = query.subscribe("fast".into()).await.unwrap().receiver;
    insert(&core, "1", "Alice").await;
    let first = receive(fast.as_mut()).await;
    insert(&core, "2", "Bob").await;
    let slow_first = receive(slow.as_mut()).await;
    let second = receive(fast.as_mut()).await;
    assert_eq!(first.sequence, 1);
    assert_eq!(second.sequence, 2);
    for result in [&first, &second] {
        assert_eq!(result.query_id, "query");
        assert_eq!(result.results.len(), 1);
        assert_eq!(
            result.metadata,
            HashMap::from([
                ("source_id".into(), json!("source")),
                ("processed_by".into(), json!("drasi-core")),
                ("result_count".into(), json!(1)),
            ])
        );
        let profiling = result.profiling.as_ref().expect("query profiling");
        assert!(profiling.query_receive_ns.is_some());
        assert!(profiling.query_core_call_ns.is_some());
        assert!(profiling.query_core_return_ns.is_some());
        assert!(profiling.query_send_ns.is_some());
    }
    assert_eq!(
        serde_json::to_value(slow_first.as_ref()).unwrap(),
        serde_json::to_value(first.as_ref()).unwrap()
    );
    assert_eq!(
        serde_json::to_value(receive(slow.as_mut()).await.as_ref()).unwrap(),
        serde_json::to_value(second.as_ref()).unwrap()
    );
    core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn bounded_fanout_preserves_complete_results_current_thread() {
    bounded_fanout().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bounded_fanout_preserves_complete_results_multi_thread() {
    bounded_fanout().await;
}

#[tokio::test]
async fn native_slow_subscriber_does_not_block_other_deliveries() {
    let (core, query) = pipeline("MATCH (n:Person) RETURN n.name AS name", 1).await;
    let mut slow = query.subscribe("slow".into()).await.unwrap().receiver;
    let mut fast = query.subscribe("fast".into()).await.unwrap().receiver;
    insert(&core, "1", "Alice").await;
    assert_eq!(receive(fast.as_mut()).await.sequence, 1);
    insert(&core, "2", "Bob").await;
    assert_eq!(receive(fast.as_mut()).await.sequence, 2);
    assert_eq!(receive(slow.as_mut()).await.sequence, 1);
    assert_eq!(receive(slow.as_mut()).await.sequence, 2);
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn scheduled_query_result_preserves_legacy_metadata_and_snapshot() {
    let (core, query) = pipeline(
        "MATCH (n:Person) WHERE drasi.trueLater(true, 2000) RETURN n.name AS name",
        1,
    )
    .await;
    let mut receiver = query.subscribe("output".into()).await.unwrap().receiver;
    let earliest = chrono::Utc::now();
    insert(&core, "1", "Alice").await;
    let result = receive(receiver.as_mut()).await;
    assert_eq!(result.query_id, "query");
    assert_eq!(result.sequence, 1);
    assert_eq!(result.results.len(), 1);
    let ResultDiff::Add {
        data,
        row_signature,
    } = &result.results[0]
    else {
        panic!("expected the scheduled Add result");
    };
    assert_eq!(data, &json!({"name":"Alice"}));
    assert_eq!(
        result.metadata,
        HashMap::from([
            ("source_id".into(), json!("source")),
            ("processed_by".into(), json!("drasi-core")),
            ("result_count".into(), json!(1)),
        ])
    );
    let profile = result.profiling.as_ref().expect("future result profiling");
    assert!(profile.source_receive_ns.is_some());
    assert_eq!(
        profile,
        &crate::profiling::ProfilingMetadata {
            source_receive_ns: profile.source_receive_ns,
            ..Default::default()
        }
    );
    assert!(result.timestamp >= earliest);
    assert!(result.timestamp <= chrono::Utc::now());
    let snapshot = query.fetch_snapshot().await.unwrap();
    assert_eq!(snapshot.as_of_sequence, 1);
    assert_eq!(
        snapshot.stream_keyed().collect::<Vec<_>>().await,
        vec![(*row_signature, data.clone())]
    );
    let outbox = query.fetch_outbox(0).await.unwrap();
    assert_eq!(
        serde_json::to_value(&outbox.results).unwrap(),
        serde_json::to_value(vec![result.as_ref()]).unwrap()
    );
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn partially_started_native_instance_can_stop_its_successful_components() {
    struct FailingSource;
    #[async_trait::async_trait]
    impl crate::Source for FailingSource {
        fn id(&self) -> &str {
            "bad"
        }
        fn type_name(&self) -> &str {
            "failure"
        }
        fn properties(&self) -> HashMap<String, serde_json::Value> {
            HashMap::new()
        }
        async fn initialize(&self, _: crate::SourceRuntimeContext) {}
        async fn start(&self) -> anyhow::Result<()> {
            anyhow::bail!("injected failure")
        }
        async fn stop(&self) -> anyhow::Result<()> {
            Ok(())
        }
        async fn status(&self) -> ComponentStatus {
            ComponentStatus::Stopped
        }
        async fn subscribe(
            &self,
            _: crate::config::SourceSubscriptionSettings,
        ) -> anyhow::Result<crate::channels::SubscriptionResponse> {
            anyhow::bail!("failure source has no subscriptions")
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }
    let core = DrasiLib::builder()
        .with_source(FailingSource)
        .with_source(TestMockSource::new("good".into()).unwrap())
        .with_query(
            crate::Query::cypher("query")
                .query("MATCH (n:Person) RETURN n.name AS name")
                .from_source("good")
                .enable_bootstrap(false)
                .build(),
        )
        .build()
        .await
        .unwrap();
    let mut events = core.subscribe_all_component_events();
    assert!(core.start().await.is_err());
    wait_for_component_status(
        &mut events,
        "query",
        ComponentStatus::Running,
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(
        core.get_source_status("good").await.unwrap(),
        ComponentStatus::Running
    );
    assert!(core.is_running().await);
    core.stop().await.unwrap();
    assert_eq!(
        core.get_source_status("good").await.unwrap(),
        ComponentStatus::Stopped
    );
    assert_eq!(
        core.get_query_status("query").await.unwrap(),
        ComponentStatus::Stopped
    );
    core.shutdown().await.unwrap();
}
