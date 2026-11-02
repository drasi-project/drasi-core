// Copyright 2025 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.

use std::collections::{BTreeMap, HashMap};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use apache_avro::{types::Value as Avro, Schema};
use axum::{
    extract::{Query as AxumQuery, State},
    routing::{get, post},
    Json, Router,
};
use drasi_lib::channels::ResultDiff;
use drasi_salesforce_common::proto;
use drasi_salesforce_common::{
    InitialCursorBehavior, SalesforceAuthConfig, SalesforceConnectionConfig,
};
use drasi_source_salesforce::{SalesforceSource, SalesforceSourceConfig};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::sync::{broadcast, oneshot, Mutex, RwLock};
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tokio_stream::Stream;
use tonic::{Response, Status};

const SOURCE_ID: &str = "salesforce-source";
const TOPIC_NAME: &str = "/data/AccountChangeEvent";
const ORG_ID: &str = "00D000000000001AAA";
const ID_1: &str = "001000000000001AAA";
const ID_2: &str = "001000000000002AAA";

#[derive(Clone, Debug)]
struct AccountRow {
    id: String,
    name: String,
    last_modified_date: String,
}

#[derive(Clone, Default)]
struct RestHarness {
    rows: Arc<RwLock<BTreeMap<String, AccountRow>>>,
    sequence: Arc<Mutex<u64>>,
}

impl RestHarness {
    async fn upsert(&self, id: &str, name: &str) {
        let mut seq = self.sequence.lock().await;
        *seq = seq.saturating_add(1);
        let timestamp = format!("2024-01-01T00:00:{:02}.000+0000", *seq % 60);
        let row = AccountRow {
            id: id.to_string(),
            name: name.to_string(),
            last_modified_date: timestamp,
        };
        self.rows.write().await.insert(id.to_string(), row);
    }

    async fn remove(&self, id: &str) {
        self.rows.write().await.remove(id);
    }

    async fn records_for_query(&self, soql: &str) -> Vec<Value> {
        let selected = parse_ids_from_soql(soql);
        let rows = self.rows.read().await;
        rows.values()
            .filter(|row| selected.as_ref().is_none_or(|ids| ids.contains(&row.id)))
            .map(|row| {
                json!({
                    "Id": row.id,
                    "Name": row.name,
                    "LastModifiedDate": row.last_modified_date
                })
            })
            .collect()
    }
}

fn parse_ids_from_soql(soql: &str) -> Option<Vec<String>> {
    let marker = "WHERE Id IN (";
    let start = soql.find(marker)?;
    let tail = &soql[start + marker.len()..];
    let end = tail.find(')')?;
    let ids = tail[..end]
        .split(',')
        .map(|entry| entry.trim().trim_matches('\'').to_string())
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    Some(ids)
}

async fn oauth_token() -> Json<Value> {
    Json(json!({
        "access_token": "example-token",
        "expires_in": 300
    }))
}

async fn describe_object() -> Json<Value> {
    Json(json!({
        "name": "Account",
        "queryable": true,
        "keyPrefix": "001",
        "fields": [
            { "name": "Id", "type": "id", "nillable": false, "compoundFieldName": null },
            { "name": "Name", "type": "string", "nillable": true, "compoundFieldName": null },
            { "name": "LastModifiedDate", "type": "datetime", "nillable": false, "compoundFieldName": null }
        ]
    }))
}

async fn query_records(
    State(rest): State<RestHarness>,
    AxumQuery(params): AxumQuery<HashMap<String, String>>,
) -> Json<Value> {
    let soql = params.get("q").cloned().unwrap_or_default();
    let records = rest.records_for_query(&soql).await;
    Json(json!({
        "records": records,
        "done": true
    }))
}

#[derive(Clone)]
struct PubSubHarness {
    schema_json: String,
    avro_schema: Schema,
    events: broadcast::Sender<proto::FetchResponse>,
}

impl PubSubHarness {
    fn new() -> Self {
        let schema_json = account_change_event_schema_json();
        let avro_schema = Schema::parse_str(&schema_json).expect("valid avro schema");
        let (events, _) = broadcast::channel(32);
        Self {
            schema_json,
            avro_schema,
            events,
        }
    }

    fn emit_invalidation(&self, operation: &str, ids: &[&str]) {
        let payload = account_change_payload(&self.avro_schema, operation, ids);
        let response = proto::FetchResponse {
            events: vec![proto::ConsumerEvent {
                event: Some(proto::ProducerEvent {
                    id: "evt-1".to_string(),
                    schema_id: "schema-1".to_string(),
                    payload,
                    headers: Vec::new(),
                }),
                replay_id: vec![0, 0, 1],
            }],
            latest_replay_id: vec![0, 0, 1],
            pending_num_requested: 0,
            rpc_id: String::new(),
        };
        let _ = self.events.send(response);
    }
}

#[derive(Clone)]
struct FakePubSub {
    harness: PubSubHarness,
}

#[tonic::async_trait]
impl proto::pub_sub_server::PubSub for FakePubSub {
    async fn get_topic(
        &self,
        request: tonic::Request<proto::TopicRequest>,
    ) -> std::result::Result<Response<proto::TopicInfo>, Status> {
        if request.get_ref().topic_name != TOPIC_NAME {
            return Err(Status::invalid_argument("unexpected topic"));
        }
        Ok(Response::new(proto::TopicInfo {
            topic_name: TOPIC_NAME.to_string(),
            can_subscribe: true,
            schema_id: "schema-1".to_string(),
            ..Default::default()
        }))
    }

    async fn get_schema(
        &self,
        request: tonic::Request<proto::SchemaRequest>,
    ) -> std::result::Result<Response<proto::SchemaInfo>, Status> {
        if request.get_ref().schema_id != "schema-1" {
            return Err(Status::not_found("unknown schema"));
        }
        Ok(Response::new(proto::SchemaInfo {
            schema_json: self.harness.schema_json.clone(),
            schema_id: "schema-1".to_string(),
            rpc_id: String::new(),
        }))
    }

    type SubscribeStream =
        Pin<Box<dyn Stream<Item = std::result::Result<proto::FetchResponse, Status>> + Send>>;

    async fn subscribe(
        &self,
        request: tonic::Request<tonic::Streaming<proto::FetchRequest>>,
    ) -> std::result::Result<Response<Self::SubscribeStream>, Status> {
        let mut requests = request.into_inner();
        let first = requests
            .message()
            .await?
            .ok_or_else(|| Status::invalid_argument("missing initial fetch request"))?;
        if first.topic_name != TOPIC_NAME {
            return Err(Status::invalid_argument("unexpected fetch topic"));
        }

        tokio::spawn(async move { while requests.message().await.ok().flatten().is_some() {} });

        let mut events = self.harness.events.subscribe();
        let (tx, rx) = tokio::sync::mpsc::channel(32);
        tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(response) => {
                        if tx.send(Ok(response)).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }
}

fn account_change_event_schema_json() -> String {
    let change_types = [
        "CREATE",
        "UPDATE",
        "DELETE",
        "UNDELETE",
        "GAP_CREATE",
        "GAP_UPDATE",
        "GAP_DELETE",
        "GAP_UNDELETE",
        "GAP_OVERFLOW",
        "SNAPSHOT",
    ];

    json!({
        "type":"record",
        "name":"AccountChangeEvent",
        "fields":[
            {"name":"ChangeEventHeader","type":{
                "type":"record","name":"Header","fields":[
                    {"name":"entityName","type":"string"},
                    {"name":"changeType","type":{"type":"enum","name":"ChangeType","symbols":change_types}},
                    {"name":"recordIds","type":{"type":"array","items":"string"}},
                    {"name":"commitTimestamp","type":"long"},
                    {"name":"changedFields","type":{"type":"array","items":"string"}},
                    {"name":"nulledFields","type":{"type":"array","items":"string"}},
                    {"name":"diffFields","type":{"type":"array","items":"string"}}
                ]
            }},
            {"name":"Name","type":["null","string"]}
        ]
    })
    .to_string()
}

fn account_change_payload(schema: &Schema, operation: &str, ids: &[&str]) -> Vec<u8> {
    let change_types = [
        "CREATE",
        "UPDATE",
        "DELETE",
        "UNDELETE",
        "GAP_CREATE",
        "GAP_UPDATE",
        "GAP_DELETE",
        "GAP_UNDELETE",
        "GAP_OVERFLOW",
        "SNAPSHOT",
    ];
    let operation_index = change_types
        .iter()
        .position(|value| *value == operation)
        .expect("supported operation");

    let array = |entries: &[&str]| {
        Avro::Array(
            entries
                .iter()
                .map(|entry| Avro::String((*entry).to_string()))
                .collect(),
        )
    };

    let value = Avro::Record(vec![
        (
            "ChangeEventHeader".to_string(),
            Avro::Record(vec![
                (
                    "entityName".to_string(),
                    Avro::String("Account".to_string()),
                ),
                (
                    "changeType".to_string(),
                    Avro::Enum(operation_index as u32, operation.to_string()),
                ),
                ("recordIds".to_string(), array(ids)),
                ("commitTimestamp".to_string(), Avro::Long(1_700_000_000_000)),
                ("changedFields".to_string(), array(&[])),
                ("nulledFields".to_string(), array(&[])),
                ("diffFields".to_string(), array(&[])),
            ]),
        ),
        (
            "Name".to_string(),
            Avro::Union(1, Box::new(Avro::String("ignored".to_string()))),
        ),
    ]);

    apache_avro::to_avro_datum(schema, value).expect("valid payload")
}

fn value_as_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn matches_fields(data: &Value, fields: &[(&str, &str)]) -> bool {
    fields.iter().all(|(field, expected)| {
        data.get(*field)
            .and_then(value_as_string)
            .map(|value| value == *expected)
            .unwrap_or(false)
    })
}

fn matches_update(
    entry: &ResultDiff,
    before_fields: &[(&str, &str)],
    after_fields: &[(&str, &str)],
) -> bool {
    match entry {
        ResultDiff::Update { before, after, .. } => {
            matches_fields(before, before_fields) && matches_fields(after, after_fields)
        }
        _ => false,
    }
}

fn matches_change(entry: &ResultDiff, kind: &str, fields: &[(&str, &str)]) -> bool {
    match (kind, entry) {
        ("ADD", ResultDiff::Add { data, .. }) | ("DELETE", ResultDiff::Delete { data, .. }) => {
            matches_fields(data, fields)
        }
        _ => false,
    }
}

async fn wait_for_change<F>(
    subscription: &mut drasi_reaction_application::subscription::Subscription,
    timeout: Duration,
    mut matcher: F,
) -> Result<ResultDiff>
where
    F: FnMut(&ResultDiff) -> bool,
{
    let started = Instant::now();
    while started.elapsed() < timeout {
        if let Some(result) = subscription.recv().await {
            for entry in &result.results {
                if matcher(entry) {
                    return Ok(entry.clone());
                }
            }
        }
    }
    anyhow::bail!("timed out waiting for expected change")
}

pub async fn run(diagnose_only: bool) -> Result<()> {
    let timeout = Duration::from_secs(60);

    tokio::time::timeout(timeout, async move {
        let temp = TempDir::new().context("failed to create temp directory")?;
        let spool_root = temp.path().join("spool");

        let rest = RestHarness::default();
        let rest_app = Router::new()
            .route("/oauth/token", post(oauth_token))
            .route(
                "/services/data/v68.0/sobjects/Account/describe",
                get(describe_object),
            )
            .route("/services/data/v68.0/query", get(query_records))
            .with_state(rest.clone());
        let rest_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let rest_address = rest_listener.local_addr()?;
        let rest_server = tokio::spawn(async move {
            let _ = axum::serve(rest_listener, rest_app).await;
        });
        let instance_url = format!("http://{rest_address}");

        let pubsub_harness = PubSubHarness::new();
        let pubsub_service = FakePubSub {
            harness: pubsub_harness.clone(),
        };
        let grpc_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let grpc_address = grpc_listener.local_addr()?;
        let (grpc_shutdown_tx, grpc_shutdown_rx) = oneshot::channel();
        let grpc_server = tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(proto::pub_sub_server::PubSubServer::new(pubsub_service))
                .serve_with_incoming_shutdown(TcpListenerStream::new(grpc_listener), async {
                    let _ = grpc_shutdown_rx.await;
                })
                .await
        });
        let pubsub_endpoint = format!("http://{grpc_address}");

        println!("REST harness: {instance_url}");
        println!("Pub/Sub harness: {pubsub_endpoint}");
        println!("Spool root: {}", spool_root.display());

        if diagnose_only {
            println!("DIAGNOSE_OK");
            rest_server.abort();
            let _ = grpc_shutdown_tx.send(());
            let _ = tokio::time::timeout(Duration::from_secs(5), grpc_server).await;
            return Ok::<_, anyhow::Error>(());
        }

        let source = SalesforceSource::builder(SOURCE_ID)
            .with_config(SalesforceSourceConfig {
                connection: SalesforceConnectionConfig {
                    instance_url: instance_url.clone(),
                    expected_org_id: ORG_ID.to_string(),
                    api_version: "68.0".to_string(),
                    auth: SalesforceAuthConfig::ClientCredentials {
                        token_url: format!("{instance_url}/oauth/token"),
                        client_id: "client-id".to_string(),
                        client_secret: "client-secret".to_string(),
                    },
                },
                objects: vec!["Account".to_string()],
                pubsub_endpoint,
                reconcile_interval_seconds: 1,
                full_scan_interval_seconds: 1,
                max_rows: 10_000,
                max_events: 10_000,
                retention_hours: 24,
                max_projected_row_bytes: 1024 * 1024,
                max_scan_bytes: 8 * 1024 * 1024,
                max_replay_page_events: 10,
                max_replay_page_bytes: 64 * 1024,
                max_subscribers: 8,
                spool_root: spool_root.display().to_string(),
                snapshot_ticket_ttl_seconds: 300,
                max_snapshot_leases: 8,
                max_spool_bytes: 8 * 1024 * 1024 * 1024,
                initial_cursor_behavior: InitialCursorBehavior::Latest,
            })
            .build()
            .context("failed to build source")?;

        let queries = super::demo_queries(&["Account".into()], &[])?;
        let (drasi, mut subscription) = crate::build_runtime(source, temp.path(), &queries).await?;
        drasi.start().await?;

        tokio::time::sleep(Duration::from_secs(1)).await;

        rest.upsert(ID_1, "Acme").await;
        pubsub_harness.emit_invalidation("CREATE", &[ID_1]);
        wait_for_change(&mut subscription, Duration::from_secs(20), |entry| {
            matches_change(entry, "ADD", &[("id", ID_1), ("name", "Acme")])
        })
        .await?;
        println!("CREATE_DETECTED ✓");

        rest.upsert(ID_1, "Acme Updated").await;
        pubsub_harness.emit_invalidation("UPDATE", &[ID_1]);
        wait_for_change(&mut subscription, Duration::from_secs(20), |entry| {
            matches_update(
                entry,
                &[("id", ID_1), ("name", "Acme")],
                &[("id", ID_1), ("name", "Acme Updated")],
            )
        })
        .await?;
        println!("UPDATE_DETECTED ✓");

        rest.remove(ID_1).await;
        pubsub_harness.emit_invalidation("DELETE", &[ID_1]);
        wait_for_change(&mut subscription, Duration::from_secs(20), |entry| {
            matches_change(entry, "DELETE", &[("id", ID_1), ("name", "Acme Updated")])
        })
        .await?;
        println!("DELETE_DETECTED ✓");

        rest.upsert(ID_2, "Recovered By Full Scan").await;
        wait_for_change(&mut subscription, Duration::from_secs(20), |entry| {
            matches_change(
                entry,
                "ADD",
                &[("id", ID_2), ("name", "Recovered By Full Scan")],
            )
        })
        .await?;
        println!("FULL_SCAN_REPAIR_DETECTED ✓");

        drasi.shutdown().await?;
        rest_server.abort();
        let _ = grpc_shutdown_tx.send(());
        let _ = tokio::time::timeout(Duration::from_secs(5), grpc_server).await;
        println!("EXAMPLE_COMPLETE ✓");
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("example timed out")??;

    Ok(())
}
