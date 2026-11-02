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

//! Salesforce uses the existing source ABI, including its in-plugin bootstrap.
//! Run with `make test-salesforce-ffi`; no Salesforce account is contacted.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use bytes::Bytes;
use drasi_core::models::{Element, ElementValue, SourceChange};
use drasi_host_sdk::callbacks;
use drasi_host_sdk::loader::{load_plugin_from_path, LoadedPlugin};
use drasi_lib::channels::SourceEvent;
use drasi_lib::config::SourceSubscriptionSettings;
use drasi_lib::schema::PropertyType;
use drasi_lib::sources::Source;
use drasi_plugin_sdk::descriptor::SourcePluginDescriptor;
use serde_json::json;

fn load() -> LoadedPlugin {
    let library = format!(
        "{}drasi_source_salesforce{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    );
    let path = std::env::current_exe()
        .expect("test executable path")
        .parent()
        .and_then(Path::parent)
        .expect("Cargo target directory")
        .join(library);
    assert!(
        path.is_file(),
        "missing {}; run make test-salesforce-ffi",
        path.display()
    );
    load_plugin_from_path(
        &path,
        std::ptr::null_mut(),
        callbacks::default_log_callback_fn(),
        std::ptr::null_mut(),
        callbacks::default_lifecycle_callback_fn(),
    )
    .expect("load Salesforce plugin")
}

fn settings(query_id: &str, resume_from: Option<Bytes>) -> SourceSubscriptionSettings {
    SourceSubscriptionSettings {
        source_id: "salesforce-ffi".into(),
        query_id: query_id.into(),
        enable_bootstrap: true,
        nodes: ["Account".into(), "Contact".into()].into(),
        relations: ["REL_Account".into()].into(),
        resume_sequence: resume_from.as_ref().map(|_| 0),
        resume_from,
        request_position_handle: true,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn salesforce_schema_snapshot_and_replay_use_existing_ffi() -> Result<()> {
    use wiremock::{
        matchers::{method, path, path_regex},
        Mock, MockServer, ResponseTemplate,
    };

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "offline-schema-fixture",
            "expires_in": 3600
        })))
        .mount(&server)
        .await;
    for description in [
        json!({
            "name": "Account", "queryable": true, "keyPrefix": "001",
            "fields": [
                {"name": "Id", "type": "id", "nillable": false},
                {"name": "Name", "type": "string", "nillable": true},
                {"name": "LastModifiedDate", "type": "datetime", "nillable": false}
            ]
        }),
        json!({
            "name": "Contact", "queryable": true, "keyPrefix": "003",
            "fields": [
                {"name": "Id", "type": "id", "nillable": false},
                {"name": "Name", "type": "string", "nillable": true},
                {"name": "Day__c", "type": "date", "nillable": true},
                {"name": "Choices__c", "type": "multipicklist", "nillable": true},
                {"name": "AccountId", "type": "reference", "nillable": true,
                 "relationshipName": "Account", "referenceTo": ["Account"]},
                {"name": "RelatedId", "type": "reference", "nillable": true,
                 "relationshipName": "Related", "referenceTo": ["Contact", "Account"]}
            ]
        }),
    ] {
        Mock::given(method("GET"))
            .and(path(format!(
                "/services/data/v68.0/sobjects/{}/describe",
                description["name"].as_str().unwrap()
            )))
            .respond_with(ResponseTemplate::new(200).set_body_json(description))
            .expect(1)
            .mount(&server)
            .await;
    }
    let updated = Arc::new(AtomicBool::new(false));
    let query_updated = updated.clone();
    Mock::given(method("GET"))
        .and(path_regex(r"^/services/data/v68.0/query(All)?/?$"))
        .respond_with(move |request: &wiremock::Request| {
            let query = request
                .url
                .query_pairs()
                .find(|(key, _)| key == "q")
                .unwrap()
                .1;
            let record = if query.contains("FROM Account") {
                json!({
                    "Id": "001000000000001AAA",
                    "Name": if query_updated.load(Ordering::SeqCst) { "Acme updated" } else { "Acme" },
                    "LastModifiedDate": "2023-11-14T22:13:20.000Z"
                })
            } else {
                assert!(query.contains("FROM Contact"), "{query}");
                json!({
                    "Id": "003000000000001AAA", "Name": "Ada",
                    "AccountId": "001000000000001AAA", "RelatedId": null,
                    "Day__c": "2023-11-14", "Choices__c": "one;two"
                })
            };
            ResponseTemplate::new(200)
                .set_body_json(json!({"records": [record], "done": true, "totalSize": 1}))
        })
        .mount(&server)
        .await;
    let pubsub = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let plugin = load();
    let spool = tempfile::tempdir()?;
    let config = json!({
        "connection": {
            "instanceUrl": server.uri(),
            "expectedOrgId": "00D000000000001",
            "apiVersion": "68.0",
            "auth": {
                "type": "clientCredentials",
                "tokenUrl": format!("{}/oauth/token", server.uri()),
                "clientId": "offline-fixture",
                "clientSecret": "offline-fixture-not-a-credential"
            }
        },
        "objects": ["Account", "Contact"],
        "spoolRoot": spool.path(),
        "fullScanIntervalSeconds": 1,
        "pubsubEndpoint": format!("http://{}", pubsub.local_addr()?)
    });
    let source = plugin.source_plugins[0]
        .create_source("salesforce-ffi", &config, false)
        .await?;
    let (events, _event_rx) = tokio::sync::mpsc::channel(32);
    source
        .initialize(drasi_lib::SourceRuntimeContext::new(
            "ffi-test",
            "salesforce-ffi",
            None,
            events,
            None,
        ))
        .await;
    assert_eq!(source.describe_schema(), None);
    let mut live_settings = settings("live", None);
    live_settings.enable_bootstrap = false;
    let mut live = source.subscribe(live_settings).await?;
    source.start().await?;
    let outcome = async {
        for _ in 0..3 {
            tokio::time::timeout(Duration::from_secs(10), live.receiver.recv()).await??;
        }
        let expected = source
            .describe_schema()
            .context("plugin schema unavailable")?;
        assert_eq!(expected.nodes.len(), 2);
        for (label, field, data_type) in [
            ("Account", "Id", PropertyType::String),
            ("Account", "LastModifiedDate", PropertyType::Timestamp),
            ("Contact", "Day__c", PropertyType::String),
            ("Contact", "Choices__c", PropertyType::Json),
        ] {
            let node = expected
                .nodes
                .iter()
                .find(|node| node.label == label)
                .unwrap();
            assert_eq!(
                node.properties
                    .iter()
                    .find(|property| property.name == field)
                    .unwrap()
                    .data_type,
                Some(data_type)
            );
        }
        assert_eq!(expected.relations.len(), 2);
        let relation = expected
            .relations
            .iter()
            .find(|relation| relation.label == "REL_Account")
            .unwrap();
        assert_eq!(relation.from.as_deref(), Some("Contact"));
        assert_eq!(relation.to.as_deref(), Some("Account"));
        assert_eq!(
            expected
                .relations
                .iter()
                .find(|relation| relation.label == "REL_Related")
                .unwrap()
                .to,
            None
        );
        for _ in 0..20 {
            let mut actual = source.describe_schema().unwrap();
            assert_eq!(actual, expected);
            actual.nodes.clear();
        }

        let mut snapshot = source.subscribe(settings("snapshot", None)).await?;
        let mut elements = Vec::new();
        while let Some(event) = tokio::time::timeout(
            Duration::from_secs(10),
            snapshot.bootstrap_receiver.as_mut().unwrap().recv(),
        )
        .await?
        {
            let SourceChange::Insert { element } = event.change else {
                panic!("expected snapshot inserts");
            };
            elements.push(element);
        }
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            snapshot.bootstrap_result_receiver.take().unwrap(),
        )
        .await???;
        assert_eq!(result.event_count, 3);
        assert_eq!(elements.len(), 3);
        for name in ["Acme", "Ada"] {
            assert!(elements[..2]
                .iter()
                .any(|element| matches!(element, Element::Node { properties, .. }
                    if properties.get("Name") == Some(&ElementValue::String(name.into())))));
        }
        assert!(
            matches!(&elements[2], Element::Relation { in_node, out_node, metadata, .. }
            if metadata.labels[0].as_ref() == "REL_Account"
                && in_node.element_id.as_ref() == "sf:00D000000000001:Contact:003000000000001AAA"
                && out_node.element_id.as_ref() == "sf:00D000000000001:Account:001000000000001AAA")
        );
        let position = result
            .source_position
            .context("snapshot boundary missing")?;
        drop(snapshot);
        source.remove_position_handle("snapshot").await;
        updated.store(true, Ordering::SeqCst);
        let written = tokio::time::timeout(Duration::from_secs(10), live.receiver.recv()).await??;
        assert!(matches!(&written.event,
            SourceEvent::Change(SourceChange::Update { element: Element::Node { properties, .. } })
                if properties.get("Name") == Some(&ElementValue::String("Acme updated".into()))));
        drop(live);
        source.remove_position_handle("live").await;

        let mut resumed = source
            .subscribe(settings("resumed", Some(position)))
            .await?;
        assert!(resumed.bootstrap_receiver.is_none());
        assert!(resumed.bootstrap_result_receiver.is_none());
        assert!(resumed.position_handle.is_some());
        let replayed =
            tokio::time::timeout(Duration::from_secs(10), resumed.receiver.recv()).await??;
        assert_eq!(replayed.sequence, written.sequence);
        assert_eq!(replayed.source_position, written.source_position);
        assert_eq!(replayed.event, written.event);
        drop(resumed);
        source.remove_position_handle("resumed").await;
        Ok::<_, anyhow::Error>(expected)
    }
    .await;
    source.stop().await?;
    assert_eq!(source.describe_schema(), Some(outcome?));
    source.deprovision().await?;
    assert_eq!(source.describe_schema(), None);
    assert_eq!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| request.url.path().ends_with("/describe"))
            .count(),
        2
    );
    drop(source);
    drop(plugin);
    drop(pubsub);
    spool.close()?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn salesforce_existing_ffi_rejects_invalid_positions_and_resumes_empty_snapshot() {
    let plugin = load();
    let spool = tempfile::tempdir().unwrap();
    let config = json!({
        "connection": {
            "instanceUrl": "https://example.my.salesforce.com",
            "expectedOrgId": "00D000000000001",
            "auth": {
                "type": "clientCredentials",
                "tokenUrl": "https://example.my.salesforce.com/services/oauth2/token",
                "clientId": "offline-fixture",
                "clientSecret": "offline-fixture-not-a-credential"
            }
        },
        "objects": ["Account", "Contact"],
        "spoolRoot": spool.path(),
        "maxSubscribers": 1
    });
    let source = plugin.source_plugins[0]
        .create_source("salesforce-ffi", &config, false)
        .await
        .unwrap();
    let (events, _event_rx) = tokio::sync::mpsc::channel(32);
    source
        .initialize(drasi_lib::SourceRuntimeContext::new(
            "ffi-test",
            "salesforce-ffi",
            None,
            events,
            None,
        ))
        .await;
    assert!(source.supports_replay());
    let mut missing_position = settings("missing-position", None);
    missing_position.resume_sequence = Some(77);
    assert!(source.subscribe(missing_position).await.is_err());
    assert!(source
        .subscribe(settings(
            "bad-position",
            Some(Bytes::from_static(&[255, 0, 1]))
        ))
        .await
        .is_err());

    let mut snapshot = source.subscribe(settings("snapshot", None)).await.unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        snapshot.bootstrap_result_receiver.take().unwrap(),
    )
    .await
    .unwrap()
    .unwrap()
    .unwrap();
    assert_eq!(result.event_count, 0);
    let position = result.source_position.unwrap();
    assert!(!position.is_empty());
    assert!(snapshot
        .bootstrap_receiver
        .as_mut()
        .unwrap()
        .recv()
        .await
        .is_none());
    drop(snapshot);
    source.remove_position_handle("snapshot").await;

    let mut mismatched = settings("bad-sequence", Some(position.clone()));
    mismatched.resume_sequence = Some(77);
    assert!(source.subscribe(mismatched).await.is_err());
    let resumed = source
        .subscribe(settings("resumed", Some(position)))
        .await
        .unwrap();
    assert!(resumed.bootstrap_receiver.is_none());
    assert!(resumed.position_handle.is_some());
    drop(resumed);
    source.remove_position_handle("resumed").await;
    source.deprovision().await.unwrap();
    drop(source);
    drop(plugin);
    spool.close().unwrap();
}
