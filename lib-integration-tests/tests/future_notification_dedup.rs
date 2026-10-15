use std::time::Duration;

use drasi_lib::channels::{ComponentStatus, ResultDiff};
use drasi_lib::{DrasiLib, Query};
use drasi_reaction_application::ApplicationReactionBuilder;
use drasi_source_application::{ApplicationSource, ApplicationSourceConfig, PropertyMapBuilder};
use tokio::time::timeout;

async fn wait_for_running(drasi: &DrasiLib, component_id: &str) {
    let mut events = drasi.subscribe_all_component_events();
    if drasi
        .get_graph()
        .await
        .nodes
        .iter()
        .any(|node| node.id == component_id && node.status == ComponentStatus::Running)
    {
        return;
    }
    timeout(Duration::from_secs(5), async {
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
    for source_id in ["ordinary-source", "__future_queue__"] {
        let (source, source_handle) = ApplicationSource::new(
            source_id,
            ApplicationSourceConfig {
                properties: Default::default(),
                durability: None,
            },
        )
        .unwrap();
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
            .from_source(source_id)
            .auto_start(true)
            .enable_bootstrap(false)
            .build();
        let drasi = DrasiLib::builder()
            .with_id(format!("future-dedup-{source_id}"))
            .with_source(source)
            .with_query(query)
            .with_reaction(reaction)
            .build()
            .await
            .unwrap();
        drasi.start().await.unwrap();
        for component_id in [source_id, "deadlines", "results"] {
            wait_for_running(&drasi, component_id).await;
        }
        let mut subscription = reaction_handle
            .subscribe_with_options(Default::default())
            .await
            .unwrap();

        for index in 0..129 {
            source_handle
                .send_node_insert(
                    format!("warmup-{index}"),
                    vec!["Item"],
                    PropertyMapBuilder::new()
                        .with_string("name", "warmup")
                        .with_integer("deadline", 0)
                        .build(),
                )
                .await
                .unwrap();
            let result = timeout(Duration::from_secs(5), subscription.recv())
                .await
                .unwrap()
                .unwrap();
            assert!(matches!(&result.results[0], ResultDiff::Add { data, .. }
                if data.get("due").and_then(|value| value.as_bool()) == Some(true)));
        }

        let deadline = chrono::Utc::now().timestamp_millis() + 1000;
        source_handle
            .send_node_insert(
                "scheduled",
                vec!["Item"],
                PropertyMapBuilder::new()
                    .with_string("name", "scheduled")
                    .with_integer("deadline", deadline)
                    .build(),
            )
            .await
            .unwrap();
        let initial = timeout(Duration::from_secs(5), subscription.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(&initial.results[0], ResultDiff::Add { data, .. }
            if data.get("name").and_then(|value| value.as_str()) == Some("scheduled")
                && data.get("due").and_then(|value| value.as_bool()) == Some(false)));

        let due_result = timeout(Duration::from_secs(3), subscription.recv()).await;
        drasi.stop().await.unwrap();
        let due_result = due_result
            .unwrap_or_else(|_| panic!("future notification suppressed for source '{source_id}'"))
            .unwrap();
        assert!(due_result.results.iter().any(|diff| matches!(diff,
            ResultDiff::Update { after, .. }
                if after.get("name").and_then(|value| value.as_str()) == Some("scheduled")
                    && after.get("due").and_then(|value| value.as_bool()) == Some(true)
        )));
    }
}
