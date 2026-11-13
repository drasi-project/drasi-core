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

use anyhow::{anyhow, Result};
use drasi_lib::channels::{ComponentStatus, QueryResult};
use drasi_reaction_application::ApplicationReaction;
use shared_tests::recovery_test_helpers::*;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::timeout;

async fn recv_result(rx: &mut mpsc::Receiver<QueryResult>) -> Result<QueryResult> {
    timeout(Duration::from_secs(5), rx.recv())
        .await
        .map_err(|_| anyhow!("Timed out waiting for application result"))?
        .ok_or_else(|| anyhow!("Application receiver closed"))
}

async fn recv_results(
    rx: &mut mpsc::Receiver<QueryResult>,
    count: usize,
) -> Result<Vec<QueryResult>> {
    let mut results = Vec::with_capacity(count);
    for _ in 0..count {
        results.push(recv_result(rx).await?);
    }
    Ok(results)
}

async fn recv_until_sequence_at_least(
    rx: &mut mpsc::Receiver<QueryResult>,
    minimum_sequence: u64,
    max_results: usize,
) -> Result<Vec<QueryResult>> {
    let mut results = Vec::new();
    while results.len() < max_results {
        let result = recv_result(rx).await?;
        let sequence = result.sequence;
        results.push(result);
        if sequence >= minimum_sequence {
            break;
        }
    }
    Ok(results)
}

#[tokio::test]
async fn test_application_reaction_strict_recovery_clean_restart() -> Result<()> {
    let reaction_id = drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-0000000003ef")
        .expect("fixture UUID");
    let (reaction, handle) = ApplicationReaction::new(
        reaction_id,
        vec![drasi_lib::config::ComponentReference::by_name(
            drasi_lib::ComponentName::try_new("q1").expect("fixture component name"),
        )],
    );
    let mut app_rx = handle
        .take_receiver()
        .await
        .ok_or_else(|| anyhow!("Application receiver already taken"))?;

    let (core, source_handle, _state_store) = build_core_with_reaction(
        "app-clean-recovery",
        person_query(
            &drasi_lib::ComponentName::try_new("app-clean-recovery-source")
                .expect("fixture component name"),
            100,
        ),
        &drasi_lib::ComponentName::try_new("app-clean").expect("fixture component name"),
        reaction,
    )
    .await?;
    let reaction_component_id = core
        .component_by_name(
            &drasi_lib::ComponentName::try_new("app-clean").expect("fixture component name"),
        )?
        .id()
        .clone();
    let query_id = core
        .component_by_name(&drasi_lib::ComponentName::try_new("q1")?)?
        .id()
        .clone();
    let mut event_rx = core.subscribe_all_component_events();

    core.start().await?;
    wait_for_component_status(
        &mut event_rx,
        &reaction_component_id,
        ComponentStatus::Running,
        Duration::from_secs(5),
    )
    .await?;

    insert_person(&source_handle, "p1", "Alice", 30).await?;
    insert_person(&source_handle, "p2", "Bob", 31).await?;
    let initial = recv_results(&mut app_rx, 2).await?;
    assert_eq!(initial.len(), 2);
    assert!(initial.iter().all(|result| result.query_id == query_id));

    stop_reaction_and_wait(&core, &reaction_component_id).await?;
    core.start_reaction_by_name(
        &drasi_lib::ComponentName::try_new("app-clean").expect("fixture component name"),
    )
    .await?;
    wait_for_component_status(
        &mut event_rx,
        &reaction_component_id,
        ComponentStatus::Running,
        Duration::from_secs(5),
    )
    .await?;

    insert_person(&source_handle, "p3", "Charlie", 32).await?;
    let initial_max_sequence = initial.last().unwrap().sequence;
    let after_restart =
        recv_until_sequence_at_least(&mut app_rx, initial_max_sequence + 1, 4).await?;
    assert!(
        after_restart
            .iter()
            .any(|result| result.sequence > initial_max_sequence),
        "Restarted application reaction should resume live delivery"
    );
    assert!(after_restart
        .iter()
        .all(|result| result.query_id == query_id));

    core.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn test_application_reaction_strict_recovery_outbox_catchup() -> Result<()> {
    let reaction_id = drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-0000000003f0")
        .expect("fixture UUID");
    let (reaction, handle) = ApplicationReaction::new(
        reaction_id,
        vec![drasi_lib::config::ComponentReference::by_name(
            drasi_lib::ComponentName::try_new("q1").expect("fixture component name"),
        )],
    );
    let mut app_rx = handle
        .take_receiver()
        .await
        .ok_or_else(|| anyhow!("Application receiver already taken"))?;

    let (core, source_handle, _state_store) = build_core_with_reaction(
        "app-outbox-catchup",
        person_query(
            &drasi_lib::ComponentName::try_new("app-outbox-catchup-source")
                .expect("fixture component name"),
            100,
        ),
        &drasi_lib::ComponentName::try_new("app-catchup").expect("fixture component name"),
        reaction,
    )
    .await?;
    let reaction_component_id = core
        .component_by_name(
            &drasi_lib::ComponentName::try_new("app-catchup").expect("fixture component name"),
        )?
        .id()
        .clone();
    let query_id = core
        .component_by_name(&drasi_lib::ComponentName::try_new("q1")?)?
        .id()
        .clone();
    let mut event_rx = core.subscribe_all_component_events();

    core.start().await?;
    wait_for_component_status(
        &mut event_rx,
        &reaction_component_id,
        ComponentStatus::Running,
        Duration::from_secs(5),
    )
    .await?;

    insert_person(&source_handle, "p1", "Alice", 30).await?;
    let first = recv_result(&mut app_rx).await?;
    assert_eq!(first.query_id, query_id);

    stop_reaction_and_wait(&core, &reaction_component_id).await?;

    insert_person(&source_handle, "p2", "Bob", 31).await?;
    insert_person(&source_handle, "p3", "Charlie", 32).await?;
    insert_person(&source_handle, "p4", "Diana", 33).await?;

    core.start_reaction_by_name(
        &drasi_lib::ComponentName::try_new("app-catchup").expect("fixture component name"),
    )
    .await?;
    wait_for_component_status(
        &mut event_rx,
        &reaction_component_id,
        ComponentStatus::Running,
        Duration::from_secs(5),
    )
    .await?;

    let replayed = recv_until_sequence_at_least(&mut app_rx, first.sequence + 3, 5).await?;
    assert!(
        replayed.len() >= 3,
        "Expected replayed application results after restart"
    );
    assert!(replayed.iter().all(|result| result.query_id == query_id));
    let replay_sequences: Vec<u64> = replayed.iter().map(|result| result.sequence).collect();
    assert!(
        replay_sequences
            .windows(2)
            .all(|window| window[0] < window[1]),
        "Replayed application results should preserve order: {replay_sequences:?}"
    );
    assert!(
        replay_sequences.last().copied().unwrap_or_default() >= first.sequence + 3,
        "Replay should catch up through events produced while the reaction was stopped"
    );

    core.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn test_application_reaction_sequence_preserved() -> Result<()> {
    let reaction_id = drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-0000000003f1")
        .expect("fixture UUID");
    let (reaction, handle) = ApplicationReaction::new(
        reaction_id,
        vec![drasi_lib::config::ComponentReference::by_name(
            drasi_lib::ComponentName::try_new("q1").expect("fixture component name"),
        )],
    );
    let mut app_rx = handle
        .take_receiver()
        .await
        .ok_or_else(|| anyhow!("Application receiver already taken"))?;

    let (core, source_handle, _state_store) = build_core_with_reaction(
        "app-sequence-order",
        person_query(
            &drasi_lib::ComponentName::try_new("app-sequence-order-source")
                .expect("fixture component name"),
            100,
        ),
        &drasi_lib::ComponentName::try_new("app-sequence").expect("fixture component name"),
        reaction,
    )
    .await?;
    let reaction_component_id = core
        .component_by_name(
            &drasi_lib::ComponentName::try_new("app-sequence").expect("fixture component name"),
        )?
        .id()
        .clone();
    let mut event_rx = core.subscribe_all_component_events();

    core.start().await?;
    wait_for_component_status(
        &mut event_rx,
        &reaction_component_id,
        ComponentStatus::Running,
        Duration::from_secs(5),
    )
    .await?;

    insert_person(&source_handle, "p1", "Alice", 30).await?;
    insert_person(&source_handle, "p2", "Bob", 31).await?;
    insert_person(&source_handle, "p3", "Charlie", 32).await?;
    insert_person(&source_handle, "p4", "Diana", 33).await?;
    insert_person(&source_handle, "p5", "Eve", 34).await?;
    let results = recv_results(&mut app_rx, 5).await?;

    let sequences: Vec<u64> = results.iter().map(|result| result.sequence).collect();
    assert!(
        sequences.windows(2).all(|window| window[0] < window[1]),
        "Application results should preserve monotonically increasing sequence numbers: {sequences:?}"
    );

    core.shutdown().await?;
    Ok(())
}
