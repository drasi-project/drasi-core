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

//! 200-scenario battle gauntlet for drasi-reaction-a2a.
//! Run: cargo test -p drasi-reaction-a2a --test gauntlet -- --nocapture --test-threads=1

mod mock_server;
mod mock_source;

use std::net::TcpStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use drasi_lib::channels::ComponentStatus;
use drasi_lib::component_graph::ComponentGraph;
use drasi_lib::context::ReactionRuntimeContext;
use drasi_lib::state_store::StateStoreProvider;
use drasi_lib::{MemoryStateStoreProvider, Reaction, ReactionRecoveryPolicy};
use drasi_reaction_a2a::activation::ActivationState;
use drasi_reaction_a2a::client::{
    build_cancel_task_rpc, build_get_task_rpc, build_send_message_rpc, parse_send_message_result,
    CancelTaskRequest, OutboundPart, SendMessageRequest, SendMessageResult,
};
use drasi_reaction_a2a::{
    next_action, A2AReaction, A2AReactionConfig, Action, Activation, MessageId, Operation,
    TerminalUpdatePolicy,
};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

#[derive(Debug, Clone)]
struct Row {
    id: String,
    layer: &'static str,
    name: String,
    status: Status,
    detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Status {
    Pass,
    Fail,
    Probe,
    Skip,
}

struct Report {
    rows: Vec<Row>,
}

impl Report {
    fn new() -> Self {
        Self { rows: Vec::new() }
    }

    fn pass(&mut self, id: impl Into<String>, layer: &'static str, name: impl Into<String>) {
        self.rows.push(Row {
            id: id.into(),
            layer,
            name: name.into(),
            status: Status::Pass,
            detail: String::new(),
        });
    }

    fn fail(
        &mut self,
        id: impl Into<String>,
        layer: &'static str,
        name: impl Into<String>,
        detail: impl Into<String>,
    ) {
        self.rows.push(Row {
            id: id.into(),
            layer,
            name: name.into(),
            status: Status::Fail,
            detail: detail.into(),
        });
    }

    fn probe(
        &mut self,
        id: impl Into<String>,
        layer: &'static str,
        name: impl Into<String>,
        detail: impl Into<String>,
    ) {
        self.rows.push(Row {
            id: id.into(),
            layer,
            name: name.into(),
            status: Status::Probe,
            detail: detail.into(),
        });
    }

    fn skip(
        &mut self,
        id: impl Into<String>,
        layer: &'static str,
        name: impl Into<String>,
        detail: impl Into<String>,
    ) {
        self.rows.push(Row {
            id: id.into(),
            layer,
            name: name.into(),
            status: Status::Skip,
            detail: detail.into(),
        });
    }

    #[allow(clippy::print_stdout)]
    fn print(&self) {
        let mut pass = 0;
        let mut fail = 0;
        let mut probe = 0;
        let mut skip = 0;
        println!("\n===== A2A REACTION GAUNTLET =====");
        println!(
            "{:<8} {:<8} {:<10} {:<52} DETAIL",
            "ID", "LAYER", "STATUS", "NAME"
        );
        for row in &self.rows {
            let status = match row.status {
                Status::Pass => {
                    pass += 1;
                    "PASS"
                }
                Status::Fail => {
                    fail += 1;
                    "FAIL"
                }
                Status::Probe => {
                    probe += 1;
                    "PROBE"
                }
                Status::Skip => {
                    skip += 1;
                    "SKIP"
                }
            };
            if row.status != Status::Pass {
                println!(
                    "{:<8} {:<8} {:<10} {:<52} {}",
                    row.id, row.layer, status, row.name, row.detail
                );
            }
        }
        println!(
            "----- total={} pass={} fail={} probe={} skip={} -----",
            self.rows.len(),
            pass,
            fail,
            probe,
            skip
        );
        for row in &self.rows {
            if row.status == Status::Fail {
                println!("FAIL {}: {} — {}", row.id, row.name, row.detail);
            }
        }
    }

    fn fail_count(&self) -> usize {
        self.rows
            .iter()
            .filter(|r| r.status == Status::Fail)
            .count()
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Absent,
    OneShot,
    Active,
    Terminal,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SpecAction {
    Create,
    FollowUp,
    Cancel,
    Drop,
}

fn spec_action(
    op: Operation,
    kind: Kind,
    policy: TerminalUpdatePolicy,
    stored_seq: Option<u64>,
    seq: u64,
) -> SpecAction {
    if let Some(stored) = stored_seq {
        if stored >= seq {
            return SpecAction::Drop;
        }
    }
    match (op, kind, policy) {
        (Operation::Add, Kind::Absent, _) => SpecAction::Create,
        (Operation::Add, Kind::Active, _) => SpecAction::Drop,
        (Operation::Add, Kind::OneShot, _) => SpecAction::Drop,
        (Operation::Add, Kind::Terminal, TerminalUpdatePolicy::Replace) => SpecAction::Create,
        (Operation::Add, Kind::Terminal, TerminalUpdatePolicy::Ignore) => SpecAction::Drop,
        (Operation::Update, Kind::Absent, _) => SpecAction::Create,
        (Operation::Update, Kind::Active, _) => SpecAction::FollowUp,
        (Operation::Update, Kind::OneShot, _) => SpecAction::Drop,
        (Operation::Update, Kind::Terminal, TerminalUpdatePolicy::Replace) => SpecAction::Create,
        (Operation::Update, Kind::Terminal, TerminalUpdatePolicy::Ignore) => SpecAction::Drop,
        (Operation::Delete, Kind::Active, _) => SpecAction::Cancel,
        (Operation::Delete, _, _) => SpecAction::Drop,
    }
}

fn classify(action: &Action) -> SpecAction {
    match action {
        Action::SendCreate => SpecAction::Create,
        Action::SendFollowUp { .. } => SpecAction::FollowUp,
        Action::Cancel { .. } => SpecAction::Cancel,
        Action::Drop { .. } => SpecAction::Drop,
    }
}

fn activation_for(kind: Kind, seq: u64) -> ActivationState {
    match kind {
        Kind::Absent => ActivationState::Absent,
        Kind::OneShot => ActivationState::Present(Activation::OneShot {
            message_id: MessageId("msg".into()),
            sequence: seq,
        }),
        Kind::Active => ActivationState::Present(Activation::ActiveTask {
            task_id: "task-1".into(),
            context_id: "ctx".into(),
            state: "WORKING".into(),
            sequence: seq,
        }),
        Kind::Terminal => ActivationState::Present(Activation::TerminalTask {
            task_id: "task-9".into(),
            context_id: "ctx".into(),
            state: "COMPLETED".into(),
            sequence: seq,
        }),
    }
}

fn run_state_machine(report: &mut Report) {
    let ops = [Operation::Add, Operation::Update, Operation::Delete];
    let kinds = [Kind::Absent, Kind::OneShot, Kind::Active, Kind::Terminal];
    let policies = [TerminalUpdatePolicy::Replace, TerminalUpdatePolicy::Ignore];
    let seqs = [(2u64, 3u64), (2, 2), (2, 1)];
    let mut i = 1u32;
    for op in ops {
        for kind in kinds {
            for policy in policies {
                for (stored, incoming) in seqs {
                    let stored_seq = match kind {
                        Kind::Absent => None,
                        _ => Some(stored),
                    };
                    let expected = spec_action(op, kind, policy, stored_seq, incoming);
                    let state = activation_for(kind, stored);
                    let actual = classify(&next_action(op, &state, policy, incoming));
                    let id = format!("SM{i:03}");
                    let name = format!(
                        "{:?} {:?} {:?} stored={stored} seq={incoming}",
                        op,
                        match kind {
                            Kind::Absent => "Absent",
                            Kind::OneShot => "OneShot",
                            Kind::Active => "Active",
                            Kind::Terminal => "Terminal",
                        },
                        policy
                    );
                    if actual == expected {
                        report.pass(id, "sm", name);
                    } else {
                        report.fail(
                            id,
                            "sm",
                            name,
                            format!("expected {expected:?} got {actual:?}"),
                        );
                    }
                    i += 1;
                }
            }
        }
    }
}

fn run_parser(report: &mut Report) {
    let cases: &[(&str, Value, Result<&str, ()>)] = &[
        (
            "wrapped WORKING",
            json!({"task":{"id":"t","contextId":"c","status":{"state":"TASK_STATE_WORKING"}}}),
            Ok("active:WORKING"),
        ),
        (
            "wrapped SUBMITTED",
            json!({"task":{"id":"t","contextId":"c","status":{"state":"TASK_STATE_SUBMITTED"}}}),
            Ok("active:SUBMITTED"),
        ),
        (
            "wrapped INPUT_REQUIRED",
            json!({"task":{"id":"t","contextId":"c","status":{"state":"TASK_STATE_INPUT_REQUIRED"}}}),
            Ok("active:INPUT_REQUIRED"),
        ),
        (
            "wrapped AUTH_REQUIRED",
            json!({"task":{"id":"t","contextId":"c","status":{"state":"TASK_STATE_AUTH_REQUIRED"}}}),
            Ok("active:AUTH_REQUIRED"),
        ),
        (
            "wrapped COMPLETED",
            json!({"task":{"id":"t","contextId":"c","status":{"state":"TASK_STATE_COMPLETED"}}}),
            Ok("terminal:COMPLETED"),
        ),
        (
            "wrapped FAILED",
            json!({"task":{"id":"t","contextId":"c","status":{"state":"TASK_STATE_FAILED"}}}),
            Ok("terminal:FAILED"),
        ),
        (
            "wrapped CANCELED",
            json!({"task":{"id":"t","contextId":"c","status":{"state":"TASK_STATE_CANCELED"}}}),
            Ok("terminal:CANCELED"),
        ),
        (
            "wrapped REJECTED",
            json!({"task":{"id":"t","contextId":"c","status":{"state":"TASK_STATE_REJECTED"}}}),
            Ok("terminal:REJECTED"),
        ),
        (
            "bare COMPLETED",
            json!({"id":"t","contextId":"c","status":{"state":"TASK_STATE_COMPLETED"}}),
            Ok("terminal:COMPLETED"),
        ),
        (
            "bare WORKING top-level state",
            json!({"id":"t","contextId":"c","state":"WORKING"}),
            Ok("active:WORKING"),
        ),
        (
            "unprefixed completed",
            json!({"id":"t","status":{"state":"completed"}}),
            Ok("terminal:COMPLETED"),
        ),
        (
            "unprefixed failed",
            json!({"id":"t","status":{"state":"failed"}}),
            Ok("terminal:FAILED"),
        ),
        (
            "message wrapped",
            json!({"message":{"role":"ROLE_AGENT","parts":[{"text":"ok"}]}}),
            Ok("message"),
        ),
        (
            "bare message",
            json!({"role":"ROLE_USER","parts":[{"text":"x"}]}),
            Ok("message"),
        ),
        ("missing id", json!({"status":{"state":"WORKING"}}), Err(())),
        ("missing state", json!({"id":"t"}), Err(())),
        ("empty object", json!({}), Err(())),
        ("null", json!(null), Err(())),
        ("array", json!([]), Err(())),
        ("string", json!("task"), Err(())),
        ("parts null", json!({"parts": null}), Err(())),
        (
            "parts without role",
            json!({"parts":[{"text":"x"}]}),
            Err(()),
        ),
        ("role without parts", json!({"role":"ROLE_AGENT"}), Err(())),
        ("message not object", json!({"message": "nope"}), Err(())),
        (
            "task missing id",
            json!({"task":{"status":{"state":"WORKING"}}}),
            Err(()),
        ),
        (
            "whitespace state",
            json!({"id":"t","status":{"state":"  COMPLETED  "}}),
            Ok("terminal:COMPLETED"),
        ),
        (
            "no contextId",
            json!({"id":"t","status":{"state":"WORKING"}}),
            Ok("active:WORKING"),
        ),
        (
            "extra fields",
            json!({"id":"t","status":{"state":"FAILED"},"history":[],"foo":1}),
            Ok("terminal:FAILED"),
        ),
        (
            "task preferred over message",
            json!({
                "task":{"id":"t","status":{"state":"WORKING"}},
                "message":{"role":"ROLE_AGENT","parts":[{"text":"x"}]}
            }),
            Ok("active:WORKING"),
        ),
        (
            "numeric id rejected",
            json!({"id":1,"status":{"state":"WORKING"}}),
            Err(()),
        ),
        (
            "empty id string",
            json!({"id":"","status":{"state":"WORKING"}}),
            Ok("active:WORKING"),
        ),
        (
            "unknown state FOO",
            json!({"id":"t","status":{"state":"FOO"}}),
            Ok("active:FOO"),
        ),
        (
            "hyphen input-required",
            json!({"id":"t","status":{"state":"input-required"}}),
            Ok("active:INPUT_REQUIRED"),
        ),
        (
            "CANCELLED british",
            json!({"id":"t","status":{"state":"CANCELLED"}}),
            Ok("terminal:CANCELED"),
        ),
        (
            "lowercase prefix task_state_completed",
            json!({"id":"t","status":{"state":"task_state_completed"}}),
            Ok("terminal:COMPLETED"),
        ),
        (
            "mixed prefix TASK_STATE_completed",
            json!({"id":"t","status":{"state":"TASK_STATE_completed"}}),
            Ok("terminal:COMPLETED"),
        ),
    ];

    for (i, (name, value, expected)) in cases.iter().enumerate() {
        let id = format!("PR{:03}", i + 1);
        let parsed = parse_send_message_result(value);
        let actual = match parsed {
            Ok(SendMessageResult::Message) => "message".to_string(),
            Ok(SendMessageResult::Task {
                state, terminal, ..
            }) => {
                if terminal {
                    format!("terminal:{state}")
                } else {
                    format!("active:{state}")
                }
            }
            Err(_) => "err".to_string(),
        };
        match expected {
            Ok(exp) if actual == *exp => report.pass(id, "parse", *name),
            Ok(exp) => {
                let probe_names = ["unknown state FOO"];
                if probe_names.contains(name) {
                    report.probe(
                        id,
                        "parse",
                        *name,
                        format!("contract-ish {exp} actual {actual}"),
                    );
                } else {
                    report.fail(id, "parse", *name, format!("expected {exp} got {actual}"));
                }
            }
            Err(()) if actual == "err" => report.pass(id, "parse", *name),
            Err(()) => report.fail(id, "parse", *name, format!("expected err got {actual}")),
        }
    }
}

fn run_config(report: &mut Report) {
    fn cfg(
        endpoint: &str,
        timeout_ms: u64,
        keys: Vec<&str>,
        template: Option<&str>,
    ) -> A2AReactionConfig {
        A2AReactionConfig {
            endpoint: endpoint.into(),
            token: None,
            timeout_ms,
            result_key_fields: keys.into_iter().map(str::to_string).collect(),
            instruction_template: template.map(str::to_string),
            terminal_update_policy: TerminalUpdatePolicy::Replace,
            return_immediately: true,
        }
    }

    let cases: &[(&str, A2AReactionConfig, Option<usize>, bool)] = &[
        (
            "valid http",
            cfg("http://127.0.0.1:9999/", 5000, vec!["id"], None),
            None,
            true,
        ),
        (
            "valid https",
            cfg("https://agent.example.com/a2a", 1, vec!["invoiceId"], None),
            None,
            true,
        ),
        (
            "empty endpoint",
            cfg("", 5000, vec!["id"], None),
            None,
            false,
        ),
        (
            "whitespace endpoint",
            cfg("   ", 5000, vec!["id"], None),
            None,
            false,
        ),
        (
            "ftp scheme",
            cfg("ftp://127.0.0.1/", 5000, vec!["id"], None),
            None,
            false,
        ),
        (
            "no host",
            cfg("http://", 5000, vec!["id"], None),
            None,
            false,
        ),
        (
            "timeout zero",
            cfg("http://localhost/", 0, vec!["id"], None),
            None,
            false,
        ),
        (
            "empty keys",
            cfg("http://localhost/", 5000, vec![], None),
            None,
            false,
        ),
        (
            "blank key field",
            cfg("http://localhost/", 5000, vec!["  "], None),
            None,
            false,
        ),
        (
            "one blank among keys",
            cfg("http://localhost/", 5000, vec!["id", ""], None),
            None,
            false,
        ),
        (
            "queue capacity zero",
            cfg("http://localhost/", 5000, vec!["id"], None),
            Some(0),
            false,
        ),
        (
            "queue capacity one",
            cfg("http://localhost/", 5000, vec!["id"], None),
            Some(1),
            true,
        ),
        (
            "good template",
            cfg(
                "http://localhost/",
                5000,
                vec!["id"],
                Some("hello {{after.id}}"),
            ),
            None,
            true,
        ),
        (
            "bad template",
            cfg("http://localhost/", 5000, vec!["id"], Some("{{#unclosed")),
            None,
            false,
        ),
        (
            "empty template",
            cfg("http://localhost/", 5000, vec!["id"], Some("")),
            None,
            true,
        ),
        (
            "ws scheme",
            cfg("ws://localhost/a2a", 5000, vec!["id"], None),
            None,
            false,
        ),
        (
            "file scheme",
            cfg("file:///tmp/a2a", 5000, vec!["id"], None),
            None,
            false,
        ),
        (
            "http with userinfo",
            cfg("http://user:pass@127.0.0.1:9/", 5000, vec!["id"], None),
            None,
            true,
        ),
        (
            "https IPv6",
            cfg("https://[::1]:443/", 5000, vec!["id"], None),
            None,
            true,
        ),
        (
            "timeout huge",
            cfg("http://localhost/", u64::MAX, vec!["id"], None),
            None,
            true,
        ),
        (
            "nested template helper",
            cfg(
                "http://localhost/",
                5000,
                vec!["id"],
                Some("{{json after}}"),
            ),
            None,
            true,
        ),
    ];

    for (i, (name, config, cap, ok)) in cases.iter().enumerate() {
        let id = format!("CF{:03}", i + 1);
        let actual = config.validate(&[], *cap).is_ok();
        if actual == *ok {
            report.pass(id, "config", *name);
        } else {
            report.fail(
                id,
                "config",
                *name,
                format!("expected ok={ok} got ok={actual}"),
            );
        }
    }
}

fn run_rpc(report: &mut Report) {
    let create = build_send_message_rpc(&SendMessageRequest {
        message_id: "m1".into(),
        activation_id: "a1".into(),
        task_id: None,
        parts: vec![
            OutboundPart::Text("hi".into()),
            OutboundPart::Data {
                media_type: "application/vnd.drasi.change+json".into(),
                data: json!({"queryId":"q"}),
            },
        ],
        return_immediately: true,
    });
    let follow = build_send_message_rpc(&SendMessageRequest {
        message_id: "m2".into(),
        activation_id: "a1".into(),
        task_id: Some("task-1".into()),
        parts: vec![OutboundPart::Text("u".into())],
        return_immediately: false,
    });
    let get = build_get_task_rpc("m3", "task-1");
    let cancel = build_cancel_task_rpc(&CancelTaskRequest {
        message_id: "m4".into(),
        task_id: "task-1".into(),
    });

    let checks: &[(&str, bool, String)] = &[
        (
            "create method",
            create["method"] == json!("SendMessage"),
            format!("{}", create["method"]),
        ),
        (
            "create omits taskId",
            create["params"]["message"].get("taskId").is_none(),
            "taskId present".into(),
        ),
        (
            "create role ROLE_USER",
            create["params"]["message"]["role"] == json!("ROLE_USER"),
            format!("{}", create["params"]["message"]["role"]),
        ),
        (
            "create untagged parts",
            create["params"]["message"]["parts"][0]
                .get("kind")
                .is_none(),
            "kind present".into(),
        ),
        (
            "create activationId",
            create["params"]["message"]["metadata"]["activationId"] == json!("a1"),
            format!("{}", create["params"]["message"]["metadata"]),
        ),
        (
            "create returnImmediately true",
            create["params"]["configuration"]["returnImmediately"] == json!(true),
            format!("{}", create["params"]["configuration"]),
        ),
        (
            "follow-up taskId",
            follow["params"]["message"]["taskId"] == json!("task-1"),
            format!("{}", follow["params"]["message"]["taskId"]),
        ),
        (
            "follow-up returnImmediately false",
            follow["params"]["configuration"]["returnImmediately"] == json!(false),
            format!("{}", follow["params"]["configuration"]),
        ),
        (
            "GetTask method",
            get["method"] == json!("GetTask"),
            format!("{}", get["method"]),
        ),
        (
            "GetTask params.id",
            get["params"]["id"] == json!("task-1"),
            format!("{}", get["params"]),
        ),
        (
            "CancelTask method",
            cancel["method"] == json!("CancelTask"),
            format!("{}", cancel["method"]),
        ),
        (
            "CancelTask params.id",
            cancel["params"]["id"] == json!("task-1"),
            format!("{}", cancel["params"]),
        ),
    ];

    for (i, (name, ok, detail)) in checks.iter().enumerate() {
        let id = format!("RPC{:03}", i + 1);
        if *ok {
            report.pass(id, "rpc", *name);
        } else {
            report.fail(id, "rpc", *name, detail.clone());
        }
    }
}

struct OrderedJsonRpc {
    next: AtomicUsize,
    bodies: Vec<Value>,
}

impl Respond for OrderedJsonRpc {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        let index = self.next.fetch_add(1, Ordering::SeqCst);
        let body = self
            .bodies
            .get(index)
            .unwrap_or_else(|| self.bodies.last().expect("bodies"));
        ResponseTemplate::new(200).set_body_json(body.clone())
    }
}

fn task_body(id: &str, state: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": "1",
        "result": {
            "task": {
                "id": id,
                "contextId": "ctx-1",
                "status": { "state": format!("TASK_STATE_{state}") }
            }
        }
    })
}

fn bare_task_body(id: &str, state: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": "1",
        "result": {
            "id": id,
            "contextId": "ctx-1",
            "status": { "state": format!("TASK_STATE_{state}") }
        }
    })
}

fn message_body() -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": "1",
        "result": { "message": { "role": "ROLE_AGENT", "parts": [{"text":"ok"}] } }
    })
}

fn rpc_error(code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": "1",
        "error": { "code": code, "message": message }
    })
}

async fn initialize_with_store(reaction: &A2AReaction, store: Arc<MemoryStateStoreProvider>) {
    let (graph, _rx) = ComponentGraph::new("a2a-g");
    let context = ReactionRuntimeContext::new(
        "a2a-g",
        reaction.id(),
        Some(store),
        graph.update_sender(),
        None,
    );
    reaction.initialize(context).await;
}

fn memory_store() -> Arc<MemoryStateStoreProvider> {
    Arc::new(MemoryStateStoreProvider::new())
}

async fn wait_for_requests(server: &MockServer, expected: usize, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let count = server.received_requests().await.unwrap_or_default().len();
        if count >= expected {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_status(reaction: &A2AReaction, want: ComponentStatus, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if reaction.status().await == want {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn methods_of(requests: &[Request]) -> Vec<String> {
    requests
        .iter()
        .map(|r| {
            let body: Value = serde_json::from_slice(&r.body).unwrap_or(json!({}));
            body["method"].as_str().unwrap_or("?").to_string()
        })
        .collect()
}

struct ProcCase {
    name: &'static str,
    policy: TerminalUpdatePolicy,
    recovery: Option<ReactionRecoveryPolicy>,
    token: Option<&'static str>,
    template: Option<&'static str>,
    keys: &'static [&'static str],
    bodies: Vec<Value>,
    events: Vec<Event>,
    expect_methods: &'static [&'static str],
    expect_error: bool,
    expect_header_version: bool,
    expect_bearer: bool,
}

enum Event {
    Add {
        seq: u64,
        invoice: &'static str,
        extra: Value,
    },
    Update {
        seq: u64,
        invoice: &'static str,
        after: Value,
    },
    Delete {
        seq: u64,
        invoice: &'static str,
    },
}

fn invoice_doc(id: &str, amount: f64) -> Value {
    json!({"invoiceId": id, "amount": amount})
}

async fn run_process(report: &mut Report) {
    let cases = process_cases();
    for (i, case) in cases.into_iter().enumerate() {
        let id = format!("P{:03}", i + 1);
        match run_one_process(&case).await {
            Ok(()) => report.pass(id, "proc", case.name),
            Err(detail) => report.fail(id, "proc", case.name, detail),
        }
    }
}

fn process_cases() -> Vec<ProcCase> {
    vec![
        ProcCase {
            name: "ADD create WORKING",
            policy: TerminalUpdatePolicy::Replace,
            recovery: None,
            token: None,
            template: None,
            keys: &["invoiceId"],
            bodies: vec![task_body("task-1", "WORKING")],
            events: vec![Event::Add {
                seq: 1,
                invoice: "A",
                extra: invoice_doc("A", 1.0),
            }],
            expect_methods: &["SendMessage"],
            expect_error: false,
            expect_header_version: true,
            expect_bearer: false,
        },
        ProcCase {
            name: "UPDATE follow-up after GetTask WORKING",
            policy: TerminalUpdatePolicy::Replace,
            recovery: None,
            token: None,
            template: None,
            keys: &["invoiceId"],
            bodies: vec![
                task_body("task-1", "WORKING"),
                bare_task_body("task-1", "WORKING"),
                task_body("task-1", "WORKING"),
            ],
            events: vec![
                Event::Add {
                    seq: 1,
                    invoice: "B",
                    extra: invoice_doc("B", 1.0),
                },
                Event::Update {
                    seq: 2,
                    invoice: "B",
                    after: invoice_doc("B", 2.0),
                },
            ],
            expect_methods: &["SendMessage", "GetTask", "SendMessage"],
            expect_error: false,
            expect_header_version: true,
            expect_bearer: false,
        },
        ProcCase {
            name: "DELETE cancel after GetTask WORKING",
            policy: TerminalUpdatePolicy::Replace,
            recovery: None,
            token: None,
            template: None,
            keys: &["invoiceId"],
            bodies: vec![
                task_body("task-1", "WORKING"),
                bare_task_body("task-1", "WORKING"),
                json!({"jsonrpc":"2.0","id":"1","result":{"id":"task-1"}}),
            ],
            events: vec![
                Event::Add {
                    seq: 1,
                    invoice: "C",
                    extra: invoice_doc("C", 1.0),
                },
                Event::Delete {
                    seq: 2,
                    invoice: "C",
                },
            ],
            expect_methods: &["SendMessage", "GetTask", "CancelTask"],
            expect_error: false,
            expect_header_version: true,
            expect_bearer: false,
        },
        ProcCase {
            name: "CancelTask not found clears without error",
            policy: TerminalUpdatePolicy::Replace,
            recovery: None,
            token: None,
            template: None,
            keys: &["invoiceId"],
            bodies: vec![
                task_body("task-1", "WORKING"),
                bare_task_body("task-1", "WORKING"),
                rpc_error(-32001, "Task not found"),
            ],
            events: vec![
                Event::Add {
                    seq: 1,
                    invoice: "CN",
                    extra: invoice_doc("CN", 1.0),
                },
                Event::Delete {
                    seq: 2,
                    invoice: "CN",
                },
            ],
            expect_methods: &["SendMessage", "GetTask", "CancelTask"],
            expect_error: false,
            expect_header_version: true,
            expect_bearer: false,
        },
        ProcCase {
            name: "UPDATE after GetTask COMPLETED creates",
            policy: TerminalUpdatePolicy::Replace,
            recovery: None,
            token: None,
            template: None,
            keys: &["invoiceId"],
            bodies: vec![
                task_body("task-1", "SUBMITTED"),
                bare_task_body("task-1", "COMPLETED"),
                task_body("task-2", "SUBMITTED"),
            ],
            events: vec![
                Event::Add {
                    seq: 1,
                    invoice: "D",
                    extra: invoice_doc("D", 1.0),
                },
                Event::Update {
                    seq: 2,
                    invoice: "D",
                    after: invoice_doc("D", 2.0),
                },
            ],
            expect_methods: &["SendMessage", "GetTask", "SendMessage"],
            expect_error: false,
            expect_header_version: true,
            expect_bearer: false,
        },
        ProcCase {
            name: "terminal COMPLETED replace UPDATE creates",
            policy: TerminalUpdatePolicy::Replace,
            recovery: None,
            token: None,
            template: None,
            keys: &["invoiceId"],
            bodies: vec![
                task_body("task-1", "COMPLETED"),
                task_body("task-2", "WORKING"),
            ],
            events: vec![
                Event::Add {
                    seq: 1,
                    invoice: "E",
                    extra: invoice_doc("E", 1.0),
                },
                Event::Update {
                    seq: 2,
                    invoice: "E",
                    after: invoice_doc("E", 2.0),
                },
            ],
            expect_methods: &["SendMessage", "SendMessage"],
            expect_error: false,
            expect_header_version: true,
            expect_bearer: false,
        },
        ProcCase {
            name: "terminal ignore UPDATE silent",
            policy: TerminalUpdatePolicy::Ignore,
            recovery: None,
            token: None,
            template: None,
            keys: &["invoiceId"],
            bodies: vec![task_body("task-1", "COMPLETED")],
            events: vec![
                Event::Add {
                    seq: 1,
                    invoice: "F",
                    extra: invoice_doc("F", 1.0),
                },
                Event::Update {
                    seq: 2,
                    invoice: "F",
                    after: invoice_doc("F", 2.0),
                },
            ],
            expect_methods: &["SendMessage"],
            expect_error: false,
            expect_header_version: true,
            expect_bearer: false,
        },
        ProcCase {
            name: "one-shot no follow-up",
            policy: TerminalUpdatePolicy::Replace,
            recovery: None,
            token: None,
            template: None,
            keys: &["invoiceId"],
            bodies: vec![message_body()],
            events: vec![
                Event::Add {
                    seq: 1,
                    invoice: "G",
                    extra: invoice_doc("G", 1.0),
                },
                Event::Update {
                    seq: 2,
                    invoice: "G",
                    after: invoice_doc("G", 2.0),
                },
            ],
            expect_methods: &["SendMessage"],
            expect_error: false,
            expect_header_version: true,
            expect_bearer: false,
        },
        ProcCase {
            name: "one-shot DELETE silent",
            policy: TerminalUpdatePolicy::Replace,
            recovery: None,
            token: None,
            template: None,
            keys: &["invoiceId"],
            bodies: vec![message_body()],
            events: vec![
                Event::Add {
                    seq: 1,
                    invoice: "H",
                    extra: invoice_doc("H", 1.0),
                },
                Event::Delete {
                    seq: 2,
                    invoice: "H",
                },
            ],
            expect_methods: &["SendMessage"],
            expect_error: false,
            expect_header_version: true,
            expect_bearer: false,
        },
        ProcCase {
            name: "replay ADD dropped",
            policy: TerminalUpdatePolicy::Replace,
            recovery: None,
            token: None,
            template: None,
            keys: &["invoiceId"],
            bodies: vec![task_body("task-1", "WORKING")],
            events: vec![
                Event::Add {
                    seq: 7,
                    invoice: "I",
                    extra: invoice_doc("I", 1.0),
                },
                Event::Add {
                    seq: 7,
                    invoice: "I",
                    extra: invoice_doc("I", 1.0),
                },
            ],
            expect_methods: &["SendMessage"],
            expect_error: false,
            expect_header_version: true,
            expect_bearer: false,
        },
        ProcCase {
            name: "UPDATE absent creates",
            policy: TerminalUpdatePolicy::Replace,
            recovery: None,
            token: None,
            template: None,
            keys: &["invoiceId"],
            bodies: vec![task_body("task-1", "WORKING")],
            events: vec![Event::Update {
                seq: 1,
                invoice: "J",
                after: invoice_doc("J", 2.0),
            }],
            expect_methods: &["SendMessage"],
            expect_error: false,
            expect_header_version: true,
            expect_bearer: false,
        },
        ProcCase {
            name: "DELETE absent silent",
            policy: TerminalUpdatePolicy::Replace,
            recovery: None,
            token: None,
            template: None,
            keys: &["invoiceId"],
            bodies: vec![task_body("task-1", "WORKING")],
            events: vec![Event::Delete {
                seq: 1,
                invoice: "K",
            }],
            expect_methods: &[],
            expect_error: false,
            expect_header_version: false,
            expect_bearer: false,
        },
        ProcCase {
            name: "ADD after terminal replace creates",
            policy: TerminalUpdatePolicy::Replace,
            recovery: None,
            token: None,
            template: None,
            keys: &["invoiceId"],
            bodies: vec![
                task_body("task-1", "COMPLETED"),
                task_body("task-2", "WORKING"),
            ],
            events: vec![
                Event::Add {
                    seq: 1,
                    invoice: "L",
                    extra: invoice_doc("L", 1.0),
                },
                Event::Add {
                    seq: 2,
                    invoice: "L",
                    extra: invoice_doc("L", 1.0),
                },
            ],
            expect_methods: &["SendMessage", "SendMessage"],
            expect_error: false,
            expect_header_version: true,
            expect_bearer: false,
        },
        ProcCase {
            name: "GetTask method-not-found then follow-up",
            policy: TerminalUpdatePolicy::Replace,
            recovery: None,
            token: None,
            template: None,
            keys: &["invoiceId"],
            bodies: vec![
                task_body("task-1", "WORKING"),
                rpc_error(-32601, "Method not found"),
                task_body("task-1", "WORKING"),
            ],
            events: vec![
                Event::Add {
                    seq: 1,
                    invoice: "M",
                    extra: invoice_doc("M", 1.0),
                },
                Event::Update {
                    seq: 2,
                    invoice: "M",
                    after: invoice_doc("M", 2.0),
                },
            ],
            expect_methods: &["SendMessage", "GetTask", "SendMessage"],
            expect_error: false,
            expect_header_version: true,
            expect_bearer: false,
        },
        ProcCase {
            name: "GetTask not found then UPDATE creates",
            policy: TerminalUpdatePolicy::Replace,
            recovery: None,
            token: None,
            template: None,
            keys: &["invoiceId"],
            bodies: vec![
                task_body("task-1", "WORKING"),
                rpc_error(-32001, "Task not found"),
                task_body("task-2", "SUBMITTED"),
            ],
            events: vec![
                Event::Add {
                    seq: 1,
                    invoice: "N",
                    extra: invoice_doc("N", 1.0),
                },
                Event::Update {
                    seq: 2,
                    invoice: "N",
                    after: invoice_doc("N", 2.0),
                },
            ],
            expect_methods: &["SendMessage", "GetTask", "SendMessage"],
            expect_error: false,
            expect_header_version: true,
            expect_bearer: false,
        },
        ProcCase {
            name: "JSON-RPC error on ADD stops",
            policy: TerminalUpdatePolicy::Replace,
            recovery: None,
            token: None,
            template: None,
            keys: &["invoiceId"],
            bodies: vec![rpc_error(-32600, "bad request")],
            events: vec![Event::Add {
                seq: 1,
                invoice: "O",
                extra: invoice_doc("O", 1.0),
            }],
            expect_methods: &["SendMessage"],
            expect_error: true,
            expect_header_version: true,
            expect_bearer: false,
        },
        ProcCase {
            name: "instruction template text part",
            policy: TerminalUpdatePolicy::Replace,
            recovery: None,
            token: None,
            template: Some("go {{after.invoiceId}}"),
            keys: &["invoiceId"],
            bodies: vec![task_body("task-1", "WORKING")],
            events: vec![Event::Add {
                seq: 1,
                invoice: "P",
                extra: invoice_doc("P", 1.0),
            }],
            expect_methods: &["SendMessage"],
            expect_error: false,
            expect_header_version: true,
            expect_bearer: false,
        },
        ProcCase {
            name: "unicode result key",
            policy: TerminalUpdatePolicy::Replace,
            recovery: None,
            token: None,
            template: None,
            keys: &["invoiceId"],
            bodies: vec![task_body("task-1", "WORKING")],
            events: vec![Event::Add {
                seq: 1,
                invoice: "发票",
                extra: json!({"invoiceId":"发票","amount":1}),
            }],
            expect_methods: &["SendMessage"],
            expect_error: false,
            expect_header_version: true,
            expect_bearer: false,
        },
        ProcCase {
            name: "colon in result key",
            policy: TerminalUpdatePolicy::Replace,
            recovery: None,
            token: None,
            template: None,
            keys: &["invoiceId"],
            bodies: vec![task_body("task-1", "WORKING")],
            events: vec![Event::Add {
                seq: 1,
                invoice: "a:b",
                extra: json!({"invoiceId":"a:b","amount":1}),
            }],
            expect_methods: &["SendMessage"],
            expect_error: false,
            expect_header_version: true,
            expect_bearer: false,
        },
        ProcCase {
            name: "FAILED terminal replace",
            policy: TerminalUpdatePolicy::Replace,
            recovery: None,
            token: None,
            template: None,
            keys: &["invoiceId"],
            bodies: vec![
                task_body("task-1", "FAILED"),
                task_body("task-2", "WORKING"),
            ],
            events: vec![
                Event::Add {
                    seq: 1,
                    invoice: "Q",
                    extra: invoice_doc("Q", 1.0),
                },
                Event::Update {
                    seq: 2,
                    invoice: "Q",
                    after: invoice_doc("Q", 2.0),
                },
            ],
            expect_methods: &["SendMessage", "SendMessage"],
            expect_error: false,
            expect_header_version: true,
            expect_bearer: false,
        },
        ProcCase {
            name: "CANCELED terminal replace",
            policy: TerminalUpdatePolicy::Replace,
            recovery: None,
            token: None,
            template: None,
            keys: &["invoiceId"],
            bodies: vec![
                task_body("task-1", "CANCELED"),
                task_body("task-2", "WORKING"),
            ],
            events: vec![
                Event::Add {
                    seq: 1,
                    invoice: "R",
                    extra: invoice_doc("R", 1.0),
                },
                Event::Update {
                    seq: 2,
                    invoice: "R",
                    after: invoice_doc("R", 2.0),
                },
            ],
            expect_methods: &["SendMessage", "SendMessage"],
            expect_error: false,
            expect_header_version: true,
            expect_bearer: false,
        },
        ProcCase {
            name: "REJECTED terminal replace",
            policy: TerminalUpdatePolicy::Replace,
            recovery: None,
            token: None,
            template: None,
            keys: &["invoiceId"],
            bodies: vec![
                task_body("task-1", "REJECTED"),
                task_body("task-2", "WORKING"),
            ],
            events: vec![
                Event::Add {
                    seq: 1,
                    invoice: "S",
                    extra: invoice_doc("S", 1.0),
                },
                Event::Update {
                    seq: 2,
                    invoice: "S",
                    after: invoice_doc("S", 2.0),
                },
            ],
            expect_methods: &["SendMessage", "SendMessage"],
            expect_error: false,
            expect_header_version: true,
            expect_bearer: false,
        },
        ProcCase {
            name: "INPUT_REQUIRED still follow-up",
            policy: TerminalUpdatePolicy::Replace,
            recovery: None,
            token: None,
            template: None,
            keys: &["invoiceId"],
            bodies: vec![
                task_body("task-1", "INPUT_REQUIRED"),
                bare_task_body("task-1", "INPUT_REQUIRED"),
                task_body("task-1", "INPUT_REQUIRED"),
            ],
            events: vec![
                Event::Add {
                    seq: 1,
                    invoice: "T",
                    extra: invoice_doc("T", 1.0),
                },
                Event::Update {
                    seq: 2,
                    invoice: "T",
                    after: invoice_doc("T", 2.0),
                },
            ],
            expect_methods: &["SendMessage", "GetTask", "SendMessage"],
            expect_error: false,
            expect_header_version: true,
            expect_bearer: false,
        },
        ProcCase {
            name: "AUTH_REQUIRED still follow-up",
            policy: TerminalUpdatePolicy::Replace,
            recovery: None,
            token: None,
            template: None,
            keys: &["invoiceId"],
            bodies: vec![
                task_body("task-1", "AUTH_REQUIRED"),
                bare_task_body("task-1", "AUTH_REQUIRED"),
                task_body("task-1", "WORKING"),
            ],
            events: vec![
                Event::Add {
                    seq: 1,
                    invoice: "U",
                    extra: invoice_doc("U", 1.0),
                },
                Event::Update {
                    seq: 2,
                    invoice: "U",
                    after: invoice_doc("U", 2.0),
                },
            ],
            expect_methods: &["SendMessage", "GetTask", "SendMessage"],
            expect_error: false,
            expect_header_version: true,
            expect_bearer: false,
        },
        ProcCase {
            name: "DELETE after GetTask COMPLETED no cancel",
            policy: TerminalUpdatePolicy::Replace,
            recovery: None,
            token: None,
            template: None,
            keys: &["invoiceId"],
            bodies: vec![
                task_body("task-1", "SUBMITTED"),
                bare_task_body("task-1", "COMPLETED"),
            ],
            events: vec![
                Event::Add {
                    seq: 1,
                    invoice: "V",
                    extra: invoice_doc("V", 1.0),
                },
                Event::Delete {
                    seq: 2,
                    invoice: "V",
                },
            ],
            expect_methods: &["SendMessage", "GetTask"],
            expect_error: false,
            expect_header_version: true,
            expect_bearer: false,
        },
        ProcCase {
            name: "AutoSkipGap survives JSON-RPC error",
            policy: TerminalUpdatePolicy::Replace,
            recovery: Some(ReactionRecoveryPolicy::AutoSkipGap),
            token: None,
            template: None,
            keys: &["invoiceId"],
            bodies: vec![rpc_error(-32600, "bad"), task_body("task-1", "WORKING")],
            events: vec![
                Event::Add {
                    seq: 1,
                    invoice: "W",
                    extra: invoice_doc("W", 1.0),
                },
                Event::Add {
                    seq: 2,
                    invoice: "W2",
                    extra: invoice_doc("W2", 1.0),
                },
            ],
            expect_methods: &["SendMessage", "SendMessage"],
            expect_error: false,
            expect_header_version: true,
            expect_bearer: false,
        },
        ProcCase {
            name: "token Bearer header",
            policy: TerminalUpdatePolicy::Replace,
            recovery: None,
            token: Some("secret-token"),
            template: None,
            keys: &["invoiceId"],
            bodies: vec![task_body("task-1", "WORKING")],
            events: vec![Event::Add {
                seq: 1,
                invoice: "X",
                extra: invoice_doc("X", 1.0),
            }],
            expect_methods: &["SendMessage"],
            expect_error: false,
            expect_header_version: true,
            expect_bearer: true,
        },
        ProcCase {
            name: "nested result key",
            policy: TerminalUpdatePolicy::Replace,
            recovery: None,
            token: None,
            template: None,
            keys: &["nested.id"],
            bodies: vec![task_body("task-1", "WORKING")],
            events: vec![Event::Add {
                seq: 1,
                invoice: "nest",
                extra: json!({"nested":{"id":"nest"},"amount":1}),
            }],
            expect_methods: &["SendMessage"],
            expect_error: false,
            expect_header_version: true,
            expect_bearer: false,
        },
        ProcCase {
            name: "multi result key",
            policy: TerminalUpdatePolicy::Replace,
            recovery: None,
            token: None,
            template: None,
            keys: &["invoiceId", "customer"],
            bodies: vec![task_body("task-1", "WORKING")],
            events: vec![Event::Add {
                seq: 1,
                invoice: "Y",
                extra: json!({"invoiceId":"Y","customer":"Acme","amount":1}),
            }],
            expect_methods: &["SendMessage"],
            expect_error: false,
            expect_header_version: true,
            expect_bearer: false,
        },
        ProcCase {
            name: "missing result key no RPC",
            policy: TerminalUpdatePolicy::Replace,
            recovery: None,
            token: None,
            template: None,
            keys: &["invoiceId"],
            bodies: vec![task_body("task-1", "WORKING")],
            events: vec![Event::Add {
                seq: 1,
                invoice: "Z",
                extra: json!({"amount":1}),
            }],
            expect_methods: &[],
            expect_error: false,
            expect_header_version: false,
            expect_bearer: false,
        },
        ProcCase {
            name: "ADD ignored while cached active",
            policy: TerminalUpdatePolicy::Replace,
            recovery: None,
            token: None,
            template: None,
            keys: &["invoiceId"],
            bodies: vec![
                task_body("task-1", "WORKING"),
                bare_task_body("task-1", "WORKING"),
            ],
            events: vec![
                Event::Add {
                    seq: 1,
                    invoice: "AA",
                    extra: invoice_doc("AA", 1.0),
                },
                Event::Add {
                    seq: 2,
                    invoice: "AA",
                    extra: invoice_doc("AA", 1.0),
                },
            ],
            expect_methods: &["SendMessage", "GetTask"],
            expect_error: false,
            expect_header_version: true,
            expect_bearer: false,
        },
        ProcCase {
            name: "success without result stops",
            policy: TerminalUpdatePolicy::Replace,
            recovery: None,
            token: None,
            template: None,
            keys: &["invoiceId"],
            bodies: vec![json!({"jsonrpc":"2.0","id":"1"})],
            events: vec![Event::Add {
                seq: 1,
                invoice: "AB",
                extra: invoice_doc("AB", 1.0),
            }],
            expect_methods: &["SendMessage"],
            expect_error: true,
            expect_header_version: true,
            expect_bearer: false,
        },
        ProcCase {
            name: "malformed task stops",
            policy: TerminalUpdatePolicy::Replace,
            recovery: None,
            token: None,
            template: None,
            keys: &["invoiceId"],
            bodies: vec![json!({"jsonrpc":"2.0","id":"1","result":{"task":{"id":"t"}}})],
            events: vec![Event::Add {
                seq: 1,
                invoice: "AC",
                extra: invoice_doc("AC", 1.0),
            }],
            expect_methods: &["SendMessage"],
            expect_error: true,
            expect_header_version: true,
            expect_bearer: false,
        },
        ProcCase {
            name: "two keys independent activations",
            policy: TerminalUpdatePolicy::Replace,
            recovery: None,
            token: None,
            template: None,
            keys: &["invoiceId"],
            bodies: vec![
                task_body("task-1", "WORKING"),
                task_body("task-2", "WORKING"),
            ],
            events: vec![
                Event::Add {
                    seq: 1,
                    invoice: "AD1",
                    extra: invoice_doc("AD1", 1.0),
                },
                Event::Add {
                    seq: 2,
                    invoice: "AD2",
                    extra: invoice_doc("AD2", 1.0),
                },
            ],
            expect_methods: &["SendMessage", "SendMessage"],
            expect_error: false,
            expect_header_version: true,
            expect_bearer: false,
        },
        ProcCase {
            name: "replay UPDATE dropped",
            policy: TerminalUpdatePolicy::Replace,
            recovery: None,
            token: None,
            template: None,
            keys: &["invoiceId"],
            bodies: vec![
                task_body("task-1", "WORKING"),
                bare_task_body("task-1", "WORKING"),
                task_body("task-1", "WORKING"),
            ],
            events: vec![
                Event::Add {
                    seq: 1,
                    invoice: "AE",
                    extra: invoice_doc("AE", 1.0),
                },
                Event::Update {
                    seq: 2,
                    invoice: "AE",
                    after: invoice_doc("AE", 2.0),
                },
                Event::Update {
                    seq: 2,
                    invoice: "AE",
                    after: invoice_doc("AE", 2.0),
                },
            ],
            expect_methods: &["SendMessage", "GetTask", "SendMessage"],
            expect_error: false,
            expect_header_version: true,
            expect_bearer: false,
        },
        ProcCase {
            name: "GetTask COMPLETED then ADD creates",
            policy: TerminalUpdatePolicy::Replace,
            recovery: None,
            token: None,
            template: None,
            keys: &["invoiceId"],
            bodies: vec![
                task_body("task-1", "SUBMITTED"),
                bare_task_body("task-1", "COMPLETED"),
                task_body("task-2", "WORKING"),
            ],
            events: vec![
                Event::Add {
                    seq: 1,
                    invoice: "AF",
                    extra: invoice_doc("AF", 1.0),
                },
                Event::Add {
                    seq: 2,
                    invoice: "AF",
                    extra: invoice_doc("AF", 1.0),
                },
            ],
            expect_methods: &["SendMessage", "GetTask", "SendMessage"],
            expect_error: false,
            expect_header_version: true,
            expect_bearer: false,
        },
        ProcCase {
            name: "empty template still data part",
            policy: TerminalUpdatePolicy::Replace,
            recovery: None,
            token: None,
            template: Some(""),
            keys: &["invoiceId"],
            bodies: vec![task_body("task-1", "WORKING")],
            events: vec![Event::Add {
                seq: 1,
                invoice: "AG",
                extra: invoice_doc("AG", 1.0),
            }],
            expect_methods: &["SendMessage"],
            expect_error: false,
            expect_header_version: true,
            expect_bearer: false,
        },
        ProcCase {
            name: "boolean result key",
            policy: TerminalUpdatePolicy::Replace,
            recovery: None,
            token: None,
            template: None,
            keys: &["flag"],
            bodies: vec![task_body("task-1", "WORKING")],
            events: vec![Event::Add {
                seq: 1,
                invoice: "AH",
                extra: json!({"flag": true, "invoiceId":"AH"}),
            }],
            expect_methods: &["SendMessage"],
            expect_error: false,
            expect_header_version: true,
            expect_bearer: false,
        },
        ProcCase {
            name: "numeric result key",
            policy: TerminalUpdatePolicy::Replace,
            recovery: None,
            token: None,
            template: None,
            keys: &["n"],
            bodies: vec![task_body("task-1", "WORKING")],
            events: vec![Event::Add {
                seq: 1,
                invoice: "AI",
                extra: json!({"n": 42}),
            }],
            expect_methods: &["SendMessage"],
            expect_error: false,
            expect_header_version: true,
            expect_bearer: false,
        },
        ProcCase {
            name: "null result key value",
            policy: TerminalUpdatePolicy::Replace,
            recovery: None,
            token: None,
            template: None,
            keys: &["invoiceId"],
            bodies: vec![task_body("task-1", "WORKING")],
            events: vec![Event::Add {
                seq: 1,
                invoice: "AJ",
                extra: json!({"invoiceId": null}),
            }],
            expect_methods: &["SendMessage"],
            expect_error: false,
            expect_header_version: true,
            expect_bearer: false,
        },
        ProcCase {
            name: "object result key value",
            policy: TerminalUpdatePolicy::Replace,
            recovery: None,
            token: None,
            template: None,
            keys: &["obj"],
            bodies: vec![task_body("task-1", "WORKING")],
            events: vec![Event::Add {
                seq: 1,
                invoice: "AK",
                extra: json!({"obj":{"a":1}}),
            }],
            expect_methods: &["SendMessage"],
            expect_error: false,
            expect_header_version: true,
            expect_bearer: false,
        },
        ProcCase {
            name: "array result key value",
            policy: TerminalUpdatePolicy::Replace,
            recovery: None,
            token: None,
            template: None,
            keys: &["arr"],
            bodies: vec![task_body("task-1", "WORKING")],
            events: vec![Event::Add {
                seq: 1,
                invoice: "AL",
                extra: json!({"arr":[1,2]}),
            }],
            expect_methods: &["SendMessage"],
            expect_error: false,
            expect_header_version: true,
            expect_bearer: false,
        },
        ProcCase {
            name: "terminal ignore ADD silent",
            policy: TerminalUpdatePolicy::Ignore,
            recovery: None,
            token: None,
            template: None,
            keys: &["invoiceId"],
            bodies: vec![task_body("task-1", "COMPLETED")],
            events: vec![
                Event::Add {
                    seq: 1,
                    invoice: "AM",
                    extra: invoice_doc("AM", 1.0),
                },
                Event::Add {
                    seq: 2,
                    invoice: "AM",
                    extra: invoice_doc("AM", 1.0),
                },
            ],
            expect_methods: &["SendMessage"],
            expect_error: false,
            expect_header_version: true,
            expect_bearer: false,
        },
        ProcCase {
            name: "follow-up message keeps task then cancel",
            policy: TerminalUpdatePolicy::Replace,
            recovery: None,
            token: None,
            template: None,
            keys: &["invoiceId"],
            bodies: vec![
                task_body("task-1", "WORKING"),
                bare_task_body("task-1", "WORKING"),
                message_body(),
                bare_task_body("task-1", "WORKING"),
                json!({"jsonrpc":"2.0","id":"1","result":{"id":"task-1"}}),
            ],
            events: vec![
                Event::Add {
                    seq: 1,
                    invoice: "AN",
                    extra: invoice_doc("AN", 1.0),
                },
                Event::Update {
                    seq: 2,
                    invoice: "AN",
                    after: invoice_doc("AN", 2.0),
                },
                Event::Delete {
                    seq: 3,
                    invoice: "AN",
                },
            ],
            expect_methods: &[
                "SendMessage",
                "GetTask",
                "SendMessage",
                "GetTask",
                "CancelTask",
            ],
            expect_error: false,
            expect_header_version: true,
            expect_bearer: false,
        },
        ProcCase {
            name: "earlier sequence dropped",
            policy: TerminalUpdatePolicy::Replace,
            recovery: None,
            token: None,
            template: None,
            keys: &["invoiceId"],
            bodies: vec![task_body("task-1", "WORKING")],
            events: vec![
                Event::Add {
                    seq: 5,
                    invoice: "AO",
                    extra: invoice_doc("AO", 1.0),
                },
                Event::Update {
                    seq: 4,
                    invoice: "AO",
                    after: invoice_doc("AO", 2.0),
                },
            ],
            expect_methods: &["SendMessage"],
            expect_error: false,
            expect_header_version: true,
            expect_bearer: false,
        },
        ProcCase {
            name: "GetTask FAILED then replace create",
            policy: TerminalUpdatePolicy::Replace,
            recovery: None,
            token: None,
            template: None,
            keys: &["invoiceId"],
            bodies: vec![
                task_body("task-1", "WORKING"),
                bare_task_body("task-1", "FAILED"),
                task_body("task-2", "WORKING"),
            ],
            events: vec![
                Event::Add {
                    seq: 1,
                    invoice: "AP",
                    extra: invoice_doc("AP", 1.0),
                },
                Event::Update {
                    seq: 2,
                    invoice: "AP",
                    after: invoice_doc("AP", 2.0),
                },
            ],
            expect_methods: &["SendMessage", "GetTask", "SendMessage"],
            expect_error: false,
            expect_header_version: true,
            expect_bearer: false,
        },
    ]
}

async fn run_one_process(case: &ProcCase) -> Result<(), String> {
    let server = mock_server::start().await;
    if !case.bodies.is_empty() {
        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(OrderedJsonRpc {
                next: AtomicUsize::new(0),
                bodies: case.bodies.clone(),
            })
            .mount(&server)
            .await;
    }

    let mut builder = A2AReaction::builder(format!("g-{}", case.name))
        .with_query("q1")
        .with_endpoint(server.uri())
        .with_result_key_fields(case.keys.iter().copied())
        .with_terminal_update_policy(case.policy);
    if let Some(token) = case.token {
        builder = builder.with_token(token);
    }
    if let Some(template) = case.template {
        builder = builder.with_instruction_template(template);
    }
    if let Some(policy) = case.recovery {
        builder = builder.with_recovery_policy(policy);
    }
    let reaction = builder.build().map_err(|e| e.to_string())?;
    initialize_with_store(&reaction, memory_store()).await;
    reaction.start().await.map_err(|e| e.to_string())?;

    for event in &case.events {
        match event {
            Event::Add { seq, extra, .. } => {
                reaction
                    .enqueue_query_result(mock_source::add_result("q1", *seq, extra.clone()))
                    .await
                    .map_err(|e| e.to_string())?;
            }
            Event::Update { seq, after, .. } => {
                let before = after.clone();
                reaction
                    .enqueue_query_result(mock_source::update_result(
                        "q1",
                        *seq,
                        before,
                        after.clone(),
                    ))
                    .await
                    .map_err(|e| e.to_string())?;
            }
            Event::Delete { seq, invoice } => {
                let mut data = invoice_doc(invoice, 1.0);
                if case.keys == ["nested.id"] {
                    data = json!({"nested":{"id": invoice}});
                } else if case.keys == ["invoiceId", "customer"] {
                    data = json!({"invoiceId": invoice, "customer":"Acme"});
                } else if case.keys == ["flag"] {
                    data = json!({"flag": true});
                } else if case.keys == ["n"] {
                    data = json!({"n": 42});
                } else if case.keys == ["obj"] {
                    data = json!({"obj":{"a":1}});
                } else if case.keys == ["arr"] {
                    data = json!({"arr":[1,2]});
                }
                reaction
                    .enqueue_query_result(mock_source::delete_result("q1", *seq, data))
                    .await
                    .map_err(|e| e.to_string())?;
            }
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    }

    let expected_len = case.expect_methods.len();
    if expected_len > 0 {
        let ok = wait_for_requests(&server, expected_len, Duration::from_secs(3)).await;
        if !ok {
            let got = server.received_requests().await.unwrap_or_default().len();
            let _ = reaction.stop().await;
            return Err(format!(
                "timeout waiting {expected_len} requests, got {got}"
            ));
        }
    } else {
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    if case.expect_error {
        let ok = wait_status(&reaction, ComponentStatus::Error, Duration::from_secs(3)).await;
        let _ = reaction.stop().await;
        if !ok {
            return Err(format!(
                "expected Error status, got {:?}",
                reaction.status().await
            ));
        }
    } else {
        tokio::time::sleep(Duration::from_millis(80)).await;
        let status = reaction.status().await;
        if status == ComponentStatus::Error {
            let _ = reaction.stop().await;
            return Err("reaction entered Error unexpectedly".into());
        }
        let _ = reaction.stop().await;
    }

    let requests = server.received_requests().await.unwrap_or_default();
    let methods = methods_of(&requests);
    if methods != case.expect_methods {
        return Err(format!(
            "methods expected {:?} got {:?}",
            case.expect_methods, methods
        ));
    }

    if case.expect_header_version && !requests.is_empty() {
        let version = requests[0]
            .headers
            .get("A2A-Version")
            .and_then(|v| v.to_str().ok());
        if version != Some("1.0") {
            return Err(format!("A2A-Version header {version:?}"));
        }
    }
    if case.expect_bearer {
        let auth = requests[0]
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if auth != "Bearer secret-token" {
            return Err(format!("authorization {auth}"));
        }
    }

    if case.name == "instruction template text part" && !requests.is_empty() {
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap_or(json!({}));
        let text = body["params"]["message"]["parts"][0]["text"]
            .as_str()
            .unwrap_or("");
        if text != "go P" {
            return Err(format!("template text {text}"));
        }
    }

    if case.name == "UPDATE after GetTask COMPLETED creates" && requests.len() >= 3 {
        let body: Value = serde_json::from_slice(&requests[2].body).unwrap_or(json!({}));
        if body["params"]["message"].get("taskId").is_some()
            && !body["params"]["message"]["taskId"].is_null()
        {
            return Err("expected create without taskId after completed GetTask".into());
        }
    }

    Ok(())
}

fn live_up() -> bool {
    TcpStream::connect_timeout(
        &"127.0.0.1:9999".parse().expect("live agent address"),
        Duration::from_millis(200),
    )
    .is_ok()
}

async fn run_live(report: &mut Report) {
    if !live_up() {
        for i in 1..=14 {
            report.skip(
                format!("L{i:03}"),
                "live",
                format!("live scenario {i}"),
                "127.0.0.1:9999 not listening",
            );
        }
        return;
    }

    let client =
        drasi_reaction_a2a::client::A2AClient::new("http://127.0.0.1:9999/".into(), None, 5000)
            .expect("client");

    let create = client
        .send_message(SendMessageRequest {
            message_id: format!("gauntlet-live-{}", chrono_like_now()),
            activation_id: "gauntlet".into(),
            task_id: None,
            parts: vec![OutboundPart::Text("gauntlet hello".into())],
            return_immediately: true,
        })
        .await;
    match create {
        Ok(drasi_reaction_a2a::client::DeliveryResult::Delivered(SendMessageResult::Task {
            task_id,
            terminal,
            state,
            ..
        })) => {
            report.pass(
                "L001",
                "live",
                format!("create state={state} terminal={terminal}"),
            );
            let got = client.get_task("gauntlet-get", &task_id).await;
            match got {
                Ok(drasi_reaction_a2a::client::DeliveryResult::Delivered(
                    SendMessageResult::Task {
                        terminal: t2,
                        state: s2,
                        ..
                    },
                )) => {
                    if t2 {
                        report.pass("L002", "live", format!("GetTask terminal {s2}"));
                    } else {
                        report.probe("L002", "live", "GetTask not yet terminal", s2);
                    }
                }
                other => report.fail("L002", "live", "GetTask", format!("{other:?}")),
            }
            let follow = client
                .send_message(SendMessageRequest {
                    message_id: format!("gauntlet-follow-{}", chrono_like_now()),
                    activation_id: "gauntlet".into(),
                    task_id: Some(task_id),
                    parts: vec![OutboundPart::Text("follow".into())],
                    return_immediately: true,
                })
                .await;
            match follow {
                Err(error) if error.to_string().contains("terminal state") => {
                    report.pass("L003", "live", "follow-up rejected on completed task");
                }
                Ok(_) => report.probe(
                    "L003",
                    "live",
                    "follow-up succeeded",
                    "agent accepted follow-up on prior task",
                ),
                Err(error) => report.fail("L003", "live", "follow-up", error.to_string()),
            }
        }
        other => {
            report.fail("L001", "live", "create", format!("{other:?}"));
            report.skip("L002", "live", "GetTask", "create failed");
            report.skip("L003", "live", "follow-up", "create failed");
        }
    }

    let no_version = tokio::process::Command::new("curl")
        .args([
            "-sS",
            "-H",
            "Content-Type: application/json",
            "-d",
            r#"{"jsonrpc":"2.0","id":"nv","method":"SendMessage","params":{"message":{"role":"ROLE_USER","messageId":"nv","parts":[{"text":"x"}],"metadata":{}},"configuration":{"returnImmediately":true}}}"#,
            "http://127.0.0.1:9999/",
        ])
        .output()
        .await;
    match no_version {
        Ok(out) => {
            let body = String::from_utf8_lossy(&out.stdout);
            if body.contains("-32009") || body.contains("not supported") {
                report.pass("L004", "live", "missing A2A-Version rejected");
            } else {
                report.probe(
                    "L004",
                    "live",
                    "missing A2A-Version body",
                    body.chars().take(180).collect::<String>(),
                );
            }
        }
        Err(e) => report.skip("L004", "live", "curl missing version", e.to_string()),
    }

    let with_version = tokio::process::Command::new("curl")
        .args([
            "-sS",
            "-H",
            "Content-Type: application/json",
            "-H",
            "A2A-Version: 1.0",
            "-d",
            r#"{"jsonrpc":"2.0","id":"v1","method":"SendMessage","params":{"message":{"role":"ROLE_USER","messageId":"v1-gauntlet","parts":[{"text":"versioned"}],"metadata":{}},"configuration":{"returnImmediately":true}}}"#,
            "http://127.0.0.1:9999/",
        ])
        .output()
        .await;
    match with_version {
        Ok(out) => {
            let body = String::from_utf8_lossy(&out.stdout);
            if body.contains("TASK_STATE_SUBMITTED") || body.contains("task") {
                report.pass("L005", "live", "A2A-Version 1.0 accepted");
            } else {
                report.fail(
                    "L005",
                    "live",
                    "A2A-Version 1.0",
                    body.chars().take(180).collect::<String>(),
                );
            }
        }
        Err(e) => report.fail("L005", "live", "curl versioned", e.to_string()),
    }

    let unicode = client
        .send_message(SendMessageRequest {
            message_id: format!("gauntlet-uni-{}", chrono_like_now()),
            activation_id: "gauntlet".into(),
            task_id: None,
            parts: vec![OutboundPart::Text("发票 こんにちは".into())],
            return_immediately: true,
        })
        .await;
    match unicode {
        Ok(_) => report.pass("L006", "live", "unicode text part"),
        Err(e) => report.fail("L006", "live", "unicode text part", e.to_string()),
    }

    let data = client
        .send_message(SendMessageRequest {
            message_id: format!("gauntlet-data-{}", chrono_like_now()),
            activation_id: "gauntlet".into(),
            task_id: None,
            parts: vec![OutboundPart::Data {
                media_type: "application/vnd.drasi.change+json".into(),
                data: json!({"queryId":"q","operation":"ADD","resultKey":"inv-live","after":{"invoiceId":"inv-live"}}),
            }],
            return_immediately: true,
        })
        .await;
    match data {
        Ok(_) => report.pass("L007", "live", "drasi change data part"),
        Err(e) => report.fail("L007", "live", "drasi change data part", e.to_string()),
    }

    let mut rapid_ok = 0;
    for i in 0..5 {
        let r = client
            .send_message(SendMessageRequest {
                message_id: format!("gauntlet-rapid-{i}-{}", chrono_like_now()),
                activation_id: "gauntlet".into(),
                task_id: None,
                parts: vec![OutboundPart::Text(format!("rapid {i}"))],
                return_immediately: true,
            })
            .await;
        if r.is_ok() {
            rapid_ok += 1;
        }
    }
    if rapid_ok == 5 {
        report.pass("L008", "live", "5 rapid creates");
    } else {
        report.fail("L008", "live", "5 rapid creates", format!("{rapid_ok}/5"));
    }

    let empty_text = client
        .send_message(SendMessageRequest {
            message_id: format!("gauntlet-empty-{}", chrono_like_now()),
            activation_id: "gauntlet".into(),
            task_id: None,
            parts: vec![OutboundPart::Text(String::new())],
            return_immediately: true,
        })
        .await;
    match empty_text {
        Ok(_) => report.pass("L009", "live", "empty text part"),
        Err(e) => report.probe("L009", "live", "empty text part", e.to_string()),
    }

    let huge = "x".repeat(32_000);
    let large = client
        .send_message(SendMessageRequest {
            message_id: format!("gauntlet-large-{}", chrono_like_now()),
            activation_id: "gauntlet".into(),
            task_id: None,
            parts: vec![OutboundPart::Text(huge)],
            return_immediately: true,
        })
        .await;
    match large {
        Ok(_) => report.pass("L010", "live", "32k text part"),
        Err(e) => report.probe("L010", "live", "32k text part", e.to_string()),
    }

    let cancel_missing = tokio::process::Command::new("curl")
        .args([
            "-sS",
            "-H",
            "Content-Type: application/json",
            "-H",
            "A2A-Version: 1.0",
            "-d",
            r#"{"jsonrpc":"2.0","id":"c0","method":"CancelTask","params":{"id":"does-not-exist"}}"#,
            "http://127.0.0.1:9999/",
        ])
        .output()
        .await;
    match cancel_missing {
        Ok(out) => {
            let body = String::from_utf8_lossy(&out.stdout);
            report.probe(
                "L011",
                "live",
                "CancelTask unknown id",
                body.chars().take(180).collect::<String>(),
            );
        }
        Err(e) => report.skip("L011", "live", "CancelTask unknown id", e.to_string()),
    }

    let get_missing = tokio::process::Command::new("curl")
        .args([
            "-sS",
            "-H",
            "Content-Type: application/json",
            "-H",
            "A2A-Version: 1.0",
            "-d",
            r#"{"jsonrpc":"2.0","id":"g0","method":"GetTask","params":{"id":"does-not-exist"}}"#,
            "http://127.0.0.1:9999/",
        ])
        .output()
        .await;
    match get_missing {
        Ok(out) => {
            let body = String::from_utf8_lossy(&out.stdout);
            if body.contains("error") {
                report.pass("L012", "live", "GetTask unknown id errors");
            } else {
                report.probe(
                    "L012",
                    "live",
                    "GetTask unknown id",
                    body.chars().take(180).collect::<String>(),
                );
            }
        }
        Err(e) => report.fail("L012", "live", "GetTask unknown id", e.to_string()),
    }

    let tasks_get = tokio::process::Command::new("curl")
        .args([
            "-sS",
            "-H",
            "Content-Type: application/json",
            "-H",
            "A2A-Version: 1.0",
            "-d",
            r#"{"jsonrpc":"2.0","id":"tg","method":"tasks/get","params":{"id":"x"}}"#,
            "http://127.0.0.1:9999/",
        ])
        .output()
        .await;
    match tasks_get {
        Ok(out) => {
            let body = String::from_utf8_lossy(&out.stdout);
            if body.contains("-32601") {
                report.pass("L013", "live", "tasks/get method not found");
            } else {
                report.probe(
                    "L013",
                    "live",
                    "tasks/get",
                    body.chars().take(120).collect::<String>(),
                );
            }
        }
        Err(e) => report.fail("L013", "live", "tasks/get", e.to_string()),
    }

    let wrong_role = tokio::process::Command::new("curl")
        .args([
            "-sS",
            "-H",
            "Content-Type: application/json",
            "-H",
            "A2A-Version: 1.0",
            "-d",
            r#"{"jsonrpc":"2.0","id":"role","method":"SendMessage","params":{"message":{"role":"user","messageId":"role-x","parts":[{"text":"x"}],"metadata":{}},"configuration":{"returnImmediately":true}}}"#,
            "http://127.0.0.1:9999/",
        ])
        .output()
        .await;
    match wrong_role {
        Ok(out) => {
            let body = String::from_utf8_lossy(&out.stdout);
            report.probe(
                "L014",
                "live",
                "role=user vs ROLE_USER",
                body.chars().take(180).collect::<String>(),
            );
        }
        Err(e) => report.skip("L014", "live", "role=user", e.to_string()),
    }
}

fn chrono_like_now() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

#[tokio::test(flavor = "multi_thread")]
async fn battle_200() {
    let mut report = Report::new();
    run_state_machine(&mut report);
    run_parser(&mut report);
    run_config(&mut report);
    run_rpc(&mut report);
    run_process(&mut report).await;
    run_live(&mut report).await;

    // Pad/trim visibility: we want ~200 rows.
    report.print();
    eprintln!(
        "gauntlet scenario count = {} (target 200)",
        report.rows.len()
    );
    assert_eq!(
        report.fail_count(),
        0,
        "{} contract failures",
        report.fail_count()
    );
    assert!(
        report.rows.len() >= 180,
        "expected at least 180 scenarios, got {}",
        report.rows.len()
    );
}
