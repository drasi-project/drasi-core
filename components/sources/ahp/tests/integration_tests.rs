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

//! End-to-end tests: AHP source + bootstrap provider + continuous queries
//! against the in-process mock AHP host (real WebSocket, real AHP wire
//! format, official `ahp` reducers).

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use drasi_ahp_common::ahp_types::actions::StateAction;
use drasi_ahp_common::ahp_types::state::{AutomationState, ChangesSummary, SessionState};
use drasi_ahp_common::mock::MockAhpHost;
use drasi_bootstrap_ahp::AhpBootstrapProvider;
use drasi_lib::{DrasiLib, Query};
use drasi_source_ahp::AhpSource;
use serde_json::{json, Value};

const SOURCE_ID: &str = "agents";

fn session_state(title: &str, status: u32) -> SessionState {
    let id = title.to_lowercase().replace(' ', "-");
    serde_json::from_value(json!({
        "provider": "copilot",
        "title": title,
        "status": status,
        "lifecycle": "ready",
        "activeClients": [],
        "chats": [{
            "resource": format!("ahp-chat:/{id}"),
            "title": "Main",
            "status": status,
            "modifiedAt": "2026-01-01T00:00:00.000Z"
        }],
        "defaultChat": format!("ahp-chat:/{id}")
    }))
    .expect("valid session state")
}

fn action(value: Value) -> StateAction {
    serde_json::from_value(value).expect("valid action")
}

async fn start_host() -> Result<MockAhpHost> {
    let host = MockAhpHost::start().await?;
    host.set_agents(vec![serde_json::from_value(json!({
        "provider": "copilot",
        "displayName": "GitHub Copilot",
        "description": "Coding agent",
        "models": []
    }))?])?;
    host.add_session("ahp-session:/s1", session_state("Fix login", 1));
    host.add_session("ahp-session:/s2", session_state("Write docs", 8));
    Ok(host)
}

async fn build_core(host: &MockAhpHost, queries: &[(&str, &str)]) -> Result<Arc<DrasiLib>> {
    let bootstrap = AhpBootstrapProvider::builder()
        .with_url(host.url())
        .build()?;
    let source = AhpSource::builder(SOURCE_ID)
        .with_url(host.url())
        .with_keepalive_interval(Duration::from_millis(500))
        .with_reconnect_delay(Duration::from_millis(100), Duration::from_millis(500))
        .with_bootstrap_provider(bootstrap)
        .build()?;
    let mut builder = DrasiLib::builder().with_id("ahp-test").with_source(source);
    for (id, cypher) in queries {
        builder = builder.with_query(
            Query::cypher(*id)
                .query(*cypher)
                .from_source(SOURCE_ID)
                .auto_start(true)
                .enable_bootstrap(true)
                .build(),
        );
    }
    let core = Arc::new(builder.build().await?);
    core.start().await?;
    Ok(core)
}

async fn wait_for(
    core: &DrasiLib,
    query_id: &str,
    predicate: impl Fn(&[Value]) -> bool,
) -> Result<Vec<Value>> {
    let start = Instant::now();
    loop {
        if let Ok(results) = core.get_query_results(query_id).await {
            if predicate(&results) {
                return Ok(results);
            }
            if start.elapsed() > Duration::from_secs(20) {
                anyhow::bail!("timed out waiting on '{query_id}'; latest results: {results:?}");
            }
        } else if start.elapsed() > Duration::from_secs(20) {
            anyhow::bail!("timed out waiting on '{query_id}'");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

const SESSIONS_QUERY: &str = "MATCH (s:Session)-[:USES_AGENT]->(a:Agent) \
     RETURN s.id AS id, s.title AS title, s.state AS state, a.displayName AS agent";
const INPUT_QUERY: &str =
    "MATCH (s:Session)-[:NEEDS_INPUT]->(r:InputRequest)-[:IN_CHAT]->(c:Chat) \
     RETURN s.title AS session, r.message AS message, c.title AS chat";

#[tokio::test(flavor = "multi_thread")]
async fn bootstraps_and_streams_session_changes() -> Result<()> {
    let host = start_host().await?;
    let core = build_core(
        &host,
        &[("sessions", SESSIONS_QUERY), ("input", INPUT_QUERY)],
    )
    .await?;

    let results = wait_for(&core, "sessions", |r| r.len() == 2).await?;
    assert!(results
        .iter()
        .all(|r| r["agent"] == json!("GitHub Copilot")));
    wait_for(&core, "input", |r| r.is_empty()).await?;

    // Agent asks for input -> appears as InputRequest.
    host.dispatch(
        "ahp-session:/s2",
        action(json!({
            "type": "session/inputNeededSet",
            "request": {
                "kind": "chatInput", "id": "q1", "chat": "ahp-chat:/write-docs",
                "request": {"id": "q1", "message": "Which audience?"}
            }
        })),
    )?;
    let results = wait_for(&core, "input", |r| r.len() == 1).await?;
    assert_eq!(results[0]["message"], json!("Which audience?"));
    assert_eq!(results[0]["chat"], json!("Main"));

    // Answered -> removed.
    host.dispatch(
        "ahp-session:/s2",
        action(json!({"type": "session/inputNeededRemoved", "id": "q1"})),
    )?;
    wait_for(&core, "input", |r| r.is_empty()).await?;

    // Title change flows through the session reducer.
    host.dispatch(
        "ahp-session:/s1",
        action(json!({"type": "session/titleChanged", "title": "Fix login flow"})),
    )?;
    wait_for(&core, "sessions", |r| {
        r.iter().any(|x| x["title"] == json!("Fix login flow"))
    })
    .await?;

    // Catalogue: new session added, then one removed.
    host.add_session("ahp-session:/s3", session_state("Refactor", 1));
    wait_for(&core, "sessions", |r| r.len() == 3).await?;
    host.remove_session("ahp-session:/s1");
    let results = wait_for(&core, "sessions", |r| r.len() == 2).await?;
    assert!(results.iter().all(|r| r["id"] != json!("s1")));

    core.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn summary_changes_drive_file_change_queries() -> Result<()> {
    let host = start_host().await?;
    let core = build_core(
        &host,
        &[
            (
                "big",
                "MATCH (s:Session) WHERE s.filesChanged > 10 \
                 RETURN s.title AS title, s.filesChanged AS files",
            ),
            (
                "failed",
                "MATCH (s:Session) WHERE s.hasError = true RETURN s.title AS title",
            ),
        ],
    )
    .await?;
    wait_for(&core, "big", |r| r.is_empty()).await?;
    wait_for(&core, "failed", |r| r.is_empty()).await?;

    host.set_session_status("ahp-session:/s2", 2);
    let results = wait_for(&core, "failed", |r| r.len() == 1).await?;
    assert_eq!(results[0]["title"], json!("Write docs"));

    host.set_session_changes(
        "ahp-session:/s1",
        ChangesSummary {
            additions: Some(400),
            deletions: Some(20),
            files: Some(25),
        },
    );
    let results = wait_for(&core, "big", |r| r.len() == 1).await?;
    assert_eq!(results[0]["files"], json!(25));
    core.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn resynchronises_after_connection_loss() -> Result<()> {
    let host = start_host().await?;
    let core = build_core(&host, &[("sessions", SESSIONS_QUERY)]).await?;
    wait_for(&core, "sessions", |r| r.len() == 2).await?;
    // Wait until the streaming connection is up alongside the closed bootstrap one.
    let start = Instant::now();
    while host.connection_count() == 0 && start.elapsed() < Duration::from_secs(10) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    host.disconnect_all();
    // Changes made while the source is disconnected are not pushed to it.
    host.remove_session("ahp-session:/s2");
    host.add_session("ahp-session:/s4", session_state("Offline change", 2));

    let results = wait_for(&core, "sessions", |r| {
        r.len() == 2
            && r.iter().any(|x| x["id"] == json!("s4"))
            && r.iter().all(|x| x["id"] != json!("s2"))
    })
    .await?;
    let s4 = results
        .iter()
        .find(|x| x["id"] == json!("s4"))
        .expect("s4 present");
    assert_eq!(s4["state"], json!("error"));
    core.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn mirrors_automation_runs() -> Result<()> {
    let host = start_host().await?;
    let entry = |status: Value| {
        json!({
            "resource": "ahp-automation:/nightly",
            "definition": {
                "title": "Nightly triage",
                "message": {"text": "Triage new issues", "origin": {"kind": "user"}},
                "session": {},
                "enabled": true,
                "triggers": []
            },
            "runs": [{
                "resource": "ahp-automation-run:/r1",
                "automation": "ahp-automation:/nightly",
                "origin": {"kind": "manual"},
                "lifecycle": status,
                "sessionCount": 1
            }],
            "operations": [],
            "createdAt": "2026-01-01T00:00:00.000Z",
            "modifiedAt": "2026-01-01T00:00:00.000Z"
        })
    };
    let catalogue: AutomationState = serde_json::from_value(json!({
        "entries": [entry(json!({
            "status": "running",
            "createdAt": "2026-01-01T00:00:00.000Z",
            "startedAt": "2026-01-01T00:00:01.000Z"
        }))]
    }))?;
    host.enable_automations(catalogue);

    let core = build_core(
        &host,
        &[(
            "runs",
            "MATCH (a:Automation)-[:HAS_RUN]->(r:AutomationRun) \
             RETURN a.title AS automation, r.status AS status",
        )],
    )
    .await?;
    let results = wait_for(&core, "runs", |r| r.len() == 1).await?;
    assert_eq!(results[0]["status"], json!("running"));

    host.dispatch(
        "ahp-automations://",
        action(json!({
            "type": "automation/set",
            "automation": entry(json!({
                "status": "failed",
                "createdAt": "2026-01-01T00:00:00.000Z",
                "completedAt": "2026-01-01T00:05:00.000Z",
                "error": {"errorType": "timeout", "message": "Run timed out"}
            }))
        })),
    )?;
    wait_for(&core, "runs", |r| {
        r.len() == 1 && r[0]["status"] == json!("failed")
    })
    .await?;
    core.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn emits_initial_snapshot_without_bootstrap_when_not_waiting() -> Result<()> {
    let host = start_host().await?;
    let source = AhpSource::builder(SOURCE_ID)
        .with_url(host.url())
        .with_startup_sync_timeout(Duration::ZERO)
        .build()?;
    let core = Arc::new(
        DrasiLib::builder()
            .with_id("ahp-nobootstrap-test")
            .with_source(source)
            .with_query(
                Query::cypher("sessions")
                    .query(SESSIONS_QUERY)
                    .from_source(SOURCE_ID)
                    .auto_start(true)
                    .build(),
            )
            .build()
            .await?,
    );
    core.start().await?;
    wait_for(&core, "sessions", |r| r.len() == 2).await?;
    core.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn rejected_bearer_token_fails_start() -> Result<()> {
    let host = start_host().await?;
    host.require_bearer_token(Some("right".into()));

    let wrong = AhpSource::builder("wrong")
        .with_url(host.url())
        .with_bearer_token("wrong")
        .build()?;
    let err = drasi_lib::Source::start(&wrong)
        .await
        .expect_err("must fail");
    assert!(format!("{err:#}").contains("rejected"), "{err:#}");
    assert_eq!(
        drasi_lib::Source::status(&wrong).await,
        drasi_lib::channels::ComponentStatus::Error
    );

    let right = AhpSource::builder("right")
        .with_url(host.url())
        .with_bearer_token("right")
        .build()?;
    drasi_lib::Source::start(&right).await?;
    assert_eq!(
        drasi_lib::Source::status(&right).await,
        drasi_lib::channels::ComponentStatus::Running
    );
    drasi_lib::Source::stop(&right).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn excludes_archived_sessions_when_configured() -> Result<()> {
    let host = start_host().await?;
    let source = AhpSource::builder(SOURCE_ID)
        .with_url(host.url())
        .with_include_archived(false)
        .with_bootstrap_provider(
            AhpBootstrapProvider::builder()
                .with_url(host.url())
                .with_include_archived(false)
                .build()?,
        )
        .build()?;
    let core = Arc::new(
        DrasiLib::builder()
            .with_id("ahp-archive-test")
            .with_source(source)
            .with_query(
                Query::cypher("active")
                    .query("MATCH (s:Session) RETURN s.id AS id")
                    .from_source(SOURCE_ID)
                    .auto_start(true)
                    .enable_bootstrap(true)
                    .build(),
            )
            .build()
            .await?,
    );
    core.start().await?;
    wait_for(&core, "active", |r| r.len() == 2).await?;

    host.dispatch(
        "ahp-session:/s1",
        action(json!({"type": "session/isArchivedChanged", "isArchived": true})),
    )?;
    let results = wait_for(&core, "active", |r| r.len() == 1).await?;
    assert_eq!(results[0]["id"], json!("s2"));
    core.stop().await?;
    Ok(())
}
