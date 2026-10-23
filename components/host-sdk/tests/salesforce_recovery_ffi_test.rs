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

//! Offline recovery through independently built Salesforce source and bootstrap
//! plugins. Run with `make test-host-sdk`; no Salesforce account is contacted.

use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::{Context, Result};
use bytes::Bytes;
use drasi_core::models::{Element, ElementValue, SourceChange};
use drasi_host_sdk::callbacks;
use drasi_host_sdk::loader::{load_plugin_from_path, LoadedPlugin};
use drasi_lib::bootstrap::{BootstrapContext, BootstrapRequest};
use drasi_lib::config::SourceSubscriptionSettings;
use drasi_lib::sources::{Source, SourceError};
use drasi_plugin_sdk::descriptor::{BootstrapPluginDescriptor, SourcePluginDescriptor};
use drasi_salesforce_common::{
    write_snapshot_record, GraphRecord, LookupRelationship, SnapshotManifest, SnapshotTicket,
    SALESFORCE_BOOTSTRAP_TICKET_CONTEXT_PROPERTY, SNAPSHOT_MANIFEST_VERSION,
};

fn load(crate_name: &str) -> LoadedPlugin {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let library = format!(
        "{}{}{}",
        std::env::consts::DLL_PREFIX,
        crate_name.replace('-', "_"),
        std::env::consts::DLL_SUFFIX
    );
    let path = root.join("target/debug/plugins").join(library);
    assert!(
        path.is_file(),
        "missing {}; run make build-test-plugins",
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
        nodes: ["Account".into()].into(),
        relations: Default::default(),
        resume_sequence: resume_from.as_ref().map(|_| 0),
        resume_from,
        request_position_handle: true,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn salesforce_schema_discovery_crosses_independently_loaded_plugin() -> Result<()> {
    use drasi_salesforce_common::Catalog;
    use serde_json::json;
    use wiremock::{
        matchers::{method, path},
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
    let descriptions = [
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
                {"name": "Day__c", "type": "date", "nillable": true},
                {"name": "Choices__c", "type": "multipicklist", "nillable": true},
                {"name": "AccountId", "type": "reference", "nillable": true,
                 "relationshipName": "Account", "referenceTo": ["Account"]},
                {"name": "RelatedId", "type": "reference", "nillable": true,
                 "relationshipName": "Related", "referenceTo": ["Contact", "Account"]}
            ]
        }),
    ];
    for description in &descriptions {
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
    let expected = Catalog::from_describes(
        descriptions
            .into_iter()
            .map(serde_json::from_value)
            .collect::<std::result::Result<Vec<_>, _>>()?,
    )?
    .source_schema();
    // Leave the stream handshake pending: inspection must not depend on ingestion readiness.
    let pubsub = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let source_plugin = load("drasi-source-salesforce");
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
        "pubsubEndpoint": format!("http://{}", pubsub.local_addr()?)
    });
    let source = source_plugin.source_plugins[0]
        .create_source("salesforce-schema-ffi", &config, false)
        .await?;
    let (events, _event_rx) = tokio::sync::mpsc::channel(32);
    source
        .initialize(drasi_lib::SourceRuntimeContext::new(
            "schema-ffi-test",
            "salesforce-schema-ffi",
            None,
            events,
            None,
        ))
        .await;
    assert_eq!(source.describe_schema(), None);
    source.start().await?;
    let outcome = async {
        for _ in 0..20 {
            let mut actual = source
                .describe_schema()
                .context("plugin schema was unavailable")?;
            anyhow::ensure!(
                actual == expected,
                "plugin schema differs from static projection"
            );
            actual.nodes.clear();
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    source.stop().await?;
    outcome?;
    assert_eq!(source.describe_schema(), Some(expected));
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
    drop(source_plugin);
    drop(pubsub);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn salesforce_snapshot_and_recovery_cross_independent_plugins() {
    let source_plugin = load("drasi-source-salesforce");
    let bootstrap_plugin = load("drasi-bootstrap-salesforce");
    let spool = tempfile::tempdir().unwrap();
    let config = serde_json::json!({
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
        "spoolRoot": spool.path().to_str().unwrap(),
        "maxSubscribers": 1
    });
    let source = source_plugin.source_plugins[0]
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
    let bootstrap = bootstrap_plugin.bootstrap_plugins[0]
        .create_bootstrap_provider(
            &serde_json::json!({"spoolRoot": spool.path().to_str().unwrap()}),
            &config,
        )
        .await
        .unwrap();
    source.set_bootstrap_provider(bootstrap).await;

    let mut missing_position = settings("missing-position", None);
    missing_position.resume_sequence = Some(77);
    let error = source
        .subscribe(missing_position)
        .await
        .err()
        .expect("nonzero sequence without a position must be rejected");
    assert!(error.to_string().contains("requires a local position"));

    let requested = Bytes::from_static(&[255, 0, 1]);
    let error = source
        .subscribe(settings("bad-position", Some(requested.clone())))
        .await
        .err()
        .expect("malformed position must not silently restart");
    let Some(SourceError::PositionUnavailable {
        source_id,
        requested: actual,
        ..
    }) = error.downcast_ref::<SourceError>()
    else {
        panic!("dynamic source lost the typed recovery error: {error:#}");
    };
    assert_eq!(source_id, "salesforce-ffi");
    assert_eq!(actual, &requested);

    // A failed subscription must not consume the only subscriber slot.
    let mut snapshot = source.subscribe(settings("snapshot", None)).await.unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        snapshot.bootstrap_result_receiver.take().unwrap(),
    )
    .await
    .expect("bootstrap through two plugins stalled")
    .unwrap()
    .unwrap();
    assert_eq!(result.event_count, 0);
    let position = result.source_position.expect("snapshot boundary was lost");
    assert!(!position.is_empty());
    assert!(snapshot.position_handle.is_some());
    assert!(snapshot
        .bootstrap_receiver
        .as_mut()
        .unwrap()
        .recv()
        .await
        .is_none());
    drop(snapshot);

    let mut mismatched = settings("bad-sequence", Some(position.clone()));
    mismatched.resume_sequence = Some(77);
    let error = source
        .subscribe(mismatched)
        .await
        .err()
        .expect("nonzero sequence must match the local cursor");
    assert!(error.to_string().contains("inconsistent resume checkpoint"));

    let resumed = source
        .subscribe(settings("resumed", Some(position)))
        .await
        .unwrap();
    assert!(resumed.bootstrap_receiver.is_none());
    assert!(resumed.bootstrap_result_receiver.is_none());
    assert!(resumed.position_handle.is_some());
    source.remove_position_handle("snapshot").await;
    drop(resumed);
    source.remove_position_handle("resumed").await;
    source.stop().await.unwrap();
    drop(source);
    drop(bootstrap_plugin);
    drop(source_plugin);
}

#[tokio::test(flavor = "current_thread")]
async fn salesforce_bootstrap_plugin_replays_populated_v2_snapshot_fixture() -> Result<()> {
    let bootstrap_plugin = load("drasi-bootstrap-salesforce");
    let spool = tempfile::tempdir().unwrap();
    let ticket = write_populated_v2_snapshot_fixture(spool.path())?;

    let bootstrap = bootstrap_plugin.bootstrap_plugins[0]
        .create_bootstrap_provider(
            &serde_json::json!({
                "spoolRoot": spool.path().to_str().unwrap(),
                "maxRows": 1000,
                "maxRowBytes": 1_048_576
            }),
            &serde_json::json!({}),
        )
        .await
        .unwrap();

    let context = BootstrapContext::with_properties(
        "ffi-test".to_string(),
        "salesforce-ffi".to_string(),
        HashMap::from([(
            SALESFORCE_BOOTSTRAP_TICKET_CONTEXT_PROPERTY.to_string(),
            serde_json::to_value(&ticket).unwrap(),
        )]),
    );
    context.sequence_counter.store(41, Ordering::SeqCst);

    let request = BootstrapRequest {
        query_id: "fixture-query".to_string(),
        node_labels: vec!["Account".to_string(), "Contact".to_string()],
        relation_labels: vec!["Account".to_string()],
        request_id: "fixture-request".to_string(),
    };
    let settings = SourceSubscriptionSettings {
        source_id: "salesforce-ffi".into(),
        query_id: "fixture-query".into(),
        enable_bootstrap: true,
        nodes: ["Account".into(), "Contact".into()].into(),
        relations: ["Account".into()].into(),
        resume_sequence: None,
        resume_from: None,
        request_position_handle: true,
    };

    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(16);
    let result = bootstrap
        .bootstrap(request, &context, event_tx, Some(&settings))
        .await
        .unwrap();

    let mut events = Vec::new();
    while let Some(event) = event_rx.recv().await {
        events.push(event);
    }

    assert_eq!(result.event_count, 3);
    assert_eq!(result.source_position, Some(ticket.local_position()));
    assert_eq!(events.len(), 3);
    assert_eq!(
        events
            .iter()
            .map(|event| event.sequence)
            .collect::<Vec<_>>(),
        vec![41, 42, 43],
        "bootstrap event sequences must preserve context counter progression"
    );
    assert!(
        events
            .iter()
            .all(|event| event.source_id.as_str() == "salesforce-ffi"),
        "bootstrap event source_id must be preserved through plugin boundary"
    );

    let account_node_id = "sf:00D000000000001:Account:001000000000001AAA";
    let contact_node_id = "sf:00D000000000001:Contact:003000000000001AAA";
    let relation_id = "sfrel:00D000000000001:Contact:003000000000001AAA:AccountId";

    let SourceChange::Insert { element } = &events[0].change else {
        panic!("expected Account node insert as first bootstrap event");
    };
    let Element::Node {
        metadata,
        properties,
    } = element
    else {
        panic!("expected node element for first bootstrap event");
    };
    assert_eq!(metadata.reference.source_id.as_ref(), "salesforce-ffi");
    assert_eq!(metadata.reference.element_id.as_ref(), account_node_id);
    assert_eq!(metadata.labels.len(), 1);
    assert_eq!(metadata.labels[0].as_ref(), "Account");
    assert_eq!(metadata.effective_from, 1_700_000_000_100);
    assert_eq!(
        properties.get("Id"),
        Some(&ElementValue::String("001000000000001AAA".into()))
    );
    assert_eq!(
        properties.get("Name"),
        Some(&ElementValue::String("Acme".into()))
    );

    let SourceChange::Insert { element } = &events[1].change else {
        panic!("expected Contact node insert as second bootstrap event");
    };
    let Element::Node {
        metadata,
        properties,
    } = element
    else {
        panic!("expected node element for second bootstrap event");
    };
    assert_eq!(metadata.reference.source_id.as_ref(), "salesforce-ffi");
    assert_eq!(metadata.reference.element_id.as_ref(), contact_node_id);
    assert_eq!(metadata.labels.len(), 1);
    assert_eq!(metadata.labels[0].as_ref(), "Contact");
    assert_eq!(metadata.effective_from, 1_700_000_000_200);
    assert_eq!(
        properties.get("Id"),
        Some(&ElementValue::String("003000000000001AAA".into()))
    );
    assert_eq!(
        properties.get("Name"),
        Some(&ElementValue::String("Ada".into()))
    );

    let SourceChange::Insert { element } = &events[2].change else {
        panic!("expected relation insert as third bootstrap event");
    };
    let Element::Relation {
        metadata,
        in_node,
        out_node,
        properties,
    } = element
    else {
        panic!("expected relation element for third bootstrap event");
    };
    assert_eq!(metadata.reference.source_id.as_ref(), "salesforce-ffi");
    assert_eq!(metadata.reference.element_id.as_ref(), relation_id);
    assert_eq!(metadata.labels.len(), 1);
    assert_eq!(metadata.labels[0].as_ref(), "Account");
    assert_eq!(metadata.effective_from, 1_700_000_000_200);
    assert_eq!(in_node.source_id.as_ref(), "salesforce-ffi");
    assert_eq!(in_node.element_id.as_ref(), contact_node_id);
    assert_eq!(out_node.source_id.as_ref(), "salesforce-ffi");
    assert_eq!(out_node.element_id.as_ref(), account_node_id);
    assert_eq!(
        properties.get("Field"),
        Some(&ElementValue::String("AccountId".into()))
    );
    assert_eq!(
        properties.get("TargetObject"),
        Some(&ElementValue::String("Account".into()))
    );
    assert_eq!(
        properties.get("TargetId"),
        Some(&ElementValue::String("001000000000001AAA".into()))
    );
    Ok(())
}

fn write_populated_v2_snapshot_fixture(spool_root: &Path) -> Result<SnapshotTicket> {
    let source_id = "salesforce-ffi".to_string();
    let source_identity = "fixture-source".to_string();
    let org_id = "00D000000000001".to_string();
    let objects = vec!["Account".to_string(), "Contact".to_string()];
    let epoch = (1u128 << 127) + 9;
    let head_ordinal = 42u64;

    let snapshots_dir = spool_root.join("snapshots");
    std::fs::create_dir_all(&snapshots_dir).with_context(|| {
        format!(
            "failed to create Salesforce fixture snapshot directory {}",
            snapshots_dir.display()
        )
    })?;

    let account = GraphRecord {
        object: "Account".to_string(),
        id: "001000000000001AAA".to_string(),
        properties: BTreeMap::from([("Name".to_string(), ElementValue::String("Acme".into()))]),
        relationships: Vec::new(),
        observed_at_ms: 1_700_000_000_100,
        schema_revision: "schema-revision-fixture".to_string(),
    };
    let contact = GraphRecord {
        object: "Contact".to_string(),
        id: "003000000000001AAA".to_string(),
        properties: BTreeMap::from([("Name".to_string(), ElementValue::String("Ada".into()))]),
        relationships: vec![LookupRelationship {
            field: "AccountId".to_string(),
            label: "Account".to_string(),
            target_object: "Account".to_string(),
            target_id: "001000000000001AAA".to_string(),
        }],
        observed_at_ms: 1_700_000_000_200,
        schema_revision: "schema-revision-fixture".to_string(),
    };

    let rows_rel_path = "snapshots/populated.rows.msgpack".to_string();
    let rows_path = spool_root.join(&rows_rel_path);
    let rows_file = std::fs::File::create(&rows_path)
        .with_context(|| format!("failed to create fixture rows file {}", rows_path.display()))?;
    let mut rows_writer = std::io::BufWriter::new(rows_file);
    write_snapshot_record(&mut rows_writer, &account, 1_048_576)
        .context("failed to encode Account fixture snapshot frame")?;
    write_snapshot_record(&mut rows_writer, &contact, 1_048_576)
        .context("failed to encode Contact fixture snapshot frame")?;
    rows_writer
        .flush()
        .with_context(|| format!("failed to flush fixture rows file {}", rows_path.display()))?;
    drop(rows_writer);

    let rows_bytes = std::fs::read(&rows_path)
        .with_context(|| format!("failed to read fixture rows file {}", rows_path.display()))?;
    let manifest = SnapshotManifest {
        version: SNAPSHOT_MANIFEST_VERSION,
        source_id: source_id.clone(),
        source_identity: source_identity.clone(),
        org_id: org_id.clone(),
        objects: objects.clone(),
        epoch,
        head_ordinal,
        created_at_ms: chrono::Utc::now().timestamp_millis() as u64,
        rows_rel_path,
        row_count: 2,
        rows_sha256: drasi_salesforce_common::sha256_hex(&rows_bytes),
    };
    let manifest_bytes =
        rmp_serde::to_vec_named(&manifest).context("failed to encode fixture snapshot manifest")?;

    let manifest_rel_path = "snapshots/populated.manifest.msgpack".to_string();
    let manifest_path = spool_root.join(&manifest_rel_path);
    std::fs::write(&manifest_path, &manifest_bytes).with_context(|| {
        format!(
            "failed to write fixture manifest file {}",
            manifest_path.display()
        )
    })?;

    Ok(SnapshotTicket {
        version: SNAPSHOT_MANIFEST_VERSION,
        ticket_id: "fixture-ticket".to_string(),
        source_id,
        source_identity,
        org_id,
        objects,
        epoch,
        head_ordinal,
        manifest_rel_path,
        expires_at_ms: (chrono::Utc::now().timestamp_millis() as u64).saturating_add(60_000),
        checksum_sha256: drasi_salesforce_common::sha256_hex(&manifest_bytes),
    })
}
