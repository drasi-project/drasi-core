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
    Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
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
    GitHubWorkGraphSourceConfig, LeaseTrust, TaskIssueType, TrustedIdentity, WebhookConfig,
    WorkerConfig,
};
use drasi_source_github_workgraph::source::{GitHubWorkGraphSource, GitHubWorkGraphSourceBuilder};
use drasi_state_store_redb::RedbStateStoreProvider;
use drasi_wal_redb::RedbWalProvider;
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::Sha256;
use std::{collections::HashSet, sync::Arc, time::Duration};
use tempfile::TempDir;

const ID: &str = "gh-workgraph-it";
const SECRET: &str = "webhook-secret";
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
            worker_config: None,
            lease_trust: None,
            webhook: WebhookConfig {
                host: "127.0.0.1".into(),
                port,
                secret: SECRET.into(),
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
// Worker queue convergence over a live webhook `push` delivery.
// ---------------------------------------------------------------------------

const WORKER_PATH: &str = ".github/workgraph/workers.yaml";

fn worker_file(slots: u32) -> String {
    format!(
        "version: 1\nworkers:\n  - workerId: validator-1\n    agentProfile: issue-validator\n    \
         slots: {slots}\n    leaseDuration: PT15M\n"
    )
}

/// Mount the worker-file blob query so every fetch returns `text`.
async fn mount_worker_blob(server: &wiremock::MockServer, text: &str) {
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

async fn worker_harness(server: &wiremock::MockServer) -> Harness {
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
        worker_config: Some(WorkerConfig {
            repository: "acme/w".into(),
            r#ref: "main".into(),
            path: WORKER_PATH.into(),
            token: "read-only-token".into(),
            api_base_url: format!("{}/graphql", server.uri()),
        }),
        webhook: WebhookConfig {
            host: "127.0.0.1".into(),
            port,
            secret: SECRET.into(),
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

#[tokio::test]
#[ignore]
async fn push_delivery_converges_worker_capacity_and_retires_excess_slots() {
    let server = wiremock::MockServer::start().await;
    mount_worker_blob(&server, &worker_file(2)).await;

    // Starting the Source converges the configured worker file once, so a
    // restart re-states capacity even if pushes were missed while it was down.
    let harness = worker_harness(&server).await;
    let start_ids = wal_ids(&harness, 1).await;
    assert!(start_ids.contains(&"workgraph-worker:validator-1".to_string()));
    assert!(start_ids.contains(&"workgraph-worker-slot:validator-1/1".to_string()));
    assert!(start_ids.contains(&"workgraph-worker-slot:validator-1/2".to_string()));
    let after_start = harness.wal.head_sequence(ID).await.unwrap();

    // A push on another repository, another ref, or another path is ignored.
    for (delivery, payload) in [
        (
            "d-other-repo",
            push("refs/heads/main", "acme/other", WORKER_PATH),
        ),
        (
            "d-other-ref",
            push("refs/heads/release", "acme/w", WORKER_PATH),
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
    mount_worker_blob(&server, &worker_file(1)).await;
    assert_eq!(
        harness
            .post(
                "push",
                "d-reduce",
                &push("refs/heads/main", "acme/w", WORKER_PATH)
            )
            .await,
        202
    );
    let reduced = wal_ids(&harness, after_start + 1).await;
    assert!(reduced.contains(&"workgraph-worker-slot:validator-1/2".to_string()));
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
                == "workgraph-worker-slot:validator-1/2" =>
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
            if metadata.reference.element_id.as_ref() == "workgraph-worker-slot:validator-1/2")
        ));

    // Redelivery of the same push is absorbed by the delivery marker.
    let before_redelivery = harness.wal.head_sequence(ID).await.unwrap();
    assert_eq!(
        harness
            .post(
                "push",
                "d-reduce",
                &push("refs/heads/main", "acme/w", WORKER_PATH)
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
async fn push_delivery_reports_malformed_and_unreadable_worker_files_distinctly() {
    let server = wiremock::MockServer::start().await;
    mount_worker_blob(&server, &worker_file(1)).await;
    let harness = worker_harness(&server).await;
    let after_start = harness.wal.head_sequence(ID).await.unwrap();

    // A readable but invalid file is a deterministic configuration error: it
    // becomes an explicit WorkGraphError and never an empty worker pool.
    mount_worker_blob(&server, "version: 1\nworkers: []\n").await;
    assert_eq!(
        harness
            .post(
                "push",
                "d-malformed",
                &push("refs/heads/main", "acme/w", WORKER_PATH)
            )
            .await,
        202
    );
    let malformed = wal_ids(&harness, after_start + 1).await;
    assert_eq!(malformed, vec!["workgraph-error:worker-config".to_string()]);

    // An unreadable file proves nothing, so the delivery is retryable and no
    // worker state is asserted.
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
                &push("refs/heads/main", "acme/w", WORKER_PATH)
            )
            .await,
        503
    );
    assert_eq!(
        harness.wal.head_sequence(ID).await.unwrap(),
        after_malformed
    );

    // Repairing the file converges the pool again and clears the error.
    mount_worker_blob(&server, &worker_file(1)).await;
    assert_eq!(
        harness
            .post(
                "push",
                "d-repair",
                &push("refs/heads/main", "acme/w", WORKER_PATH)
            )
            .await,
        202
    );
    let repaired = wal_ids(&harness, after_malformed + 1).await;
    assert!(repaired.contains(&"workgraph-error:worker-config".to_string()));
    assert!(repaired.contains(&"workgraph-worker:validator-1".to_string()));

    harness.source.stop().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn source_start_fails_when_the_configured_worker_file_cannot_be_read() {
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
        worker_config: Some(WorkerConfig {
            repository: "acme/w".into(),
            r#ref: "main".into(),
            path: WORKER_PATH.into(),
            token: "read-only-token".into(),
            api_base_url: format!("{}/graphql", server.uri()),
        }),
        webhook: WebhookConfig {
            host: "127.0.0.1".into(),
            port,
            secret: SECRET.into(),
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
        .expect_err("an unreadable required worker file must fail start");
    assert!(
        format!("{error:#}").contains("worker file"),
        "unexpected error: {error:#}"
    );
}

#[tokio::test]
#[ignore]
async fn a_slow_worker_file_fetch_does_not_block_other_deliveries() {
    // Regression: the shared ingress gate must not be held across the remote
    // GitHub read, or one slow worker-file fetch stalls every other delivery.
    let server = wiremock::MockServer::start().await;
    mount_worker_blob(&server, &worker_file(1)).await;
    let harness = worker_harness(&server).await;

    // Make the next worker-file fetch slow.
    server.reset().await;
    let text = worker_file(2);
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
    let push_body = push("refs/heads/main", "acme/w", WORKER_PATH).to_string();
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
        "an ordinary delivery waited {elapsed:?} behind a slow worker-file fetch"
    );
    assert_eq!(slow.await.unwrap(), 202);

    harness.source.stop().await.unwrap();
}

// ---------------------------------------------------------------------------
// Durable lease ledger: redelivery and restart converge on current state.
// ---------------------------------------------------------------------------

const LEASE_TRUST_LOGIN: &str = "reporter";

fn task_comment(comment_id: &str, body: &str, login: &str) -> Value {
    json!({
        "action": "created",
        "organization": {"login": "acme", "node_id": "O_1"},
        "repository": {
            "node_id": "R_7", "name": "w", "full_name": "acme/w", "owner": {"login": "acme"}
        },
        "issue": {
            "node_id": "I_task", "id": 1, "number": 1, "title": "t", "state": "open",
            "labels": [], "type": {"node_id": "IT_test", "name": "WorkGraphTask"},
            "user": {"login": "ada", "node_id": "U_1", "id": 1, "type": "User"}
        },
        "comment": {
            "node_id": comment_id, "id": 9001, "body": body,
            "created_at": "2026-08-19T22:05:00Z", "updated_at": "2026-08-19T22:05:00Z",
            "html_url": "https://github.com/acme/w/issues/1#issuecomment-9001",
            "user": {"login": login, "node_id": format!("U_{login}"), "id": 7, "type": "User"}
        }
    })
}

const IT_LEASE: &str = r#"WorkGraphTaskLease/v1

```json
{
  "leaseId": "0198d8c4-7c28-7d43-a8dd-e9f5be8c1b21",
  "assignmentCommentNodeId": "IC_assignment",
  "workerId": "validator-1",
  "slotId": "validator-1/1",
  "acquiredAt": "2026-08-19T22:00:00Z",
  "expiresAt": "2026-08-19T22:15:00Z"
}
```
"#;

const IT_RESULT_V2: &str = r#"WorkGraphTaskResult/v2

```json
{
  "taskType": "validate-issue",
  "leaseId": "0198d8c4-7c28-7d43-a8dd-e9f5be8c1b21",
  "outcome": "succeeded",
  "summary": "Validated the issue.",
  "result": {
    "criteria": [
      {
        "criterion": "Acceptance criteria",
        "passed": true,
        "evidence": "Present."
      }
    ]
  }
}
```
"#;

const IT_ANCHOR: &str = "workgraph-lease:I_task:0198d8c4-7c28-7d43-a8dd-e9f5be8c1b21";

async fn anchor_is_active(harness: &Harness, from: u64) -> Option<bool> {
    harness
        .wal
        .read_from(ID, from)
        .await
        .unwrap()
        .into_iter()
        .rev()
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
            } if metadata.reference.element_id.as_ref() == IT_ANCHOR => {
                match properties.get("isActive") {
                    Some(drasi_core::models::ElementValue::Bool(value)) => Some(*value),
                    _ => None,
                }
            }
            _ => None,
        })
}

#[tokio::test]
#[ignore]
async fn the_durable_lease_ledger_converges_across_redelivery_and_restart() {
    let server = wiremock::MockServer::start().await;
    mount_lifecycle_api(
        &server,
        vec![
            graphql_comment("IC_lease", IT_LEASE, "dispatcher"),
            graphql_comment("IC_result", IT_RESULT_V2, LEASE_TRUST_LOGIN),
        ],
    )
    .await;
    let tmp = TempDir::new().unwrap();
    let wal: Arc<dyn WalProvider> = Arc::new(RedbWalProvider::new(tmp.path().join("wal")));
    let harness = lifecycle_harness(&server, wal).await;

    assert_eq!(
        harness
            .post(
                "issue_comment",
                "d1",
                &task_comment("IC_lease", IT_LEASE, "dispatcher")
            )
            .await,
        202
    );
    let after_lease = harness.wal.head_sequence(ID).await.unwrap();

    assert_eq!(
        harness
            .post(
                "issue_comment",
                "d2",
                &task_comment("IC_result", IT_RESULT_V2, LEASE_TRUST_LOGIN)
            )
            .await,
        202
    );
    assert_eq!(
        anchor_is_active(&harness, after_lease + 1).await,
        Some(false)
    );
    let after_result = harness.wal.head_sequence(ID).await.unwrap();

    // Re-observing the acquisition must not resurrect the ended lease. A fresh
    // delivery ID bypasses the dedupe marker, exercising the ledger itself.
    let mut replay = task_comment("IC_lease", IT_LEASE, "dispatcher");
    replay["action"] = json!("pinned");
    assert_eq!(harness.post("issue_comment", "d3", &replay).await, 202);
    assert_eq!(
        anchor_is_active(&harness, after_result + 1).await,
        Some(false),
        "re-observing the acquisition resurrected an ended lease"
    );
    let after_replay = harness.wal.head_sequence(ID).await.unwrap();

    // Restart: the ledger is durable, so deleting the completion reactivates
    // from the surviving artifacts rather than from an empty ledger.
    harness.source.stop().await.unwrap();
    let restarted = GitHubWorkGraphSourceBuilder::new(ID)
        .with_config(lifecycle_config(
            harness.port,
            &format!("{}/graphql", server.uri()),
        ))
        .build()
        .unwrap();
    harness.start(&restarted).await;

    mount_lifecycle_api(
        &server,
        vec![graphql_comment("IC_lease", IT_LEASE, "dispatcher")],
    )
    .await;
    let mut deleted = task_comment("IC_result", IT_RESULT_V2, LEASE_TRUST_LOGIN);
    deleted["action"] = json!("deleted");
    assert_eq!(harness.post("issue_comment", "d4", &deleted).await, 202);
    assert_eq!(
        anchor_is_active(&harness, after_replay + 1).await,
        Some(true),
        "removing the only end must reactivate the lease after a restart"
    );

    restarted.stop().await.unwrap();
}

// ---------------------------------------------------------------------------
// Ledger durability under append failure, and reconciliation of a task whose
// lifecycle predates this Source's ledger.
// ---------------------------------------------------------------------------

/// A WAL that fails every append once armed, so a test can prove the ledger is
/// only persisted after the changes it implies are durable.
struct FailOnceWal {
    inner: Arc<dyn WalProvider>,
    fail: Arc<std::sync::atomic::AtomicBool>,
}

#[async_trait]
impl WalProvider for FailOnceWal {
    async fn register(
        &self,
        source_id: &str,
        config: drasi_lib::wal::WriteAheadLogConfig,
    ) -> Result<(), drasi_lib::wal::WalError> {
        self.inner.register(source_id, config).await
    }
    async fn append(
        &self,
        source_id: &str,
        event: &SourceChange,
    ) -> Result<u64, drasi_lib::wal::WalError> {
        if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(drasi_lib::wal::WalError::CapacityExhausted(
                "injected failure".to_string(),
            ));
        }
        self.inner.append(source_id, event).await
    }
    async fn read_from(
        &self,
        source_id: &str,
        sequence: u64,
    ) -> Result<Vec<(u64, SourceChange)>, drasi_lib::wal::WalError> {
        self.inner.read_from(source_id, sequence).await
    }
    async fn prune_up_to(
        &self,
        source_id: &str,
        sequence: u64,
    ) -> Result<u64, drasi_lib::wal::WalError> {
        self.inner.prune_up_to(source_id, sequence).await
    }
    async fn head_sequence(&self, source_id: &str) -> Result<u64, drasi_lib::wal::WalError> {
        self.inner.head_sequence(source_id).await
    }
    async fn oldest_sequence(
        &self,
        source_id: &str,
    ) -> Result<Option<u64>, drasi_lib::wal::WalError> {
        self.inner.oldest_sequence(source_id).await
    }
    async fn event_count(&self, source_id: &str) -> Result<u64, drasi_lib::wal::WalError> {
        self.inner.event_count(source_id).await
    }
    async fn delete_wal(&self, source_id: &str) -> Result<(), drasi_lib::wal::WalError> {
        self.inner.delete_wal(source_id).await
    }
}

fn lifecycle_config(port: u16, api: &str) -> GitHubWorkGraphSourceConfig {
    GitHubWorkGraphSourceConfig {
        organization: "acme".into(),
        task_issue_type: TaskIssueType {
            id: "IT_test".into(),
            name: "WorkGraphTask".into(),
        },
        repositories: Vec::new(),
        worker_config: Some(WorkerConfig {
            repository: "acme/w".into(),
            r#ref: "main".into(),
            path: WORKER_PATH.into(),
            token: "read-only-token".into(),
            api_base_url: api.to_string(),
        }),
        lease_trust: Some(LeaseTrust {
            dispatchers: vec![TrustedIdentity {
                id: "U_dispatcher".into(),
                login: "dispatcher".into(),
            }],
            reporters: vec![TrustedIdentity {
                id: format!("U_{LEASE_TRUST_LOGIN}"),
                login: LEASE_TRUST_LOGIN.into(),
            }],
        }),
        webhook: WebhookConfig {
            host: "127.0.0.1".into(),
            port,
            secret: SECRET.into(),
            ..WebhookConfig::default()
        },
        durability: DurabilityConfig {
            enabled: true,
            max_events: 4096,
            capacity_policy: CapacityPolicy::RejectIncoming,
        },
    }
}

fn graphql_comment(node_id: &str, body: &str, login: &str) -> Value {
    json!({
        "node_id": node_id, "id": 9100, "body": body,
        "created_at": "2026-08-19T22:05:00Z", "updated_at": "2026-08-19T22:05:00Z",
        "last_edited_at": Value::Null,
        "html_url": "https://github.com/acme/w/issues/1",
        "user": {"login": login, "node_id": format!("U_{login}"), "type": "User"},
        "editor": Value::Null
    })
}

/// Serve the worker file and one task's current comments.
async fn mount_lifecycle_api(server: &wiremock::MockServer, comments: Vec<Value>) {
    server.reset().await;
    let text = worker_file(1);
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/graphql"))
        .and(wiremock::matchers::body_string_contains(
            "object(expression",
        ))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({
            "data": { "repository": { "object": {
                "__typename": "Blob", "oid": "o", "text": text,
                "byteSize": text.len(), "isTruncated": false, "isBinary": false,
            }}}
        })))
        .mount(server)
        .await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/graphql"))
        .and(wiremock::matchers::body_string_contains(
            "issue(number: $number)",
        ))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({
            "data": { "repository": { "issue": { "comments": {
                "pageInfo": {"hasNextPage": false, "endCursor": Value::Null},
                "nodes": comments
            }}}}
        })))
        .mount(server)
        .await;
}

async fn lifecycle_harness(server: &wiremock::MockServer, wal: Arc<dyn WalProvider>) -> Harness {
    let tmp = TempDir::new().unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let store: Arc<dyn StateStoreProvider> =
        Arc::new(RedbStateStoreProvider::new(tmp.path().join("state")).unwrap());
    let harness = Harness {
        _tmp: tmp,
        source: GitHubWorkGraphSourceBuilder::new(ID)
            .with_config(lifecycle_config(port, &format!("{}/graphql", server.uri())))
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

#[tokio::test]
#[ignore]
async fn a_failed_append_never_advances_the_ledger_past_the_graph() {
    // Deleting and rekeying both touch two anchors. If the ledger were
    // persisted before the append, a failed append would leave the ledger
    // advanced and the redelivery would compute a smaller affected set,
    // permanently losing the old anchor's change.
    for (scenario, second) in [
        ("delete", None),
        (
            "rekey",
            Some(IT_LEASE.replace(
                "0198d8c4-7c28-7d43-a8dd-e9f5be8c1b21",
                "0198d8c4-7c28-7d43-a8dd-000000000000",
            )),
        ),
    ] {
        let server = wiremock::MockServer::start().await;
        mount_lifecycle_api(
            &server,
            vec![graphql_comment("IC_lease", IT_LEASE, "dispatcher")],
        )
        .await;
        let tmp = TempDir::new().unwrap();
        let inner: Arc<dyn WalProvider> = Arc::new(RedbWalProvider::new(tmp.path().join("wal")));
        let fail = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let wal: Arc<dyn WalProvider> = Arc::new(FailOnceWal {
            inner: inner.clone(),
            fail: fail.clone(),
        });
        let harness = lifecycle_harness(&server, wal).await;

        harness
            .post(
                "issue_comment",
                "d1",
                &task_comment("IC_lease", IT_LEASE, "dispatcher"),
            )
            .await;
        assert_eq!(
            anchor_is_active(&harness, 1).await,
            Some(true),
            "{scenario}"
        );
        let before = harness.wal.head_sequence(ID).await.unwrap();

        // The second delivery removes the acquisition from the original anchor.
        let mut event = match &second {
            None => task_comment("IC_lease", IT_LEASE, "dispatcher"),
            Some(body) => task_comment("IC_lease", body, "dispatcher"),
        };
        event["action"] = json!(if second.is_none() {
            "deleted"
        } else {
            "edited"
        });
        if second.is_some() {
            event["changes"] = json!({ "body": { "from": IT_LEASE } });
            event["sender"] = json!({"login": "dispatcher", "node_id": "U_dispatcher"});
        }

        // First attempt fails inside the append loop.
        fail.store(true, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            harness.post("issue_comment", "d2", &event).await,
            503,
            "{scenario}"
        );
        assert_eq!(
            harness.wal.head_sequence(ID).await.unwrap(),
            before,
            "{scenario}"
        );

        // Redelivery must recompute and re-emit the *old* anchor's change.
        fail.store(false, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            harness.post("issue_comment", "d2", &event).await,
            202,
            "{scenario}"
        );
        let emitted: Vec<String> = harness
            .wal
            .read_from(ID, before + 1)
            .await
            .unwrap()
            .into_iter()
            .map(|(_, change)| change.get_reference().element_id.to_string())
            .collect();
        assert!(
            emitted.iter().any(|id| id == IT_ANCHOR),
            "{scenario}: redelivery lost the original anchor change; emitted {emitted:?}"
        );
        if second.is_some() {
            assert!(
                emitted
                    .iter()
                    .any(|id| id == "workgraph-lease:I_task:0198d8c4-7c28-7d43-a8dd-000000000000"),
                "{scenario}: redelivery lost the new anchor change"
            );
        }
        harness.source.stop().await.unwrap();
    }
}

#[tokio::test]
#[ignore]
async fn a_lifecycle_delivery_reconciles_a_task_the_ledger_has_never_seen() {
    // After a clean bootstrap the Source's ledger is empty while GitHub already
    // holds a historical Lease. A live Result must end that lease rather than
    // delete an anchor it thinks was never acquired.
    let server = wiremock::MockServer::start().await;
    mount_lifecycle_api(
        &server,
        vec![
            graphql_comment("IC_lease", IT_LEASE, "dispatcher"),
            graphql_comment("IC_result", IT_RESULT_V2, LEASE_TRUST_LOGIN),
        ],
    )
    .await;
    let tmp = TempDir::new().unwrap();
    let wal: Arc<dyn WalProvider> = Arc::new(RedbWalProvider::new(tmp.path().join("wal")));
    let harness = lifecycle_harness(&server, wal).await;

    assert_eq!(
        harness
            .post(
                "issue_comment",
                "d1",
                &task_comment("IC_result", IT_RESULT_V2, LEASE_TRUST_LOGIN)
            )
            .await,
        202
    );
    assert_eq!(
        anchor_is_active(&harness, 1).await,
        Some(false),
        "the historical lease must be ended, not deleted"
    );

    // Deleting the Result reconciles current GitHub state, which no longer
    // contains it, and returns the historical lease to active.
    mount_lifecycle_api(
        &server,
        vec![graphql_comment("IC_lease", IT_LEASE, "dispatcher")],
    )
    .await;
    let head = harness.wal.head_sequence(ID).await.unwrap();
    let mut deleted = task_comment("IC_result", IT_RESULT_V2, LEASE_TRUST_LOGIN);
    deleted["action"] = json!("deleted");
    assert_eq!(harness.post("issue_comment", "d2", &deleted).await, 202);
    assert_eq!(anchor_is_active(&harness, head + 1).await, Some(true));

    harness.source.stop().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn reconciliation_detects_a_historical_duplicate_acquisition() {
    // Two historical trusted Lease comments share one leaseId. A live delivery
    // must discover the conflict from GitHub rather than from its empty ledger.
    let second = IT_LEASE.replace("validator-1/1", "validator-1/2");
    let server = wiremock::MockServer::start().await;
    mount_lifecycle_api(
        &server,
        vec![
            graphql_comment("IC_lease_1", IT_LEASE, "dispatcher"),
            graphql_comment("IC_lease_2", &second, "dispatcher"),
        ],
    )
    .await;
    let tmp = TempDir::new().unwrap();
    let wal: Arc<dyn WalProvider> = Arc::new(RedbWalProvider::new(tmp.path().join("wal")));
    let harness = lifecycle_harness(&server, wal).await;

    let mut expiry = task_comment("IC_expiry", IT_LEASE, "dispatcher");
    expiry["comment"]["body"] = json!(IT_EXPIRATION);
    expiry["comment"]["user"] = json!({"login": LEASE_TRUST_LOGIN, "node_id": format!("U_{LEASE_TRUST_LOGIN}"), "type": "User"});
    assert_eq!(harness.post("issue_comment", "d1", &expiry).await, 202);

    let reason = harness
        .wal
        .read_from(ID, 1)
        .await
        .unwrap()
        .into_iter()
        .rev()
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
            } if metadata.reference.element_id.as_ref() == IT_ANCHOR => {
                properties.get("endReason").cloned()
            }
            _ => None,
        })
        .expect("the anchor is projected");
    assert_eq!(
        reason,
        drasi_core::models::ElementValue::String("conflict".into()),
        "a historical duplicate acquisition must be discovered by reconciliation"
    );

    harness.source.stop().await.unwrap();
}

const IT_EXPIRATION: &str = r#"WorkGraphTaskLeaseExpiration/v1

```json
{
  "leaseCommentNodeId": "IC_lease",
  "leaseId": "0198d8c4-7c28-7d43-a8dd-e9f5be8c1b21",
  "expiredAt": "2026-08-19T22:15:00Z",
  "reason": "deadline-reached"
}
```
"#;

#[tokio::test]
#[ignore]
async fn a_slow_task_comment_fetch_does_not_block_other_deliveries() {
    // Reconciliation reads GitHub, so it must not be performed while holding
    // the shared ingress gate — the same rule the worker-file push path
    // follows.
    let server = wiremock::MockServer::start().await;
    let text = worker_file(1);
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/graphql"))
        .and(wiremock::matchers::body_string_contains(
            "object(expression",
        ))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({
            "data": { "repository": { "object": {
                "__typename": "Blob", "oid": "o", "text": text,
                "byteSize": text.len(), "isTruncated": false, "isBinary": false,
            }}}
        })))
        .mount(&server)
        .await;
    // The task-comment read is slow; the worker-file read above is not.
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/graphql"))
        .and(wiremock::matchers::body_string_contains(
            "issue(number: $number)",
        ))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(3))
                .set_body_json(json!({
                    "data": { "repository": { "issue": { "comments": {
                        "pageInfo": {"hasNextPage": false, "endCursor": Value::Null},
                        "nodes": [graphql_comment("IC_lease", IT_LEASE, "dispatcher")]
                    }}}}
                })),
        )
        .mount(&server)
        .await;

    let tmp = TempDir::new().unwrap();
    let wal: Arc<dyn WalProvider> = Arc::new(RedbWalProvider::new(tmp.path().join("wal")));
    let harness = lifecycle_harness(&server, wal).await;

    // This delivery reconciles an unseen task, so it waits on the slow read.
    let url = harness.url.clone();
    let body = task_comment("IC_result", IT_RESULT_V2, LEASE_TRUST_LOGIN).to_string();
    let slow = tokio::spawn(async move {
        let client = reqwest::Client::new();
        let mut mac = Hmac::<Sha256>::new_from_slice(SECRET.as_bytes()).unwrap();
        mac.update(body.as_bytes());
        client
            .post(&url)
            .header("x-github-event", "issue_comment")
            .header("x-github-delivery", "d-slow")
            .header(
                "x-hub-signature-256",
                format!("sha256={}", hex::encode(mac.finalize().into_bytes())),
            )
            .body(body)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    });

    // Give the reconciliation time to reach the slow read, then require an
    // unrelated delivery to complete well before that read finishes.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let started = std::time::Instant::now();
    let status = harness
        .post("issues", "d-issue", &issue("I_unblocked"))
        .await;
    let elapsed = started.elapsed();

    assert_eq!(status, 202);
    assert!(
        elapsed < Duration::from_millis(1500),
        "an ordinary delivery waited {elapsed:?} behind a slow task-comment fetch"
    );
    assert_eq!(slow.await.unwrap(), 202);

    // The reconciled lifecycle is still correct.
    assert_eq!(anchor_is_active(&harness, 1).await, Some(false));

    harness.source.stop().await.unwrap();
}

/// The most recent value of `key` on `element` in the WAL from `from`.
async fn wal_property(
    harness: &Harness,
    from: u64,
    element: &str,
    key: &str,
) -> Option<drasi_core::models::ElementValue> {
    harness
        .wal
        .read_from(ID, from)
        .await
        .unwrap()
        .into_iter()
        .rev()
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
            } if metadata.reference.element_id.as_ref() == element => properties.get(key).cloned(),
            _ => None,
        })
}

#[tokio::test]
#[ignore]
async fn editing_a_bootstrapped_lifecycle_comment_away_reconciles_the_historical_anchor() {
    // The delivery that edits a bootstrapped Result into ordinary content emits
    // only a Retract. Without scope derived from the *previous* body the Source
    // would never reconcile the task, apply that Retract to an empty ledger,
    // and leave the historical anchor permanently ended.
    for (previous, ordinary) in [
        (IT_RESULT_V2, "just an ordinary note"),
        // A v1 Result is valid but carries no leaseId, so it is equally an
        // "edit away" from the lifecycle.
        (IT_RESULT_V2, IT_RESULT_V1),
        // Invalid content leaves a WorkGraphError, and must still reconcile.
        (IT_RESULT_V2, "WorkGraphTaskResult/v2\n\n```json\n{}\n```\n"),
    ] {
        let server = wiremock::MockServer::start().await;
        // GitHub's *current* state: the Lease survives, the Result no longer
        // parses as a v2 completion.
        mount_lifecycle_api(
            &server,
            vec![
                graphql_comment("IC_lease", IT_LEASE, "dispatcher"),
                graphql_comment("IC_result", ordinary, LEASE_TRUST_LOGIN),
            ],
        )
        .await;
        let tmp = TempDir::new().unwrap();
        let wal: Arc<dyn WalProvider> = Arc::new(RedbWalProvider::new(tmp.path().join("wal")));
        let harness = lifecycle_harness(&server, wal).await;

        let mut edited = task_comment("IC_result", ordinary, LEASE_TRUST_LOGIN);
        edited["action"] = json!("edited");
        edited["changes"] = json!({ "body": { "from": previous } });
        edited["sender"] =
            json!({"login": LEASE_TRUST_LOGIN, "node_id": format!("U_{LEASE_TRUST_LOGIN}")});

        assert_eq!(harness.post("issue_comment", "d1", &edited).await, 202);
        assert_eq!(
            anchor_is_active(&harness, 1).await,
            Some(true),
            "editing the completion away must return the historical lease to active"
        );
        harness.source.stop().await.unwrap();
    }

    // Deleting a bootstrapped Lease removes the historical anchor entirely.
    let server = wiremock::MockServer::start().await;
    mount_lifecycle_api(&server, vec![]).await;
    let tmp = TempDir::new().unwrap();
    let wal: Arc<dyn WalProvider> = Arc::new(RedbWalProvider::new(tmp.path().join("wal")));
    let harness = lifecycle_harness(&server, wal).await;

    let mut deleted = task_comment("IC_lease", IT_LEASE, "dispatcher");
    deleted["action"] = json!("deleted");
    assert_eq!(harness.post("issue_comment", "d1", &deleted).await, 202);
    let anchor_deleted =
        harness
            .wal
            .read_from(ID, 1)
            .await
            .unwrap()
            .into_iter()
            .any(|(_, change)| {
                matches!(change, SourceChange::Delete { ref metadata }
            if metadata.reference.element_id.as_ref() == IT_ANCHOR)
            });
    assert!(
        anchor_deleted,
        "deleting a bootstrapped Lease must remove the historical anchor"
    );
    harness.source.stop().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn lifecycle_comments_project_normally_when_the_queue_is_not_configured() {
    // A Source with neither `workerConfig` nor `leaseTrust` runs no worker
    // queue. A lifecycle-shaped comment must still be projected as an ordinary
    // untrusted artifact rather than failing the delivery by reaching for an
    // API client that was never configured.
    let harness = Harness::new(4096).await;

    for (comment_id, body, node_label) in [
        ("IC_lease", IT_LEASE, "WorkGraphTaskLease"),
        ("IC_result", IT_RESULT_V2, "WorkGraphTaskResult"),
        ("IC_expiry", IT_EXPIRATION, "WorkGraphTaskLeaseExpiration"),
    ] {
        let before = harness.wal.head_sequence(ID).await.unwrap();
        let delivery = format!("d-{comment_id}");
        assert_eq!(
            harness
                .post(
                    "issue_comment",
                    &delivery,
                    &task_comment(comment_id, body, "anyone")
                )
                .await,
            202,
            "{node_label} must not fail the delivery"
        );

        assert_eq!(
            wal_property(&harness, before + 1, comment_id, "trusted").await,
            Some(drasi_core::models::ElementValue::Bool(false)),
            "{node_label} must be projected as untrusted"
        );
        let labels: Vec<String> = harness
            .wal
            .read_from(ID, before + 1)
            .await
            .unwrap()
            .into_iter()
            .map(|(_, change)| match change {
                SourceChange::Insert { ref element } | SourceChange::Update { ref element } => {
                    element.get_metadata().labels[0].to_string()
                }
                SourceChange::Delete { ref metadata } => metadata.labels[0].to_string(),
                SourceChange::Future { .. } => String::new(),
            })
            .collect();
        assert!(
            labels.iter().any(|label| label == node_label),
            "{node_label} was not projected; saw {labels:?}"
        );
        assert!(
            !labels
                .iter()
                .any(|label| label == "WorkGraphTaskLeaseAnchor"),
            "{node_label} must not produce an anchor without a configured queue"
        );
    }

    harness.source.stop().await.unwrap();
}

const IT_RESULT_V1: &str = r#"WorkGraphTaskResult/v1

```json
{
  "taskType": "validate-issue",
  "outcome": "succeeded",
  "summary": "Validated the issue.",
  "result": {
    "criteria": [
      {
        "criterion": "Acceptance criteria",
        "passed": true,
        "evidence": "Present."
      }
    ]
  }
}
```
"#;
