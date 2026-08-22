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

#![allow(clippy::unwrap_used)]

use async_trait::async_trait;
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
};
use drasi_lib::bootstrap::{
    BootstrapContext, BootstrapProvider, BootstrapRequest, BootstrapResult,
};
use drasi_lib::channels::events::{
    BootstrapEvent, BootstrapEventSender, SourceEvent, SourceEventWrapper,
};
use drasi_lib::channels::ChangeReceiver;
use drasi_lib::component_graph::ComponentUpdateSender;
use drasi_lib::config::SourceSubscriptionSettings;
use drasi_lib::context::SourceRuntimeContext;
use drasi_lib::state_store::StateStoreProvider;
use drasi_lib::wal::{CapacityPolicy, WalProvider};
use drasi_lib::{DurabilityConfig, Source};
use drasi_source_github_workgraph::config::{
    AgentConfig, GitHubWorkGraphSourceConfig, LeaseTrust, TaskIssueType, TrustedIdentity,
    WebhookConfig,
};
use drasi_source_github_workgraph::lease_ledger::{AllocationArtifact, AllocationEvent};
use drasi_source_github_workgraph::mapping::Converter;
use drasi_source_github_workgraph::source::{GitHubWorkGraphSource, GitHubWorkGraphSourceBuilder};
use drasi_state_store_redb::RedbStateStoreProvider;
use drasi_wal_redb::RedbWalProvider;
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    sync::{atomic::Ordering, Arc},
    time::Duration,
};
use tempfile::TempDir;

const ID: &str = "gh-workgraph-it";
const SECRET: &str = "webhook-secret";
const VALIDATION_TOKEN: &str = "lease-validation-token";
const PER_ISSUE: usize = 4;

struct Harness {
    _tmp: TempDir,
    source: GitHubWorkGraphSource,
    wal: Arc<dyn WalProvider>,
    store: Arc<dyn StateStoreProvider>,
    url: String,
    port: u16,
}

impl Harness {
    async fn new(max_events: u64) -> Self {
        let tmp = TempDir::new().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let wal: Arc<dyn WalProvider> = Arc::new(RedbWalProvider::new(tmp.path().join("wal")));
        let store: Arc<dyn StateStoreProvider> =
            Arc::new(RedbStateStoreProvider::new(tmp.path().join("state")).unwrap());
        let harness = Self {
            _tmp: tmp,
            source: Self::build(max_events, port),
            wal,
            store,
            url: format!("http://127.0.0.1:{port}/webhook"),
            port,
        };
        harness.initialize(&harness.source).await;
        harness.source.start().await.unwrap();
        harness
    }
    fn build(max_events: u64, port: u16) -> GitHubWorkGraphSource {
        let config = GitHubWorkGraphSourceConfig {
            organization: "acme".into(),
            task_issue_type: TaskIssueType {
                id: "IT_test".into(),
                name: "WorkGraphTask".into(),
            },
            repositories: Vec::new(),
            agent_config: None,
            lease_trust: None,
            webhook: WebhookConfig {
                host: "127.0.0.1".into(),
                port,
                secret: SECRET.into(),
                lease_validation_token: VALIDATION_TOKEN.into(),
                ..WebhookConfig::default()
            },
            durability: DurabilityConfig {
                enabled: true,
                max_events,
                capacity_policy: CapacityPolicy::RejectIncoming,
            },
        };
        GitHubWorkGraphSourceBuilder::new(ID)
            .with_config(config)
            .build()
            .unwrap()
    }
    async fn initialize(&self, source: &GitHubWorkGraphSource) {
        let (tx, _rx): (ComponentUpdateSender, _) = tokio::sync::mpsc::channel(8);
        let mut context =
            SourceRuntimeContext::new("gh-it", ID, Some(self.store.clone()), tx, None);
        context.wal_provider = Some(self.wal.clone());
        source.initialize(context).await;
    }
    async fn start(&self, source: &GitHubWorkGraphSource) {
        self.initialize(source).await;
        source.start().await.unwrap();
    }
    async fn post_raw(&self, headers: &[(&str, &str)], body: &str, sign: bool) -> u16 {
        let client = reqwest::Client::new();
        let mut request = client.post(&self.url).body(body.to_owned());
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        if sign {
            let mut mac = Hmac::<Sha256>::new_from_slice(SECRET.as_bytes()).unwrap();
            mac.update(body.as_bytes());
            request = request.header(
                "x-hub-signature-256",
                format!("sha256={}", hex::encode(mac.finalize().into_bytes())),
            );
        }
        request.send().await.unwrap().status().as_u16()
    }
    async fn post(&self, event: &str, delivery: &str, payload: &Value) -> u16 {
        self.post_raw(
            &[("x-github-event", event), ("x-github-delivery", delivery)],
            &payload.to_string(),
            true,
        )
        .await
    }
}

fn settings(query: &str, resume: Option<u64>, bootstrap: bool) -> SourceSubscriptionSettings {
    SourceSubscriptionSettings {
        source_id: ID.into(),
        enable_bootstrap: bootstrap,
        query_id: query.into(),
        nodes: HashSet::new(),
        relations: HashSet::new(),
        resume_from: resume.map(|n| bytes::Bytes::copy_from_slice(&n.to_be_bytes())),
        request_position_handle: true,
    }
}
fn issue(node: &str) -> Value {
    json!({"action":"opened","organization":{"login":"acme","node_id":"O_1"},
        "repository":{"node_id":"R_7","full_name":"acme/w","owner":{"login":"acme"}},
        "issue":{"node_id":node,"id":1,"number":1,"title":"t","state":"open","labels":[],
        "user":{"login":"ada","node_id":"U_1","id":1,"type":"User"}}})
}
async fn drain(
    receiver: &mut Box<dyn ChangeReceiver<SourceEventWrapper>>,
    count: usize,
) -> Vec<(u64, String)> {
    let mut seen = Vec::new();
    while seen.len() < count {
        let event = tokio::time::timeout(Duration::from_secs(5), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        if let SourceEvent::Change(change) = &event.event {
            seen.push((
                event.sequence.unwrap(),
                change.get_reference().element_id.to_string(),
            ));
        }
    }
    seen
}

struct StubBootstrap;
#[async_trait]
impl BootstrapProvider for StubBootstrap {
    async fn bootstrap(
        &self,
        _request: BootstrapRequest,
        context: &BootstrapContext,
        tx: BootstrapEventSender,
        _settings: Option<&SourceSubscriptionSettings>,
    ) -> anyhow::Result<BootstrapResult> {
        let element = Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new(ID, "I_bootstrapped"),
                labels: Arc::from(vec![Arc::from("GitHubIssue")]),
                effective_from: 1,
            },
            properties: ElementPropertyMap::new(),
        };
        tx.send(BootstrapEvent {
            source_id: ID.into(),
            change: SourceChange::Insert { element },
            timestamp: chrono::Utc::now(),
            sequence: context.next_sequence(),
        })
        .await?;
        Ok(BootstrapResult {
            event_count: 1,
            source_position: None,
        })
    }
}

#[tokio::test]
#[ignore]
async fn signed_ingress_is_durable_deduplicated_ordered_and_capacity_bounded() {
    let h = Harness::new(10_000).await;
    let mut subscription = h
        .source
        .subscribe(settings("q", None, false))
        .await
        .unwrap();
    let payload = issue("I_1");
    let body = payload.to_string();
    let event = ("x-github-event", "issues");
    let delivery = ("x-github-delivery", "d");
    assert_eq!(h.post_raw(&[event, delivery], &body, false).await, 401);
    assert_eq!(h.post_raw(&[event], &body, true).await, 400);
    assert_eq!(h.post_raw(&[event, delivery], "bad", true).await, 400);
    assert_eq!(h.post("ping", "ping", &json!({})).await, 204);
    assert_eq!(
        h.post("pull_request_review_comment", "x", &payload).await,
        204
    );
    let mut wrong = payload.clone();
    wrong["organization"]["login"] = json!("other");
    assert_eq!(h.post("issues", "wrong", &wrong).await, 403);
    let mut broken = payload.clone();
    broken["issue"].as_object_mut().unwrap().remove("node_id");
    assert_eq!(h.post("issues", "broken", &broken).await, 422);
    assert_eq!(h.wal.event_count(ID).await.unwrap(), 0);

    assert_eq!(h.post("issues", "d1", &payload).await, 202);
    assert_eq!(h.wal.event_count(ID).await.unwrap(), 4);
    assert_eq!(h.post("issues", "d1", &payload).await, 202);
    assert_eq!(h.post("issues", "d2", &issue("I_2")).await, 202);
    assert_eq!(h.wal.event_count(ID).await.unwrap(), 8);
    let seen = drain(&mut subscription.receiver, 8).await;
    assert_eq!(seen[0], (1, "I_1".into()));
    assert_eq!(seen[4], (5, "I_2".into()));
    assert_eq!(
        seen.iter().map(|v| v.0).collect::<Vec<_>>(),
        (1..=8).collect::<Vec<_>>()
    );
    h.source.stop().await.unwrap();

    let full = Harness::new(16).await;
    for n in 0..4 {
        assert_eq!(
            full.post("issues", &format!("d{n}"), &issue("I")).await,
            202
        );
    }
    assert_eq!(full.post("issues", "overflow", &issue("J")).await, 503);
    assert_eq!(full.wal.event_count(ID).await.unwrap(), 16);
    full.source.stop().await.unwrap();
    let unstarted = Harness::build(16, full.port);
    full.initialize(&unstarted).await;
    unstarted.deprovision().await.unwrap();
    assert!(full.wal.event_count(ID).await.is_err());
    assert_eq!(full.store.key_count(ID).await.unwrap(), 0);
}

#[tokio::test]
#[ignore]
async fn u64_replay_pruning_restart_and_external_bootstrap_handoff_work() {
    let h = Harness::new(10_000).await;
    h.post("issues", "d1", &issue("I_1")).await;
    h.post("issues", "d2", &issue("I_2")).await;
    assert!(h
        .source
        .subscribe(settings("ahead", Some(9), false))
        .await
        .is_err());
    let mut resumed = h
        .source
        .subscribe(settings("resume", Some(4), false))
        .await
        .unwrap();
    assert_eq!(
        drain(&mut resumed.receiver, PER_ISSUE).await[0],
        (5, "I_2".into())
    );
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    assert_eq!(h.wal.event_count(ID).await.unwrap(), 4);
    h.wal.prune_up_to(ID, 8).await.unwrap();
    let gap = h.source.subscribe(settings("gap", Some(0), false)).await;
    assert!(gap
        .err()
        .unwrap()
        .to_string()
        .contains("position unavailable"));
    h.source.stop().await.unwrap();

    let restarted = Harness::build(10_000, h.port);
    h.start(&restarted).await;
    let mut replayed = restarted
        .subscribe(settings("restart", Some(8), false))
        .await
        .unwrap();
    h.post("issues", "d3", &issue("I_3")).await;
    assert_eq!(drain(&mut replayed.receiver, PER_ISSUE).await[0].1, "I_3");
    restarted.stop().await.unwrap();

    let bootstrap = Harness::new(10_000).await;
    bootstrap
        .source
        .set_bootstrap_provider(Box::new(StubBootstrap))
        .await;
    let mut subscription = bootstrap
        .source
        .subscribe(settings("bootstrap", None, true))
        .await
        .unwrap();
    let mut receiver = subscription.bootstrap_receiver.take().unwrap();
    let first = tokio::time::timeout(Duration::from_secs(5), receiver.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        first.change.get_reference().element_id.as_ref(),
        "I_bootstrapped"
    );
    bootstrap.post("issues", "live", &issue("I_live")).await;
    assert_eq!(
        drain(&mut subscription.receiver, PER_ISSUE).await[0].1,
        "I_live"
    );
    bootstrap.source.stop().await.unwrap();
}

// ---------------------------------------------------------------------------
// Agent capacity convergence over a live webhook `push` delivery.
// ---------------------------------------------------------------------------

const AGENT_PATH: &str = ".github/workgraph/agents.yaml";

fn agent_file(slots: u32) -> String {
    format!(
        "version: 1\nagents:\n  - agentId: issue-validator\n    \
         slots: {slots}\n    leaseDuration: PT15M\n"
    )
}

/// Mount the agent-file blob query so every fetch returns `text`.
async fn mount_agent_blob(server: &wiremock::MockServer, text: &str) {
    server.reset().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/graphql"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({
            "data": { "repository": { "object": {
                "__typename": "Blob",
                "oid": "blob-oid",
                "text": text,
                "byteSize": text.len(),
                "isTruncated": false,
                "isBinary": false,
            }}}
        })))
        .mount(server)
        .await;
}

fn push(git_ref: &str, repository: &str, touched: &str) -> Value {
    json!({
        "ref": git_ref,
        "organization": {"login": "acme", "node_id": "O_1"},
        "repository": {"node_id": "R_7", "full_name": repository, "owner": {"login": "acme"}},
        "size": 1,
        "commits": [{"added": [], "modified": [touched], "removed": []}],
        "head_commit": {"added": [], "modified": [touched], "removed": []},
    })
}

async fn agent_harness(server: &wiremock::MockServer) -> Harness {
    let tmp = TempDir::new().unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let wal: Arc<dyn WalProvider> = Arc::new(RedbWalProvider::new(tmp.path().join("wal")));
    let store: Arc<dyn StateStoreProvider> =
        Arc::new(RedbStateStoreProvider::new(tmp.path().join("state")).unwrap());
    let config = GitHubWorkGraphSourceConfig {
        organization: "acme".into(),
        task_issue_type: TaskIssueType {
            id: "IT_test".into(),
            name: "WorkGraphTask".into(),
        },
        repositories: Vec::new(),
        lease_trust: Some(LeaseTrust {
            dispatchers: vec![TrustedIdentity {
                id: "U_assigner".into(),
                login: "assigner".into(),
            }],
            reporters: vec![TrustedIdentity {
                id: "U_reporter".into(),
                login: "reporter".into(),
            }],
        }),
        agent_config: Some(AgentConfig {
            repository: "acme/w".into(),
            r#ref: "main".into(),
            path: AGENT_PATH.into(),
            token: "read-only-token".into(),
            api_base_url: format!("{}/graphql", server.uri()),
        }),
        webhook: WebhookConfig {
            host: "127.0.0.1".into(),
            port,
            secret: SECRET.into(),
            lease_validation_token: VALIDATION_TOKEN.into(),
            ..WebhookConfig::default()
        },
        durability: DurabilityConfig {
            enabled: true,
            max_events: 4096,
            capacity_policy: CapacityPolicy::RejectIncoming,
        },
    };
    let harness = Harness {
        _tmp: tmp,
        source: GitHubWorkGraphSourceBuilder::new(ID)
            .with_config(config)
            .build()
            .unwrap(),
        wal,
        store,
        url: format!("http://127.0.0.1:{port}/webhook"),
        port,
    };
    harness.initialize(&harness.source).await;
    harness.source.start().await.unwrap();
    harness
}

async fn wal_ids(harness: &Harness, from: u64) -> Vec<String> {
    harness
        .wal
        .read_from(ID, from)
        .await
        .unwrap()
        .into_iter()
        .map(|(_, change)| change.get_reference().element_id.to_string())
        .collect()
}

async fn wal_changes(harness: &Harness, from: u64) -> Vec<SourceChange> {
    harness
        .wal
        .read_from(ID, from)
        .await
        .unwrap()
        .into_iter()
        .map(|(_, change)| change)
        .collect()
}

fn change_label(change: &SourceChange) -> &str {
    let metadata = match change {
        SourceChange::Delete { metadata } => metadata,
        SourceChange::Insert { element } | SourceChange::Update { element } => {
            element.get_metadata()
        }
        SourceChange::Future { .. } => panic!("unexpected future change"),
    };
    &metadata.labels[0]
}

fn change_id(change: &SourceChange) -> &str {
    change.get_reference().element_id.as_ref()
}

fn node_property<'a>(
    changes: &'a [SourceChange],
    node_id: &str,
    key: &str,
) -> Option<&'a ElementValue> {
    changes.iter().rev().find_map(|change| match change {
        SourceChange::Insert {
            element:
                Element::Node {
                    metadata,
                    properties,
                },
        }
        | SourceChange::Update {
            element:
                Element::Node {
                    metadata,
                    properties,
                },
        } if metadata.reference.element_id.as_ref() == node_id => properties.get(key),
        _ => None,
    })
}

fn assignment_body(agent_id: &str) -> String {
    format!("WorkGraphTaskAssignment/v1\n\n```json\n{{\n  \"agentId\": \"{agent_id}\"\n}}\n```\n")
}

fn result_body(task_type: &str, lease_id: &str) -> String {
    match task_type {
        "validate-issue" => format!(
            "WorkGraphTaskResult/v1\n\n```json\n{{\n  \"taskType\": \"validate-issue\",\n  \
             \"leaseId\": \"{lease_id}\",\n  \"outcome\": \"succeeded\",\n  \"summary\": \
             \"Validated the issue.\",\n  \"result\": {{\n    \"criteria\": [\n      {{\n        \
             \"criterion\": \"Acceptance criteria\",\n        \"passed\": true,\n        \
             \"evidence\": \"Present.\"\n      }}\n    ]\n  }}\n}}\n```\n"
        ),
        "request-info" => format!(
            "WorkGraphTaskResult/v1\n\n```json\n{{\n  \"taskType\": \"request-info\",\n  \
             \"leaseId\": \"{lease_id}\",\n  \"outcome\": \"succeeded\",\n  \"summary\": \
             \"Requested information.\",\n  \"result\": {{\n    \"requestCommentNodeId\": \
             \"IC_request\"\n  }}\n}}\n```\n"
        ),
        _ => panic!("unsupported task type"),
    }
}

fn task_comment(
    task_id: &str,
    task_state: &str,
    comment_id: &str,
    body: &str,
    author_login: &str,
    author_id: &str,
) -> Value {
    let task_body = r#"WorkGraphTask/v1

```yaml
taskType: validate-issue
inputs:
  validationProfile: new-issue-default
```
"#;
    json!({
        "action": "created",
        "organization": {"login": "acme", "node_id": "O_1"},
        "repository": {
            "node_id": "R_7", "name": "w", "full_name": "acme/w",
            "owner": {"login": "acme"}
        },
        "issue": {
            "node_id": task_id, "id": 1, "number": 1, "title": "t",
            "body": task_body,
            "state": task_state, "labels": [],
            "type": {"node_id": "IT_test", "name": "WorkGraphTask"},
            "user": {"login": "ada", "node_id": "U_1", "id": 1, "type": "User"}
        },
        "comment": {
            "node_id": comment_id, "id": 9, "body": body,
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "html_url": format!("https://github.com/acme/w/issues/1#{comment_id}"),
            "user": {
                "login": author_login, "node_id": author_id, "id": 2, "type": "User"
            }
        }
    })
}

fn assert_agent_counts(
    changes: &[SourceChange],
    queue_depth: i64,
    active_lease_count: i64,
    available_slot_count: i64,
) {
    let agent = "workgraph-agent:issue-validator";
    assert_eq!(
        node_property(changes, agent, "queueDepth"),
        Some(&ElementValue::Integer(queue_depth))
    );
    assert_eq!(
        node_property(changes, agent, "activeLeaseCount"),
        Some(&ElementValue::Integer(active_lease_count))
    );
    assert_eq!(
        node_property(changes, agent, "availableSlotCount"),
        Some(&ElementValue::Integer(available_slot_count))
    );
}

async fn validate_lease(harness: &Harness, request: &Value) -> u16 {
    reqwest::Client::new()
        .post(format!("{}/lease/validate", harness.url))
        .bearer_auth(VALIDATION_TOKEN)
        .json(request)
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

#[tokio::test]
async fn signed_source_contract_capacity_gates_leases_not_queue() {
    let server = wiremock::MockServer::start().await;
    mount_agent_blob(&server, &agent_file(1)).await;
    let harness = agent_harness(&server).await;

    let assignment_a = task_comment(
        "I_A",
        "open",
        "IC_A",
        &assignment_body("issue-validator"),
        "assigner",
        "U_assigner",
    );
    let before_a = harness.wal.head_sequence(ID).await.unwrap();
    assert_eq!(
        harness
            .post("issue_comment", "assignment-a", &assignment_a)
            .await,
        202
    );
    let changes_a = wal_changes(&harness, before_a + 1).await;
    assert_eq!(
        node_property(&changes_a, "IC_A", "trusted"),
        Some(&ElementValue::Bool(true))
    );
    let lease_a = hex::encode(Sha256::digest(b"I_A\0IC_A\0\x31"));
    assert!(changes_a.iter().any(|change| {
        change_id(change) == format!("workgraph-lease:I_A:{lease_a}")
            && change_label(change) == "WorkGraphTaskLease"
    }));
    assert_agent_counts(&changes_a, 0, 1, 0);

    let assignment_b = task_comment(
        "I_B",
        "open",
        "IC_B",
        &assignment_body("issue-validator"),
        "assigner",
        "U_assigner",
    );
    let before_b = harness.wal.head_sequence(ID).await.unwrap();
    assert_eq!(
        harness
            .post("issue_comment", "assignment-b", &assignment_b)
            .await,
        202
    );
    let changes_b = wal_changes(&harness, before_b + 1).await;
    assert!(!changes_b.iter().any(|change| {
        change_label(change) == "WorkGraphTaskLease"
            && matches!(
                change,
                SourceChange::Insert { .. } | SourceChange::Update { .. }
            )
    }));
    assert_agent_counts(&changes_b, 1, 1, 0);

    let untrusted = task_comment(
        "I_untrusted",
        "open",
        "IC_untrusted",
        &assignment_body("issue-validator"),
        "mallory",
        "U_mallory",
    );
    let before = harness.wal.head_sequence(ID).await.unwrap();
    assert_eq!(
        harness
            .post("issue_comment", "assignment-untrusted", &untrusted)
            .await,
        202
    );
    let changes = wal_changes(&harness, before + 1).await;
    assert_eq!(
        node_property(&changes, "IC_untrusted", "trusted"),
        Some(&ElementValue::Bool(false))
    );
    assert!(!changes
        .iter()
        .any(|change| change_label(change) == "WorkGraphTaskLease"));

    let malformed = task_comment(
        "I_malformed",
        "open",
        "IC_malformed",
        "WorkGraphTaskAssignment/v1\n\n```json\n{}\n```\n",
        "assigner",
        "U_assigner",
    );
    let before = harness.wal.head_sequence(ID).await.unwrap();
    assert_eq!(
        harness
            .post("issue_comment", "assignment-malformed", &malformed)
            .await,
        202
    );
    let changes = wal_changes(&harness, before + 1).await;
    assert!(changes
        .iter()
        .any(|change| change_label(change) == "WorkGraphError"));
    assert!(!changes
        .iter()
        .any(|change| change_label(change) == "WorkGraphTaskLease"));

    let unknown = task_comment(
        "I_unknown",
        "open",
        "IC_unknown",
        &assignment_body("issue-risk-profiler"),
        "assigner",
        "U_assigner",
    );
    let before = harness.wal.head_sequence(ID).await.unwrap();
    assert_eq!(
        harness
            .post("issue_comment", "assignment-unknown", &unknown)
            .await,
        202
    );
    let changes = wal_changes(&harness, before + 1).await;
    assert_eq!(
        node_property(&changes, "IC_unknown", "trusted"),
        Some(&ElementValue::Bool(false))
    );
    assert!(changes.iter().any(|change| {
        change_label(change) == "ASSIGNED_TO"
            && change_id(change) == "ASSIGNED_TO:IC_unknown:workgraph-agent:issue-risk-profiler"
    }));
    assert!(!changes
        .iter()
        .any(|change| change_label(change) == "WorkGraphTaskLease"));
    let stored = harness
        .store
        .get(ID, "allocator:state")
        .await
        .unwrap()
        .unwrap();
    let state: Value = serde_json::from_slice(&stored).unwrap();
    assert_eq!(state["queue"].as_object().unwrap().len(), 2);
    assert!(state["queue"].get("IC_unknown").is_none());
    assert!(state["comments"].get("IC_unknown").is_none());
    assert_eq!(state["active"].as_object().unwrap().len(), 1);
    let unknown_lease = hex::encode(Sha256::digest(b"I_unknown\0IC_unknown\0\x31"));
    assert_eq!(
        validate_lease(
            &harness,
            &json!({
                "taskNodeId": "I_unknown",
                "leaseId": unknown_lease,
                "assignmentCommentNodeId": "IC_unknown",
                "agentId": "issue-risk-profiler",
                "slotId": "issue-risk-profiler/1"
            }),
        )
        .await,
        409
    );

    let closed = task_comment(
        "I_closed",
        "closed",
        "IC_closed",
        &assignment_body("issue-validator"),
        "assigner",
        "U_assigner",
    );
    let before = harness.wal.head_sequence(ID).await.unwrap();
    assert_eq!(
        harness
            .post("issue_comment", "assignment-closed", &closed)
            .await,
        202
    );
    let changes = wal_changes(&harness, before + 1).await;
    assert_eq!(
        node_property(&changes, "IC_closed", "trusted"),
        Some(&ElementValue::Bool(false))
    );
    assert!(!changes
        .iter()
        .any(|change| change_label(change) == "WorkGraphTaskLease"));

    harness.source.stop().await.unwrap();
}

#[tokio::test]
async fn signed_source_contract_exact_result_releases_and_refills() {
    let server = wiremock::MockServer::start().await;
    mount_agent_blob(&server, &agent_file(1)).await;
    let harness = agent_harness(&server).await;
    for (delivery, task, comment) in [
        ("assignment-a", "I_A", "IC_A"),
        ("assignment-b", "I_B", "IC_B"),
    ] {
        let payload = task_comment(
            task,
            "open",
            comment,
            &assignment_body("issue-validator"),
            "assigner",
            "U_assigner",
        );
        assert_eq!(harness.post("issue_comment", delivery, &payload).await, 202);
    }
    let old_lease = hex::encode(Sha256::digest(b"I_A\0IC_A\0\x31"));
    let old_request = json!({
        "taskNodeId": "I_A",
        "leaseId": old_lease.clone(),
        "assignmentCommentNodeId": "IC_A",
        "agentId": "issue-validator",
        "slotId": "issue-validator/1"
    });

    let rejected = [
        (
            "untrusted-result",
            task_comment(
                "I_A",
                "open",
                "IC_untrusted_result",
                &result_body("validate-issue", &old_lease),
                "mallory",
                "U_mallory",
            ),
        ),
        (
            "wrong-lease-result",
            task_comment(
                "I_A",
                "open",
                "IC_wrong_lease",
                &result_body("validate-issue", "wrong-lease"),
                "reporter",
                "U_reporter",
            ),
        ),
        (
            "wrong-task-result",
            task_comment(
                "I_other",
                "open",
                "IC_wrong_task",
                &result_body("validate-issue", &old_lease),
                "reporter",
                "U_reporter",
            ),
        ),
        (
            "wrong-type-result",
            task_comment(
                "I_A",
                "open",
                "IC_wrong_type",
                &result_body("request-info", &old_lease),
                "reporter",
                "U_reporter",
            ),
        ),
    ];
    for (delivery, payload) in rejected {
        let before = harness.wal.head_sequence(ID).await.unwrap();
        assert_eq!(harness.post("issue_comment", delivery, &payload).await, 202);
        let changes = wal_changes(&harness, before + 1).await;
        let result_id = payload["comment"]["node_id"].as_str().unwrap();
        assert_eq!(
            node_property(&changes, result_id, "trusted"),
            Some(&ElementValue::Bool(false))
        );
        assert!(!changes.iter().any(|change| {
            change_label(change) == "WorkGraphTaskLease"
                && matches!(change, SourceChange::Delete { .. })
        }));
        assert_eq!(validate_lease(&harness, &old_request).await, 200);
    }

    let exact = task_comment(
        "I_A",
        "open",
        "IC_result",
        &result_body("validate-issue", &old_lease),
        "reporter",
        "U_reporter",
    );
    let before = harness.wal.head_sequence(ID).await.unwrap();
    assert_eq!(
        harness.post("issue_comment", "exact-result", &exact).await,
        202
    );
    let changes = wal_changes(&harness, before + 1).await;
    assert_eq!(
        node_property(&changes, "IC_result", "trusted"),
        Some(&ElementValue::Bool(true))
    );
    assert_eq!(
        changes
            .iter()
            .map(|change| {
                (
                    change_label(change),
                    matches!(change, SourceChange::Delete { .. }),
                )
            })
            .collect::<Vec<_>>(),
        vec![
            ("WorkGraphTaskResult", false),
            ("COMMENT_ON", false),
            ("RESULT_FOR", false),
            ("LEASE_FOR", true),
            ("LEASES_SLOT", true),
            ("WorkGraphTaskLease", true),
            ("WorkGraphTaskLease", false),
            ("LEASE_FOR", false),
            ("LEASES_SLOT", false),
            ("WorkGraphAgent", false),
        ]
    );
    assert_agent_counts(&changes, 0, 1, 0);
    let new_lease = hex::encode(Sha256::digest(b"I_B\0IC_B\0\x31"));
    let new_request = json!({
        "taskNodeId": "I_B",
        "leaseId": new_lease,
        "assignmentCommentNodeId": "IC_B",
        "agentId": "issue-validator",
        "slotId": "issue-validator/1"
    });
    assert_eq!(validate_lease(&harness, &old_request).await, 409);
    assert_eq!(validate_lease(&harness, &new_request).await, 200);

    let late = task_comment(
        "I_A",
        "open",
        "IC_late",
        &result_body("validate-issue", &old_lease),
        "reporter",
        "U_reporter",
    );
    let before = harness.wal.head_sequence(ID).await.unwrap();
    assert_eq!(
        harness.post("issue_comment", "late-result", &late).await,
        202
    );
    let changes = wal_changes(&harness, before + 1).await;
    assert_eq!(
        node_property(&changes, "IC_late", "trusted"),
        Some(&ElementValue::Bool(false))
    );
    assert!(!changes.iter().any(|change| {
        change_label(change) == "WorkGraphTaskLease"
            && matches!(change, SourceChange::Delete { .. })
    }));
    assert_eq!(validate_lease(&harness, &new_request).await, 200);

    harness.source.stop().await.unwrap();
}

#[tokio::test]
async fn restart_pruning_waits_for_all_startup_subscriptions() {
    let h = Harness::new(10_000).await;
    for n in 0..3 {
        assert_eq!(
            h.post("issues", &format!("startup-{n}"), &issue(&format!("I_{n}")))
                .await,
            202
        );
    }
    let head = h.wal.head_sequence(ID).await.unwrap();
    assert_eq!(head, 3 * PER_ISSUE as u64);
    h.source.stop().await.unwrap();

    let restarted = Harness::build(10_000, h.port);
    h.initialize(&restarted).await;
    restarted.start().await.unwrap();

    // The first query registers its durable position and catches up completely.
    let mut early = restarted
        .subscribe(settings("early", Some(0), false))
        .await
        .unwrap();
    let early_position = early.position_handle.take().unwrap();
    let early_replay = drain(&mut early.receiver, head as usize).await;
    assert_eq!(
        early_replay.iter().map(|(seq, _)| *seq).collect::<Vec<_>>(),
        (1..=head).collect::<Vec<_>>()
    );
    early_position.store(head, Ordering::Release);

    // More than one dispatch tick passes before the next query registers. The
    // startup fence must retain the WAL despite the early query reaching head.
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    assert_eq!(h.wal.event_count(ID).await.unwrap(), head);

    let mut late = restarted
        .subscribe(settings("late", Some(0), false))
        .await
        .expect("late startup subscriber must still be able to replay");
    let late_position = late.position_handle.take().unwrap();

    // The lifecycle opens pruning only after every startup query has subscribed.
    restarted
        .on_subscriptions_complete()
        .await
        .expect("on_subscriptions_complete must succeed");
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(
        h.wal.event_count(ID).await.unwrap(),
        head,
        "the late subscriber's old watermark must protect its replay"
    );

    let late_replay = drain(&mut late.receiver, head as usize).await;
    assert_eq!(
        late_replay.iter().map(|(seq, _)| *seq).collect::<Vec<_>>(),
        (1..=head).collect::<Vec<_>>()
    );
    late_position.store(head, Ordering::Release);

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if h.wal.event_count(ID).await.unwrap() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("WAL should prune after every startup subscriber reaches head");

    restarted.stop().await.unwrap();
}

/// Dynamic proxy variant of `restart_pruning_waits_for_all_startup_subscriptions`:
/// verifies that calling `on_subscriptions_complete` **through the plugin-sdk vtable**
/// (the ABI path used when the source is loaded as a dynamic plugin) releases the
/// startup pruning fence exactly as the direct call does.
///
/// Subscriptions are made on the concrete source before wrapping to keep the test
/// focused on the vtable slot for `on_subscriptions_complete`; all other pruning
/// logic is unchanged from the static test.
#[tokio::test]
async fn restart_pruning_released_via_vtable_on_subscriptions_complete() {
    // ---- vtable helpers (no-op stubs for the unused FFI slots) ----
    extern "C" fn noop_executor(_: *mut std::ffi::c_void) -> *mut std::ffi::c_void {
        std::ptr::null_mut()
    }
    fn noop_lifecycle(
        _id: &str,
        _ev: drasi_plugin_sdk::ffi::FfiLifecycleEventType,
        _details: &str,
    ) {
    }
    fn vtable_rt() -> &'static tokio::runtime::Runtime {
        static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
        RT.get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("vtable test runtime")
        })
    }

    // ---- Phase 1: seed WAL ----
    let h = Harness::new(10_000).await;
    for n in 0..3 {
        assert_eq!(
            h.post(
                "issues",
                &format!("dyn-startup-{n}"),
                &issue(&format!("DI_{n}"))
            )
            .await,
            202
        );
    }
    let head = h.wal.head_sequence(ID).await.unwrap();
    assert_eq!(head, 3 * PER_ISSUE as u64);
    h.source.stop().await.unwrap();

    // ---- Phase 2: restart with the same persistent store/WAL ----
    let restarted = Harness::build(10_000, h.port);
    h.initialize(&restarted).await;
    restarted.start().await.unwrap();

    // Early query subscribes and catches up completely.
    let mut early = restarted
        .subscribe(settings("dyn-early", Some(0), false))
        .await
        .unwrap();
    let early_position = early.position_handle.take().unwrap();
    let early_replay = drain(&mut early.receiver, head as usize).await;
    assert_eq!(
        early_replay.iter().map(|(seq, _)| *seq).collect::<Vec<_>>(),
        (1..=head).collect::<Vec<_>>()
    );
    early_position.store(head, Ordering::Release);

    // Wait a full dispatch tick — startup fence must still hold the WAL.
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    assert_eq!(h.wal.event_count(ID).await.unwrap(), head);

    // Late query subscribes before the fence is lifted.
    let mut late = restarted
        .subscribe(settings("dyn-late", Some(0), false))
        .await
        .expect("late startup subscriber must still be able to replay");
    let late_position = late.position_handle.take().unwrap();

    // ---- Key assertion: release the fence through the vtable, not directly ----
    // Wrap the source in a plugin-sdk SourceVtable (the ABI used by dynamic plugins)
    // and call on_subscriptions_complete via the vtable fn pointer.
    let vtable = drasi_plugin_sdk::ffi::vtable_gen::build_source_vtable(
        restarted,
        noop_executor,
        noop_lifecycle,
        vtable_rt,
    );
    let state = drasi_plugin_sdk::ffi::SendMutPtr(vtable.state);
    let complete_fn = vtable.on_subscriptions_complete_fn;
    let drop_fn = vtable.drop_fn;
    let result = std::thread::spawn(move || (complete_fn)(state.as_ptr()))
        .join()
        .expect("vtable thread must not abort");
    let ok: Result<(), String> = unsafe { result.into_result() };
    assert!(
        ok.is_ok(),
        "on_subscriptions_complete via vtable must succeed: {ok:?}"
    );
    // Explicitly free the vtable state (no Drop impl on SourceVtable).
    (drop_fn)(vtable.state);

    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(
        h.wal.event_count(ID).await.unwrap(),
        head,
        "late subscriber's old watermark must protect its replay"
    );

    let late_replay = drain(&mut late.receiver, head as usize).await;
    assert_eq!(
        late_replay.iter().map(|(seq, _)| *seq).collect::<Vec<_>>(),
        (1..=head).collect::<Vec<_>>()
    );
    late_position.store(head, Ordering::Release);

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if h.wal.event_count(ID).await.unwrap() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("WAL should prune after every startup subscriber reaches head (vtable path)");
}

#[tokio::test]
#[ignore]
async fn push_delivery_converges_agent_capacity_and_retires_excess_slots() {
    let server = wiremock::MockServer::start().await;
    mount_agent_blob(&server, &agent_file(2)).await;

    // Starting the Source converges the configured agent file once, so a
    // restart re-states capacity even if pushes were missed while it was down.
    let harness = agent_harness(&server).await;
    let start_ids = wal_ids(&harness, 1).await;
    assert!(start_ids.contains(&"workgraph-agent:issue-validator".to_string()));
    assert!(start_ids.contains(&"workgraph-agent-slot:issue-validator/1".to_string()));
    assert!(start_ids.contains(&"workgraph-agent-slot:issue-validator/2".to_string()));
    let after_start = harness.wal.head_sequence(ID).await.unwrap();

    // A push on another repository, another ref, or another path is ignored.
    for (delivery, payload) in [
        (
            "d-other-repo",
            push("refs/heads/main", "acme/other", AGENT_PATH),
        ),
        (
            "d-other-ref",
            push("refs/heads/release", "acme/w", AGENT_PATH),
        ),
        (
            "d-other-path",
            push("refs/heads/main", "acme/w", "README.md"),
        ),
    ] {
        assert_eq!(harness.post("push", delivery, &payload).await, 204);
    }
    assert_eq!(harness.wal.head_sequence(ID).await.unwrap(), after_start);

    // Reducing capacity retires the excess slot instead of deleting it, so an
    // in-flight Lease keeps a valid LEASES_SLOT target.
    mount_agent_blob(&server, &agent_file(1)).await;
    assert_eq!(
        harness
            .post(
                "push",
                "d-reduce",
                &push("refs/heads/main", "acme/w", AGENT_PATH)
            )
            .await,
        202
    );
    let reduced = wal_ids(&harness, after_start + 1).await;
    assert!(reduced.contains(&"workgraph-agent-slot:issue-validator/2".to_string()));
    let retired = harness
        .wal
        .read_from(ID, after_start + 1)
        .await
        .unwrap()
        .into_iter()
        .find_map(|(_, change)| match change {
            SourceChange::Insert {
                element:
                    Element::Node {
                        metadata,
                        properties,
                    },
            }
            | SourceChange::Update {
                element:
                    Element::Node {
                        metadata,
                        properties,
                    },
            } if metadata.reference.element_id.as_ref()
                == "workgraph-agent-slot:issue-validator/2" =>
            {
                properties.get("retiring").cloned()
            }
            _ => None,
        })
        .expect("the excess slot is re-projected");
    assert_eq!(retired, drasi_core::models::ElementValue::Bool(true));
    assert!(!harness
        .wal
        .read_from(ID, after_start + 1)
        .await
        .unwrap()
        .into_iter()
        .any(
            |(_, change)| matches!(change, SourceChange::Delete { ref metadata }
            if metadata.reference.element_id.as_ref() == "workgraph-agent-slot:issue-validator/2")
        ));

    // Redelivery of the same push is absorbed by the delivery marker.
    let before_redelivery = harness.wal.head_sequence(ID).await.unwrap();
    assert_eq!(
        harness
            .post(
                "push",
                "d-reduce",
                &push("refs/heads/main", "acme/w", AGENT_PATH)
            )
            .await,
        202
    );
    assert_eq!(
        harness.wal.head_sequence(ID).await.unwrap(),
        before_redelivery
    );

    harness.source.stop().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn push_delivery_reports_malformed_and_unreadable_agent_files_distinctly() {
    let server = wiremock::MockServer::start().await;
    mount_agent_blob(&server, &agent_file(1)).await;
    let harness = agent_harness(&server).await;
    let after_start = harness.wal.head_sequence(ID).await.unwrap();

    // A readable but invalid file is a deterministic configuration error: it
    // becomes an explicit WorkGraphError and never an empty agent pool.
    mount_agent_blob(&server, "version: 1\nagents: []\n").await;
    assert_eq!(
        harness
            .post(
                "push",
                "d-malformed",
                &push("refs/heads/main", "acme/w", AGENT_PATH)
            )
            .await,
        202
    );
    let malformed = wal_ids(&harness, after_start + 1).await;
    assert_eq!(malformed, vec!["workgraph-error:agent-config".to_string()]);

    // An unreadable file proves nothing, so the delivery is retryable and no
    // agent state is asserted.
    let after_malformed = harness.wal.head_sequence(ID).await.unwrap();
    server.reset().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/graphql"))
        .respond_with(wiremock::ResponseTemplate::new(401))
        .mount(&server)
        .await;
    assert_eq!(
        harness
            .post(
                "push",
                "d-unreadable",
                &push("refs/heads/main", "acme/w", AGENT_PATH)
            )
            .await,
        503
    );
    assert_eq!(
        harness.wal.head_sequence(ID).await.unwrap(),
        after_malformed
    );

    // Repairing the file converges the pool again and clears the error.
    mount_agent_blob(&server, &agent_file(1)).await;
    assert_eq!(
        harness
            .post(
                "push",
                "d-repair",
                &push("refs/heads/main", "acme/w", AGENT_PATH)
            )
            .await,
        202
    );
    let repaired = wal_ids(&harness, after_malformed + 1).await;
    assert!(repaired.contains(&"workgraph-error:agent-config".to_string()));
    assert!(repaired.contains(&"workgraph-agent:issue-validator".to_string()));

    harness.source.stop().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn source_start_fails_when_the_configured_agent_file_cannot_be_read() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/graphql"))
        .respond_with(wiremock::ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let tmp = TempDir::new().unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let wal: Arc<dyn WalProvider> = Arc::new(RedbWalProvider::new(tmp.path().join("wal")));
    let store: Arc<dyn StateStoreProvider> =
        Arc::new(RedbStateStoreProvider::new(tmp.path().join("state")).unwrap());
    let config = GitHubWorkGraphSourceConfig {
        organization: "acme".into(),
        task_issue_type: TaskIssueType {
            id: "IT_test".into(),
            name: "WorkGraphTask".into(),
        },
        repositories: Vec::new(),
        lease_trust: None,
        agent_config: Some(AgentConfig {
            repository: "acme/w".into(),
            r#ref: "main".into(),
            path: AGENT_PATH.into(),
            token: "read-only-token".into(),
            api_base_url: format!("{}/graphql", server.uri()),
        }),
        webhook: WebhookConfig {
            host: "127.0.0.1".into(),
            port,
            secret: SECRET.into(),
            lease_validation_token: VALIDATION_TOKEN.into(),
            ..WebhookConfig::default()
        },
        durability: DurabilityConfig {
            enabled: true,
            max_events: 4096,
            capacity_policy: CapacityPolicy::RejectIncoming,
        },
    };
    let source = GitHubWorkGraphSourceBuilder::new(ID)
        .with_config(config)
        .build()
        .unwrap();
    let (tx, _rx): (ComponentUpdateSender, _) = tokio::sync::mpsc::channel(8);
    let mut context = SourceRuntimeContext::new("gh-it", ID, Some(store), tx, None);
    context.wal_provider = Some(wal);
    source.initialize(context).await;

    let error = source
        .start()
        .await
        .expect_err("an unreadable required agent file must fail start");
    assert!(
        format!("{error:#}").contains("agent file"),
        "unexpected error: {error:#}"
    );
}

#[tokio::test]
#[ignore]
async fn a_slow_agent_file_fetch_does_not_block_other_deliveries() {
    // Regression: the shared ingress gate must not be held across the remote
    // GitHub read, or one slow agent-file fetch stalls every other delivery.
    let server = wiremock::MockServer::start().await;
    mount_agent_blob(&server, &agent_file(1)).await;
    let harness = agent_harness(&server).await;

    // Make the next agent-file fetch slow.
    server.reset().await;
    let text = agent_file(2);
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/graphql"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(3))
                .set_body_json(json!({
                    "data": { "repository": { "object": {
                        "__typename": "Blob", "oid": "blob-oid", "text": text,
                        "byteSize": text.len(), "isTruncated": false, "isBinary": false,
                    }}}
                })),
        )
        .mount(&server)
        .await;

    let url = harness.url.clone();
    let push_body = push("refs/heads/main", "acme/w", AGENT_PATH).to_string();
    let slow = tokio::spawn(async move {
        let client = reqwest::Client::new();
        let mut mac = Hmac::<Sha256>::new_from_slice(SECRET.as_bytes()).unwrap();
        mac.update(push_body.as_bytes());
        client
            .post(&url)
            .header("x-github-event", "push")
            .header("x-github-delivery", "d-slow")
            .header(
                "x-hub-signature-256",
                format!("sha256={}", hex::encode(mac.finalize().into_bytes())),
            )
            .body(push_body)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    });

    // Give the push time to reach the (slow) fetch, then send an ordinary
    // delivery and require it to complete well before the fetch finishes.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let started = std::time::Instant::now();
    let status = harness
        .post("issues", "d-issue", &issue("I_unblocked"))
        .await;
    let elapsed = started.elapsed();

    assert_eq!(status, 202);
    assert!(
        elapsed < Duration::from_millis(1500),
        "an ordinary delivery waited {elapsed:?} behind a slow agent-file fetch"
    );
    assert_eq!(slow.await.unwrap(), 202);

    harness.source.stop().await.unwrap();
}

#[tokio::test]
async fn lease_validation_endpoint_requires_auth_and_matches_exact_active_state() {
    let server = wiremock::MockServer::start().await;
    mount_agent_blob(&server, &agent_file(1)).await;
    let harness = agent_harness(&server).await;
    let payload = task_comment(
        "I_task",
        "open",
        "IC_assignment",
        &assignment_body("issue-validator"),
        "assigner",
        "U_assigner",
    );
    let trust = LeaseTrust {
        dispatchers: vec![TrustedIdentity {
            id: "U_assigner".into(),
            login: "assigner".into(),
        }],
        reporters: vec![TrustedIdentity {
            id: "U_reporter".into(),
            login: "reporter".into(),
        }],
    };
    let task_type = TaskIssueType {
        id: "IT_test".into(),
        name: "WorkGraphTask".into(),
    };
    assert!(trust.is_assigner(payload.pointer("/comment/user")));
    let direct = Converter::new(ID, "acme", &task_type, 1)
        .with_lease_trust(&trust)
        .convert("issue_comment", &payload)
        .unwrap()
        .unwrap();
    assert!(matches!(
        direct.allocation,
        Some(AllocationEvent::Comment {
            artifact: Some(AllocationArtifact::Assignment { trusted: true, .. }),
            ..
        })
    ));
    assert_eq!(
        harness
            .post("issue_comment", "assignment-delivery", &payload)
            .await,
        202
    );

    let lease_id = hex::encode(Sha256::digest(b"I_task\0IC_assignment\0\x31"));
    let stored = harness
        .store
        .get(ID, "allocator:state")
        .await
        .unwrap()
        .unwrap();
    let allocator_state: Value = serde_json::from_slice(&stored).unwrap();
    assert!(
        allocator_state["active"].get(&lease_id).is_some(),
        "unexpected allocator state: {allocator_state:#}"
    );
    let request = json!({
        "taskNodeId": "I_task",
        "leaseId": lease_id.clone(),
        "assignmentCommentNodeId": "IC_assignment",
        "agentId": "issue-validator",
        "slotId": "issue-validator/1"
    });
    let client = reqwest::Client::new();
    let url = format!("{}/lease/validate", harness.url);
    assert_eq!(
        client
            .post(&url)
            .json(&request)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        401
    );
    assert_eq!(
        client
            .post(&url)
            .body("{")
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        401
    );
    assert_eq!(
        client
            .post(&url)
            .bearer_auth("wrong")
            .json(&request)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        401
    );
    let response = client
        .post(&url)
        .bearer_auth(VALIDATION_TOKEN)
        .json(&request)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    let active: Value = response.json().await.unwrap();
    assert_eq!(active["leaseId"], request["leaseId"]);
    assert_eq!(active["assignmentCommentNodeId"], "IC_assignment");
    assert_eq!(active["agentId"], "issue-validator");
    assert_eq!(active["slotId"], "issue-validator/1");

    for (field, value) in [
        ("taskNodeId", "I_other"),
        ("leaseId", "wrong-lease"),
        ("assignmentCommentNodeId", "IC_other"),
        ("agentId", "validator-2"),
        ("slotId", "issue-validator/2"),
    ] {
        let mut mismatch = request.clone();
        mismatch[field] = json!(value);
        assert_eq!(
            client
                .post(&url)
                .bearer_auth(VALIDATION_TOKEN)
                .json(&mismatch)
                .send()
                .await
                .unwrap()
                .status()
                .as_u16(),
            409,
            "field {field} must be exact"
        );
    }
    let stored = harness
        .store
        .get(ID, "allocator:state")
        .await
        .unwrap()
        .unwrap();
    let mut expired: Value = serde_json::from_slice(&stored).unwrap();
    expired["active"][lease_id.as_str()]["acquiredAt"] = json!("2000-01-01T00:00:00.000Z");
    expired["active"][lease_id.as_str()]["expiresAt"] = json!("2000-01-01T00:01:00.000Z");
    harness
        .store
        .set(ID, "allocator:state", serde_json::to_vec(&expired).unwrap())
        .await
        .unwrap();
    assert_eq!(validate_lease(&harness, &request).await, 409);
    harness
        .store
        .set(ID, "allocator:state", b"{corrupt".to_vec())
        .await
        .unwrap();
    assert_eq!(
        client
            .post(&url)
            .bearer_auth(VALIDATION_TOKEN)
            .json(&request)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        503
    );
    harness.source.stop().await.unwrap();
}
