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
use drasi_source_github_workgraph::config::{GitHubWorkGraphSourceConfig, WebhookConfig};
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
