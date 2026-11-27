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

//! Bootstrap provider tests against the in-process mock AHP host.

use anyhow::Result;
use drasi_ahp_common::ahp_types::state::SessionState;
use drasi_ahp_common::mock::MockAhpHost;
use drasi_bootstrap_ahp::{AhpBootstrapConfig, AhpBootstrapProvider};
use drasi_core::models::{Element, SourceChange};
use drasi_lib::bootstrap::{BootstrapContext, BootstrapProvider, BootstrapRequest};
use drasi_lib::channels::BootstrapEvent;
use serde_json::json;
use tokio::sync::mpsc;

async fn host_with_sessions(count: usize) -> Result<MockAhpHost> {
    let host = MockAhpHost::start().await?;
    host.set_agents(vec![serde_json::from_value(json!({
        "provider": "copilot", "displayName": "Copilot", "description": "", "models": []
    }))?])?;
    for i in 0..count {
        // The first session is archived (64) and idle (1).
        let state: SessionState = serde_json::from_value(json!({
            "provider": "copilot",
            "title": format!("Session {i}"),
            "status": if i == 0 { 64 | 1 } else { 1 },
            "lifecycle": "ready",
            "activeClients": [],
            "chats": [{
                "resource": format!("ahp-chat:/c{i}"),
                "title": "Main", "status": 1,
                "modifiedAt": "2026-01-01T00:00:00.000Z"
            }]
        }))?;
        host.add_session(&format!("ahp-session:/s{i}"), state);
    }
    Ok(host)
}

async fn run(
    provider: &AhpBootstrapProvider,
    node_labels: &[&str],
    relation_labels: &[&str],
) -> Result<Vec<BootstrapEvent>> {
    let (tx, mut rx) = mpsc::channel(1024);
    let context = BootstrapContext::new_minimal("server".into(), "agents".into());
    let request = BootstrapRequest {
        query_id: "q".into(),
        node_labels: node_labels.iter().map(|s| s.to_string()).collect(),
        relation_labels: relation_labels.iter().map(|s| s.to_string()).collect(),
        request_id: "r".into(),
    };
    let result = provider.bootstrap(request, &context, tx, None).await?;
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    assert_eq!(result.event_count, events.len());
    assert!(result.source_position.is_none());
    Ok(events)
}

fn inserted(event: &BootstrapEvent) -> &Element {
    match &event.change {
        SourceChange::Insert { element } => element,
        other => panic!("bootstrap must only insert, got {other:?}"),
    }
}

fn label(event: &BootstrapEvent) -> String {
    match inserted(event) {
        Element::Node { metadata, .. } | Element::Relation { metadata, .. } => {
            metadata.labels[0].to_string()
        }
    }
}

fn is_relation(event: &BootstrapEvent) -> bool {
    matches!(inserted(event), Element::Relation { .. })
}

#[tokio::test(flavor = "multi_thread")]
async fn emits_full_graph_across_pages() -> Result<()> {
    let host = host_with_sessions(5).await?;
    let provider = AhpBootstrapProvider::builder()
        .with_config(AhpBootstrapConfig {
            url: host.url(),
            page_size: 2,
            ..Default::default()
        })
        .build()?;
    let events = run(&provider, &[], &[]).await?;
    let count = |l: &str| events.iter().filter(|e| label(e) == l).count();
    assert_eq!(count("Agent"), 1);
    assert_eq!(count("Session"), 5);
    assert_eq!(count("Chat"), 5);
    assert_eq!(count("HAS_CHAT"), 5);
    assert_eq!(count("USES_AGENT"), 5);
    // Nodes are emitted before relations.
    let first_relation = events.iter().position(is_relation).expect("relations");
    assert!(events[first_relation..].iter().all(is_relation));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn honours_label_and_archive_filters() -> Result<()> {
    let host = host_with_sessions(3).await?;
    let provider = AhpBootstrapProvider::builder()
        .with_url(host.url())
        .with_include_archived(false)
        .build()?;
    let events = run(&provider, &["Session"], &[]).await?;
    assert_eq!(events.len(), 2);
    assert!(events.iter().all(|e| label(e) == "Session"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sends_bearer_token() -> Result<()> {
    let host = host_with_sessions(1).await?;
    host.require_bearer_token(Some("secret".into()));
    let denied = AhpBootstrapProvider::builder()
        .with_url(host.url())
        .build()?;
    assert!(run(&denied, &[], &[]).await.is_err());
    let allowed = AhpBootstrapProvider::builder()
        .with_url(host.url())
        .with_bearer_token("secret")
        .build()?;
    assert!(!run(&allowed, &[], &[]).await?.is_empty());
    Ok(())
}
