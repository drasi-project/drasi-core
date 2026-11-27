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

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use drasi_lib::channels::{ComponentStatus, QueryResult, ResultDiff};
use drasi_lib::component_graph::ComponentGraph;
use drasi_lib::context::ReactionRuntimeContext;
use drasi_lib::reactions::common::{QueryConfig, TemplateSpec};
use drasi_lib::{MemoryStateStoreProvider, Reaction};
use drasi_plugin_sdk::prelude::ReactionPluginDescriptor;
use serde_json::{json, Value};

use crate::activation::{next_action, Action, Activation, Operation, ResultKey};
use crate::descriptor::AhpReactionDescriptor;
use crate::mock_host::{MockAgentHost, MockTurnState};
use crate::process::{extract_result_key, resolve_query_config, work_id, DiffPayload};
use crate::{AhpMode, AhpReaction, FollowUpKind, OnDelete};

const WAIT: Duration = Duration::from_secs(5);

fn result(query_id: &str, sequence: u64, diffs: Vec<ResultDiff>) -> QueryResult {
    QueryResult::new(
        query_id.to_string(),
        sequence,
        Utc::now(),
        diffs,
        HashMap::new(),
    )
}

fn add(query_id: &str, sequence: u64, row: Value) -> QueryResult {
    result(
        query_id,
        sequence,
        vec![ResultDiff::Add {
            data: row,
            row_signature: 0,
        }],
    )
}

fn update(query_id: &str, sequence: u64, before: Value, after: Value) -> QueryResult {
    result(
        query_id,
        sequence,
        vec![ResultDiff::Update {
            data: json!({}),
            before,
            after,
            grouping_keys: None,
            row_signature: 0,
        }],
    )
}

fn delete(query_id: &str, sequence: u64, row: Value) -> QueryResult {
    result(
        query_id,
        sequence,
        vec![ResultDiff::Delete {
            data: row,
            row_signature: 0,
        }],
    )
}

async fn init(reaction: &AhpReaction, store: Option<Arc<MemoryStateStoreProvider>>) {
    let (graph, _rx) = ComponentGraph::new("ahp-test");
    let store = store.map(|s| s as Arc<dyn drasi_lib::state_store::StateStoreProvider>);
    let context = ReactionRuntimeContext::new(
        "ahp-test",
        reaction.id(),
        store,
        graph.update_sender(),
        None,
    );
    reaction.initialize(context).await;
}

fn base_builder(host: &MockAgentHost) -> crate::AhpReactionBuilder {
    AhpReaction::builder("incident-agent")
        .with_query("incidents")
        .with_endpoint(host.url())
        .with_result_key_fields(["id"])
        .with_timeout_ms(2_000)
}

async fn wait_status(reaction: &AhpReaction, expected: ComponentStatus) {
    let deadline = tokio::time::Instant::now() + WAIT;
    while reaction.status().await != expected {
        assert!(
            tokio::time::Instant::now() < deadline,
            "status never became {expected:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// ── Builder / config ──────────────────────────────────────────────────────

#[test]
fn builder_validates_configuration() {
    let ok = AhpReaction::builder("r")
        .with_query("q")
        .with_endpoint("wss://host.example/ahp")
        .with_result_key_fields(["id"])
        .build();
    assert!(ok.is_ok());

    let cases: Vec<(&str, crate::AhpReactionBuilder)> = vec![
        (
            "scheme",
            AhpReaction::builder("r")
                .with_query("q")
                .with_endpoint("http://host")
                .with_result_key_fields(["id"]),
        ),
        (
            "resultKeyFields",
            AhpReaction::builder("r")
                .with_query("q")
                .with_endpoint("ws://host"),
        ),
        (
            "automation",
            AhpReaction::builder("r")
                .with_query("q")
                .with_endpoint("ws://host")
                .with_result_key_fields(["id"])
                .with_mode(AhpMode::Automation),
        ),
        (
            "template",
            AhpReaction::builder("r")
                .with_query("q")
                .with_endpoint("ws://host")
                .with_result_key_fields(["id"])
                .with_message_template("{{#if}}"),
        ),
        (
            "route",
            AhpReaction::builder("r")
                .with_query("q")
                .with_endpoint("ws://host")
                .with_result_key_fields(["id"])
                .with_route("other", QueryConfig::default()),
        ),
        (
            "authResource",
            AhpReaction::builder("r")
                .with_query("q")
                .with_endpoint("ws://host")
                .with_result_key_fields(["id"])
                .with_auth_resource("res"),
        ),
    ];
    for (needle, builder) in cases {
        let error = builder.build().err().expect("build should fail");
        assert!(
            format!("{error:#}").contains(needle),
            "expected '{needle}' in '{error:#}'"
        );
    }
}

#[tokio::test]
async fn reaction_trait_surface() {
    let reaction = AhpReaction::builder("r1")
        .with_queries(vec!["q1".into(), "q2".into()])
        .with_endpoint("ws://localhost:1")
        .with_token("secret")
        .with_result_key_fields(["id"])
        .with_auto_start(false)
        .build()
        .expect("build");
    assert_eq!(reaction.id(), "r1");
    assert_eq!(reaction.type_name(), "ahp");
    assert_eq!(reaction.query_ids(), vec!["q1", "q2"]);
    assert!(!reaction.auto_start());
    assert!(!reaction.is_durable());
    assert_eq!(reaction.status().await, ComponentStatus::Stopped);
    let props = reaction.properties();
    assert_eq!(props["endpoint"], json!("ws://localhost:1"));
    assert_eq!(props["token"], json!("secret"));
}

// ── Pure logic ────────────────────────────────────────────────────────────

#[test]
fn next_action_covers_every_operation() {
    let session = Activation::Session {
        session: "s".into(),
        chat: "c".into(),
        sequence: 5,
        diff_index: 0,
    };
    let run = Activation::Automation {
        run: "r".into(),
        sequence: 5,
        diff_index: 0,
    };
    use AhpMode::*;
    use Operation::*;
    assert_eq!(next_action(Session, Add, None, 1, 0), Action::StartSession);
    assert_eq!(
        next_action(Automation, Add, None, 1, 0),
        Action::RunAutomation
    );
    assert_eq!(
        next_action(Session, Update, None, 1, 0),
        Action::StartSession
    );
    assert!(matches!(
        next_action(Session, Add, Some(&session), 6, 0),
        Action::Drop { .. }
    ));
    assert_eq!(
        next_action(Session, Update, Some(&session), 6, 0),
        Action::FollowUp {
            session: "s".into(),
            chat: "c".into()
        }
    );
    assert!(matches!(
        next_action(Automation, Update, Some(&run), 6, 0),
        Action::Drop { .. }
    ));
    assert_eq!(
        next_action(Session, Delete, Some(&session), 6, 0),
        Action::Close(session.clone())
    );
    assert!(matches!(
        next_action(Session, Delete, None, 6, 0),
        Action::Drop { .. }
    ));
    // Already applied (replay) is dropped.
    assert!(matches!(
        next_action(Session, Update, Some(&session), 5, 0),
        Action::Drop { .. }
    ));
}

#[test]
fn work_ids_are_deterministic_and_uri_safe() {
    let key = ResultKey("a:b/c d".into());
    let a = work_id("r", "q", &key, 3, 1);
    assert_eq!(a, work_id("r", "q", &key, 3, 1));
    assert_ne!(a, work_id("r", "q", &key, 4, 1));
    assert_ne!(a, work_id("r2", "q", &key, 3, 1));
    assert!(a.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
}

#[test]
fn result_keys_support_nested_and_composite_fields() {
    let payload = DiffPayload::from_result_diff(&ResultDiff::Add {
        data: json!({"a": {"b": 7}, "c": "x&y"}),
        row_signature: 0,
    })
    .expect("payload");
    assert_eq!(
        extract_result_key(&payload, &["a.b".into()])
            .expect("key")
            .0,
        "7"
    );
    assert_eq!(
        extract_result_key(&payload, &["a.b".into(), "c".into()])
            .expect("key")
            .0,
        "a.b=7&c=x%26y"
    );
    assert!(extract_result_key(&payload, &["missing".into()]).is_err());
}

#[test]
fn routes_resolve_by_query_then_dotted_segment_then_default() {
    let specific = QueryConfig {
        added: Some(TemplateSpec::new("specific")),
        ..Default::default()
    };
    let default = QueryConfig {
        added: Some(TemplateSpec::new("default")),
        ..Default::default()
    };
    let routes = HashMap::from([("incidents".to_string(), specific.clone())]);
    assert_eq!(
        resolve_query_config("incidents", &routes, Some(&default)),
        Some(&specific)
    );
    assert_eq!(
        resolve_query_config("ns.incidents", &routes, Some(&default)),
        Some(&specific)
    );
    assert_eq!(
        resolve_query_config("other", &routes, Some(&default)),
        Some(&default)
    );
}

// ── End to end against the mock host ─────────────────────────────────────

#[tokio::test]
async fn session_mode_add_update_delete() {
    let host = MockAgentHost::builder()
        .with_turn_delay(Duration::from_millis(300))
        .start()
        .await
        .expect("mock host");
    let reaction = base_builder(&host)
        .with_provider("copilot")
        .with_title_template("Incident {{after.id}}")
        .with_message_template("Investigate {{after.id}}: {{after.title}} {{json after.tags}}")
        .with_follow_up_template("Severity is now {{after.severity}}")
        .with_on_delete(OnDelete::Archive)
        .build()
        .expect("build");
    init(&reaction, None).await;
    reaction.start().await.expect("start");

    let row = json!({"id": "INC-1", "title": "DB down", "severity": 2, "tags": ["db"]});
    reaction
        .enqueue_query_result(add("incidents", 1, row.clone()))
        .await
        .expect("enqueue");
    let sessions = host
        .wait_for_sessions(WAIT, |s| s.len() == 1 && s[0].turns.len() == 1)
        .await
        .expect("session started");
    let session = &sessions[0];
    assert_eq!(session.provider.as_deref(), Some("copilot"));
    assert_eq!(session.title.as_deref(), Some("Incident INC-1"));
    assert_eq!(
        session.turns[0].text,
        r#"Investigate INC-1: DB down ["db"]"#
    );
    assert_eq!(session.turns[0].meta["drasi"]["operation"], json!("ADD"));
    assert_eq!(session.turns[0].meta["drasi"]["after"], row);
    assert_eq!(session.turns[0].meta["drasi"]["resultKey"], json!("INC-1"));

    let row2 = json!({"id": "INC-1", "title": "DB down", "severity": 1, "tags": ["db"]});
    reaction
        .enqueue_query_result(update("incidents", 2, row.clone(), row2.clone()))
        .await
        .expect("enqueue");
    // The queued follow-up starts a second turn once the first completes.
    let sessions = host
        .wait_for_sessions(WAIT, |s| s[0].turns.len() == 2)
        .await
        .expect("follow-up consumed");
    assert_eq!(sessions[0].turns[1].text, "Severity is now 1");
    assert_eq!(sessions[0].turns[0].state, MockTurnState::Completed);

    reaction
        .enqueue_query_result(delete("incidents", 3, row2))
        .await
        .expect("enqueue");
    let sessions = host
        .wait_for_sessions(WAIT, |s| s[0].archived)
        .await
        .expect("archived");
    assert_eq!(sessions[0].turns[1].state, MockTurnState::Cancelled);
    assert_eq!(sessions.len(), 1);

    reaction.stop().await.expect("stop");
    assert_eq!(reaction.status().await, ComponentStatus::Stopped);
}

#[tokio::test]
async fn replayed_add_is_idempotent_and_state_survives_restart() {
    let host = MockAgentHost::builder()
        .with_turn_delay(Duration::from_millis(20))
        .start()
        .await
        .expect("mock host");
    let store = Arc::new(MemoryStateStoreProvider::new());
    let row = json!({"id": "INC-7", "title": "Disk full"});

    let first = base_builder(&host).build().expect("build");
    init(&first, Some(store.clone())).await;
    first.start().await.expect("start");
    first
        .enqueue_query_result(add("incidents", 1, row.clone()))
        .await
        .expect("enqueue");
    host.wait_for_sessions(WAIT, |s| s.len() == 1 && s[0].turns.len() == 1)
        .await
        .expect("session");
    first.stop().await.expect("stop");

    // A reaction without the stored activation replays the same ADD: the
    // deterministic session URI and turn id prevent duplicates.
    let replay = base_builder(&host).build().expect("build");
    init(&replay, None).await;
    replay.start().await.expect("start");
    replay
        .enqueue_query_result(add("incidents", 1, row.clone()))
        .await
        .expect("enqueue");
    tokio::time::sleep(Duration::from_millis(300)).await;
    replay.stop().await.expect("stop");
    let sessions = host.sessions();
    assert_eq!(sessions.len(), 1, "{sessions:#?}");
    assert_eq!(sessions[0].turns.len(), 1);

    // A restarted reaction with the store follows up on the same chat.
    let restarted = base_builder(&host).build().expect("build");
    init(&restarted, Some(store)).await;
    restarted.start().await.expect("start");
    restarted
        .enqueue_query_result(update(
            "incidents",
            2,
            row,
            json!({"id": "INC-7", "title": "Disk 95% full"}),
        ))
        .await
        .expect("enqueue");
    let sessions = host
        .wait_for_sessions(WAIT, |s| s[0].turns.len() == 2)
        .await
        .expect("follow-up");
    assert_eq!(sessions.len(), 1);
    assert!(sessions[0].turns[1].text.contains("Disk 95% full"));
    restarted.stop().await.expect("stop");
}

#[tokio::test]
async fn follow_up_on_disposed_session_starts_a_new_one() {
    let host = MockAgentHost::builder()
        .with_turn_delay(Duration::from_millis(20))
        .start()
        .await
        .expect("mock host");
    let reaction = base_builder(&host)
        .with_on_delete(OnDelete::Dispose)
        .with_follow_up_kind(FollowUpKind::Steering)
        .build()
        .expect("build");
    init(&reaction, None).await;
    reaction.start().await.expect("start");

    let row = json!({"id": "A"});
    reaction
        .enqueue_query_result(add("incidents", 1, row.clone()))
        .await
        .expect("enqueue");
    host.wait_for_sessions(WAIT, |s| s.len() == 1)
        .await
        .expect("one");
    reaction
        .enqueue_query_result(delete("incidents", 2, row.clone()))
        .await
        .expect("enqueue");
    host.wait_for_sessions(WAIT, |s| s[0].disposed)
        .await
        .expect("disposed");

    // Re-adding the row after DELETE starts a fresh session.
    reaction
        .enqueue_query_result(add("incidents", 3, row))
        .await
        .expect("enqueue");
    let sessions = host
        .wait_for_sessions(WAIT, |s| s.len() == 2 && s[1].turns.len() == 1)
        .await
        .expect("new session");
    assert_ne!(sessions[0].uri, sessions[1].uri);
    reaction.stop().await.expect("stop");
}

#[tokio::test]
async fn automation_mode_runs_and_cancels() {
    let host = MockAgentHost::start().await.expect("mock host");
    let reaction = base_builder(&host)
        .with_automation("ahp-automation:/triage")
        .build()
        .expect("build");
    init(&reaction, None).await;
    reaction.start().await.expect("start");

    let row = json!({"id": "B", "title": "Flaky test"});
    reaction
        .enqueue_query_result(add("incidents", 1, row.clone()))
        .await
        .expect("enqueue");
    let runs = host
        .wait_for_runs(WAIT, |r| r.len() == 1)
        .await
        .expect("run");
    assert_eq!(runs[0].automation, "ahp-automation:/triage");
    assert_eq!(runs[0].meta["drasi"]["after"], row);
    assert!(runs[0].request_id.starts_with("drasi-"));

    reaction
        .enqueue_query_result(delete("incidents", 2, row))
        .await
        .expect("enqueue");
    host.wait_for_runs(WAIT, |r| r[0].cancelled)
        .await
        .expect("cancelled");
    assert!(host.sessions().is_empty());
    reaction.stop().await.expect("stop");
}

#[tokio::test]
async fn authenticates_with_configured_token() {
    let host = MockAgentHost::builder()
        .with_required_token("https://agent.example/api", "s3cret")
        .start()
        .await
        .expect("mock host");

    let authed = base_builder(&host)
        .with_token("s3cret")
        .with_auth_resource("https://agent.example/api")
        .build()
        .expect("build");
    init(&authed, None).await;
    authed.start().await.expect("start");
    authed
        .enqueue_query_result(add("incidents", 1, json!({"id": "C"})))
        .await
        .expect("enqueue");
    host.wait_for_sessions(WAIT, |s| s.len() == 1)
        .await
        .expect("session");
    authed.stop().await.expect("stop");

    // Without a token the strict recovery policy stops the reaction.
    let anonymous = base_builder(&host).build().expect("build");
    init(&anonymous, None).await;
    anonymous.start().await.expect("start");
    anonymous
        .enqueue_query_result(add("incidents", 1, json!({"id": "D"})))
        .await
        .expect("enqueue");
    wait_status(&anonymous, ComponentStatus::Error).await;
    assert_eq!(host.sessions().len(), 1);
    anonymous.stop().await.expect("stop");
}

#[tokio::test]
async fn rows_without_result_key_and_noops_are_skipped() {
    let host = MockAgentHost::start().await.expect("mock host");
    let reaction = base_builder(&host).build().expect("build");
    init(&reaction, None).await;
    reaction.start().await.expect("start");
    reaction
        .enqueue_query_result(result(
            "incidents",
            1,
            vec![
                ResultDiff::Noop,
                ResultDiff::Add {
                    data: json!({"no_id": true}),
                    row_signature: 0,
                },
            ],
        ))
        .await
        .expect("enqueue");
    reaction
        .enqueue_query_result(result("incidents", 2, vec![]))
        .await
        .expect("enqueue");
    reaction
        .enqueue_query_result(add("incidents", 3, json!({"id": "E"})))
        .await
        .expect("enqueue");
    let sessions = host
        .wait_for_sessions(WAIT, |s| s.len() == 1 && s[0].turns.len() == 1)
        .await
        .expect("session");
    assert!(sessions[0].turns[0].text.contains("`E`"));
    assert_eq!(reaction.status().await, ComponentStatus::Running);
    reaction.stop().await.expect("stop");
}

// ── Descriptor ────────────────────────────────────────────────────────────

#[tokio::test]
async fn descriptor_creates_reaction_and_preserves_raw_config() {
    let descriptor = AhpReactionDescriptor;
    assert_eq!(descriptor.kind(), "ahp");
    let schema: Value =
        serde_json::from_str(&descriptor.config_schema_json()).expect("schema json");
    assert!(schema.get("reaction.ahp.AhpReactionConfig").is_some());

    let config = json!({
        "endpoint": "ws://localhost:9",
        "token": "abc",
        "resultKeyFields": ["id"],
        "titleTemplate": "{{after.id}}",
        "defaultTemplate": {"added": {"template": "hi {{after.id}}"}},
        "routes": {"q1": {"updated": {"template": "upd"}}},
        "onDelete": "dispose",
        "followUpKind": "steering",
        "recoveryPolicy": "auto_skip_gap"
    });
    let reaction = descriptor
        .create_reaction("r", vec!["q1".into()], &config, true)
        .await
        .expect("create");
    assert_eq!(reaction.type_name(), "ahp");
    assert_eq!(
        reaction.default_recovery_policy(),
        drasi_lib::recovery::ReactionRecoveryPolicy::AutoSkipGap
    );
    let props = reaction.properties();
    assert_eq!(props["token"], json!("abc"));
    assert_eq!(props["onDelete"], json!("dispose"));

    let automation = json!({
        "endpoint": "ws://localhost:9",
        "resultKeyFields": ["id"],
        "automation": "ahp-automation:/x"
    });
    descriptor
        .create_reaction("r2", vec!["q1".into()], &automation, true)
        .await
        .expect("create automation");

    let bad = json!({"endpoint": "ws://x", "resultKeyFields": ["id"], "bogus": 1});
    assert!(descriptor
        .create_reaction("r3", vec!["q1".into()], &bad, true)
        .await
        .is_err());
}
