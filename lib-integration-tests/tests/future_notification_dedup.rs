use std::{sync::Arc, time::Duration};

use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
};
use drasi_lib::channels::{ComponentStatus, ResultDiff};
use drasi_lib::{test_support::QueryTestControl, DrasiLib, Query};
use drasi_reaction_application::ApplicationReactionBuilder;
use drasi_source_application::{ApplicationSource, ApplicationSourceConfig};
use serde_json::json;
use tokio::time::timeout;

const INITIAL_TIME: u64 = 1000;
const DUE_TIME: u64 = 2000;
const WAIT: Duration = Duration::from_secs(5);

fn insert(source: &str, id: &str, name: &str, deadline: u64) -> SourceChange {
    SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new(source, id),
                labels: Arc::from([Arc::from("Item")]),
                effective_from: INITIAL_TIME,
            },
            properties: ElementPropertyMap::from(json!({
                "name": name,
                "deadline": deadline,
            })),
        },
    }
}

async fn wait_for_running(drasi: &DrasiLib, component_id: &str) {
    let mut events = drasi.subscribe_all_component_events();
    if drasi
        .get_graph()
        .await
        .expect("component graph snapshot unavailable")
        .nodes
        .iter()
        .any(|node| node.id == component_id && node.status == ComponentStatus::Running)
    {
        return;
    }
    timeout(WAIT, async {
        loop {
            let event = events
                .recv()
                .await
                .expect("failed to receive component event while waiting for startup");
            if event.component_id == component_id && event.status == ComponentStatus::Running {
                break;
            }
        }
    })
    .await
    .expect("component did not start");
}

#[tokio::test]
async fn future_notifications_ignore_data_checkpoints_with_colliding_source_id() {
    run_future_notification_dedup("current-thread").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn future_notifications_ignore_data_checkpoints_with_colliding_source_id_multi_thread() {
    run_future_notification_dedup("multi-thread").await;
}

async fn run_future_notification_dedup(runtime: &str) {
    for source_id in ["ordinary-source", "__future_queue__"] {
        let clock = QueryTestControl::new(INITIAL_TIME);
        let (source, source_handle) = ApplicationSource::new(
            source_id,
            ApplicationSourceConfig {
                properties: Default::default(),
                durability: None,
            },
        )
        .expect("create application source");
        let (reaction, reaction_handle) = ApplicationReactionBuilder::new("results")
            .with_query("deadlines")
            .with_auto_start(true)
            .build();
        let query = Query::cypher("deadlines")
            .query(
                "MATCH (item:Item)
                 RETURN item.name AS name,
                 CASE WHEN drasi.trueLater(
                     datetime.realtime() >= datetime({epochMillis: item.deadline}),
                     datetime({epochMillis: item.deadline})
                 ) THEN true ELSE false END AS due",
            )
            .from_input(source_id)
            .auto_start(true)
            .enable_bootstrap(false)
            .build();
        let drasi = DrasiLib::builder()
            .with_id(format!("future-dedup-{runtime}-{source_id}"))
            .with_source(source)
            .with_query(query)
            .with_query_test_control("deadlines", clock.clone())
            .with_reaction(reaction)
            .build()
            .await
            .expect("build timer fixture");
        drasi.start().await.expect("start timer fixture");
        for component_id in [source_id, "deadlines", "results"] {
            wait_for_running(&drasi, component_id).await;
        }
        let mut subscription = reaction_handle
            .subscribe_with_options(Default::default())
            .await
            .expect("subscribe to reaction results");

        for index in 0..129 {
            source_handle
                .send(insert(source_id, &format!("warmup-{index}"), "warmup", 0))
                .await
                .expect("send warmup input");
            let result = timeout(WAIT, subscription.recv())
                .await
                .expect("warmup result timed out")
                .expect("reaction result stream closed");
            assert!(matches!(&result.results[0], ResultDiff::Add { data, .. }
                if data.get("due").and_then(|value| value.as_bool()) == Some(true)));
        }

        source_handle
            .send(insert(source_id, "scheduled", "scheduled", DUE_TIME))
            .await
            .expect("send scheduled input");
        let initial = timeout(WAIT, subscription.recv())
            .await
            .expect("initial query result timed out")
            .expect("reaction result stream closed");
        assert!(matches!(&initial.results[0], ResultDiff::Add { data, .. }
            if data.get("name").and_then(|value| value.as_str()) == Some("scheduled")
                && data.get("due").and_then(|value| value.as_bool()) == Some(false)));

        let before = clock
            .advance_to(DUE_TIME - 1, WAIT)
            .await
            .expect("drain before the deadline");
        assert_eq!(before.output_sequence, initial.sequence);
        let due = clock
            .advance_to(DUE_TIME, WAIT)
            .await
            .expect("drain at the deadline");
        assert_eq!(due.output_sequence, initial.sequence + 1);
        let due_result = timeout(WAIT, subscription.recv())
            .await
            .unwrap_or_else(|_| panic!("future notification suppressed for source '{source_id}'"))
            .expect("reaction result stream closed");
        assert_eq!(due_result.sequence, due.output_sequence);
        assert!(due_result.results.iter().any(|diff| matches!(diff,
            ResultDiff::Update { after, .. }
                if after.get("name").and_then(|value| value.as_str()) == Some("scheduled")
                    && after.get("due").and_then(|value| value.as_bool()) == Some(true)
        )));
        let stale = clock.wake(WAIT).await.expect("drain a stale wake");
        assert_eq!(stale.output_sequence, due.output_sequence);
        drasi.shutdown().await.expect("shut down timer fixture");
    }
}
