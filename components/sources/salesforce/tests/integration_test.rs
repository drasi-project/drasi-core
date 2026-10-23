// Copyright 2025 The Drasi Authors.
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

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, ensure, Context, Result};
use apache_avro::{types::Value as Avro, Schema};
use axum::{
    extract::{Path as AxumPath, Query as AxumQuery, State},
    http::StatusCode,
    response::{IntoResponse, Response as HttpResponse},
    routing::{get, post},
    Json, Router,
};
use drasi_index_rocksdb::RocksDbIndexProvider;
use drasi_lib::channels::ResultDiff;
use drasi_lib::indexes::StorageBackendRef;
use drasi_lib::sources::Source;
use drasi_lib::{ComponentStatus, DrasiLib, Query};
use drasi_reaction_application::subscription::SubscriptionOptions;
use drasi_reaction_application::ApplicationReaction;
use drasi_salesforce_common::proto;
use drasi_salesforce_common::{
    InitialCursorBehavior, SalesforceAuthConfig, SalesforceConnectionConfig,
};
use drasi_source_salesforce::{SalesforceSource, SalesforceSourceConfig};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::sync::{broadcast, oneshot, Mutex, RwLock};
use tokio_stream::wrappers::TcpListenerStream;
use tokio_stream::Stream;
use tonic::{Response, Status};

mod metadata_graph;

const SOURCE_ID: &str = "salesforce-source";
const QUERY_ID: &str = "accounts-query";
const ORG_ID: &str = "00D000000000001AAA";
const ID_1: &str = "001000000000001AAA";
const ID_2: &str = "001000000000002AAA";
const CONTACT_1: &str = "003000000000001AAA";
const CONTACT_2: &str = "003000000000002AAA";
const JOIN_QUERY: &str = "MATCH (c:Contact)-[:Account]->(a:Account) RETURN c.Id AS id, c.Name AS contact, a.Name AS account";

#[derive(Clone, Default)]
struct RestHarness {
    rows: Arc<RwLock<BTreeMap<String, BTreeMap<String, Value>>>>,
    sequence: Arc<Mutex<u64>>,
    full_scans: Arc<AtomicUsize>,
    queries: Arc<RwLock<Vec<String>>>,
    scan_failures: Arc<RwLock<HashMap<String, ScanFailure>>>,
    failed_pages: Arc<AtomicUsize>,
    omit_account_phone: Arc<AtomicBool>,
    reject_removed_phone: Arc<AtomicBool>,
    invalid_field_responses: Arc<AtomicUsize>,
    describe_failures: Arc<AtomicUsize>,
    describe_calls: Arc<AtomicUsize>,
    describe_overrides: Arc<RwLock<HashMap<String, Value>>>,
    describe_gate: Arc<RwLock<Option<Arc<tokio::sync::Semaphore>>>>,
}

#[derive(Clone, Copy)]
enum ScanFailure {
    FirstPage,
    Continuation,
}

impl RestHarness {
    async fn upsert(&self, id: &str, name: &str) {
        self.put("Account", json!({"Id": id, "Name": name})).await;
    }

    async fn put(&self, object: &str, mut row: Value) {
        let mut seq = self.sequence.lock().await;
        *seq += 1;
        row["LastModifiedDate"] = json!(format!("2024-01-01T00:00:00.{:03}+0000", *seq));
        let id = row["Id"].as_str().expect("fixture record ID").to_owned();
        self.rows
            .write()
            .await
            .entry(object.to_owned())
            .or_default()
            .insert(id, row);
    }

    async fn contact(&self, id: &str, name: &str, account: Option<&str>) {
        self.put(
            "Contact",
            json!({"Id": id, "Name": name, "AccountId": account}),
        )
        .await;
    }

    async fn remove(&self, id: &str) {
        self.remove_object("Account", id).await;
    }

    async fn remove_object(&self, object: &str, id: &str) {
        if let Some(rows) = self.rows.write().await.get_mut(object) {
            rows.remove(id);
        }
    }

    async fn records_for_query(&self, soql: &str) -> Vec<Value> {
        let selected = parse_ids_from_soql(soql);
        let rows = self.rows.read().await;
        let fields = soql
            .strip_prefix("SELECT ")
            .and_then(|query| query.split_once(" FROM "))
            .expect("fixture accepts SELECT ... FROM ...");
        rows.get(query_object(soql))
            .into_iter()
            .flat_map(|rows| rows.values())
            .filter(|row| {
                selected
                    .as_ref()
                    .is_none_or(|ids| ids.iter().any(|id| row["Id"] == *id))
            })
            .map(|row| {
                fields
                    .0
                    .split(',')
                    .map(str::trim)
                    .map(|field| (field.to_owned(), row[field].clone()))
                    .collect::<serde_json::Map<_, _>>()
                    .into()
            })
            .collect()
    }

    async fn scans(&self, object: &str) -> usize {
        self.queries
            .read()
            .await
            .iter()
            .filter(|query| query_object(query) == object && parse_ids_from_soql(query).is_none())
            .count()
    }
}

fn query_object(soql: &str) -> &str {
    soql.split_once(" FROM ")
        .and_then(|(_, tail)| tail.split_whitespace().next())
        .expect("query must select a fixture object")
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
        "access_token": "integration-test-token",
        "expires_in": 300
    }))
}

async fn describe_object(
    State(rest): State<RestHarness>,
    AxumPath(object): AxumPath<String>,
) -> HttpResponse {
    rest.describe_calls.fetch_add(1, Ordering::Relaxed);
    let gate = rest.describe_gate.read().await.clone();
    let _permit = match gate {
        Some(gate) => Some(gate.acquire_owned().await.expect("Describe gate")),
        None => None,
    };
    if rest
        .describe_failures
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
            remaining.checked_sub(1)
        })
        .is_ok()
    {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    if let Some(description) = rest.describe_overrides.read().await.get(&object) {
        return Json(description.clone()).into_response();
    }
    let mut fields = vec![
        json!({ "name": "Id", "type": "id", "nillable": false }),
        json!({ "name": "Name", "type": "string", "nillable": true, "compoundFieldName": "Name" }),
        json!({ "name": "LastModifiedDate", "type": "datetime", "nillable": false }),
    ];
    let prefix = match object.as_str() {
        "Account" => {
            fields.extend([
                json!({ "name": "Phone", "type": "phone", "nillable": true }),
                json!({ "name": "BillingAddress", "type": "address", "nillable": true }),
                json!({ "name": "BillingCity", "type": "string", "nillable": true, "compoundFieldName": "BillingAddress" }),
                json!({ "name": "ParentId", "type": "reference", "nillable": true, "relationshipName": "Parent", "referenceTo": ["Account"] }),
            ]);
            "001"
        }
        "Contact" => {
            fields.extend([
                json!({ "name": "FirstName", "type": "string", "nillable": true, "compoundFieldName": "Name" }),
                json!({ "name": "LastName", "type": "string", "nillable": true, "compoundFieldName": "Name" }),
                json!({ "name": "Email", "type": "email", "nillable": true }),
                json!({ "name": "MailingAddress", "type": "address", "nillable": true }),
                json!({ "name": "MailingCity", "type": "string", "nillable": true, "compoundFieldName": "MailingAddress" }),
                json!({ "name": "AccountId", "type": "reference", "nillable": true, "relationshipName": "Account", "referenceTo": ["Account"] }),
                json!({ "name": "ReportsToId", "type": "reference", "nillable": true, "relationshipName": "ReportsTo", "referenceTo": ["Contact"] }),
                json!({ "name": "OwnerId", "type": "reference", "nillable": true, "relationshipName": "Owner", "referenceTo": ["User"] }),
            ]);
            "003"
        }
        other => panic!("unexpected Describe object {other}"),
    };
    if object == "Account" && rest.omit_account_phone.load(Ordering::Relaxed) {
        fields.retain(|field| field["name"] != "Phone");
    }
    Json(json!({
        "name": object,
        "queryable": true,
        "keyPrefix": prefix,
        "fields": fields
    }))
    .into_response()
}

async fn query_records(
    State(rest): State<RestHarness>,
    AxumQuery(params): AxumQuery<HashMap<String, String>>,
) -> HttpResponse {
    let soql = params.get("q").cloned().unwrap_or_default();
    let mut records = rest.records_for_query(&soql).await;
    rest.queries.write().await.push(soql.clone());
    if rest.reject_removed_phone.load(Ordering::Relaxed)
        && query_object(&soql) == "Account"
        && soql
            .split_once(" FROM ")
            .expect("SELECT query")
            .0
            .split([',', ' '])
            .any(|field| field == "Phone")
    {
        rest.invalid_field_responses.fetch_add(1, Ordering::Relaxed);
        return (StatusCode::BAD_REQUEST, Json(json!([{
            "errorCode": "INVALID_FIELD", "message": "No such column 'Phone' on entity 'Account'."
        }]))).into_response();
    }
    if parse_ids_from_soql(&soql).is_none() {
        rest.full_scans.fetch_add(1, Ordering::Relaxed);
        match rest.scan_failures.read().await.get(query_object(&soql)) {
            Some(ScanFailure::FirstPage) => {
                rest.failed_pages.fetch_add(1, Ordering::Relaxed);
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            }
            Some(ScanFailure::Continuation) => {
                records.truncate(1);
                return Json(json!({
                    "records": records,
                    "done": false,
                    "nextRecordsUrl": "/services/data/v68.0/query/failed-continuation"
                }))
                .into_response();
            }
            None => {}
        }
    }
    Json(json!({
        "records": records,
        "done": true
    }))
    .into_response()
}

async fn failed_continuation(State(rest): State<RestHarness>) -> StatusCode {
    rest.failed_pages.fetch_add(1, Ordering::Relaxed);
    StatusCode::SERVICE_UNAVAILABLE
}

#[derive(Clone)]
struct ObjectFeed {
    schema_json: String,
    avro_schema: Schema,
    events: broadcast::Sender<proto::FetchResponse>,
}

#[derive(Clone)]
struct PubSubHarness {
    feeds: HashMap<String, ObjectFeed>,
    topic_modes: Arc<RwLock<HashMap<String, TopicMode>>>,
    topic_calls: Arc<AtomicUsize>,
}

#[derive(Clone, Copy)]
enum TopicMode {
    NotFound,
    NotSubscribable,
    Unauthenticated,
}

impl PubSubHarness {
    fn new() -> Self {
        Self {
            feeds: ["Account", "Contact"]
                .into_iter()
                .map(|object| {
                    let schema_json = change_event_schema_json(object);
                    let avro_schema = Schema::parse_str(&schema_json).expect("valid avro schema");
                    let (events, _) = broadcast::channel(32);
                    (
                        object.to_owned(),
                        ObjectFeed {
                            schema_json,
                            avro_schema,
                            events,
                        },
                    )
                })
                .collect(),
            topic_modes: Default::default(),
            topic_calls: Default::default(),
        }
    }

    fn emit_invalidation(&self, operation: &str, ids: &[&str]) {
        self.emit("Account", operation, ids);
    }

    fn receiver_count(&self, object: &str) -> usize {
        self.feeds[object].events.receiver_count()
    }

    fn emit(&self, object: &str, operation: &str, ids: &[&str]) {
        let feed = &self.feeds[object];
        let payload = change_payload(&feed.avro_schema, object, operation, ids);
        let response = proto::FetchResponse {
            events: vec![proto::ConsumerEvent {
                event: Some(proto::ProducerEvent {
                    id: "evt-1".to_string(),
                    schema_id: format!("schema-{object}"),
                    payload,
                    headers: Vec::new(),
                }),
                replay_id: vec![0, 0, 1],
            }],
            latest_replay_id: vec![0, 0, 1],
            pending_num_requested: 0,
            rpc_id: String::new(),
        };
        feed.events
            .send(response)
            .expect("active native subscription");
    }
}

#[derive(Clone)]
struct FakePubSub {
    harness: PubSubHarness,
    header_gate: Option<Arc<HeaderGate>>,
}

#[derive(Default)]
struct HeaderGate {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[tonic::async_trait]
impl proto::pub_sub_server::PubSub for FakePubSub {
    async fn get_topic(
        &self,
        request: tonic::Request<proto::TopicRequest>,
    ) -> std::result::Result<Response<proto::TopicInfo>, Status> {
        let object = topic_object(&request.get_ref().topic_name)?;
        if request.metadata().get("tenantid").is_none() {
            return Err(Status::invalid_argument("missing tenantid metadata"));
        }
        if request.metadata().get("accesstoken").is_none() {
            return Err(Status::invalid_argument("missing accesstoken metadata"));
        }

        self.harness.topic_calls.fetch_add(1, Ordering::Relaxed);
        let can_subscribe = match self.harness.topic_modes.read().await.get(object) {
            Some(TopicMode::NotFound) => return Err(Status::not_found("CDC is not enabled")),
            Some(TopicMode::Unauthenticated) => return Err(Status::unauthenticated("denied")),
            Some(TopicMode::NotSubscribable) => false,
            None => true,
        };
        Ok(Response::new(proto::TopicInfo {
            topic_name: request.get_ref().topic_name.clone(),
            can_subscribe,
            schema_id: format!("schema-{object}"),
            ..Default::default()
        }))
    }

    async fn get_schema(
        &self,
        request: tonic::Request<proto::SchemaRequest>,
    ) -> std::result::Result<Response<proto::SchemaInfo>, Status> {
        let schema_id = &request.get_ref().schema_id;
        let feed = schema_id
            .strip_prefix("schema-")
            .and_then(|object| self.harness.feeds.get(object))
            .ok_or_else(|| Status::not_found("unknown schema"))?;
        Ok(Response::new(proto::SchemaInfo {
            schema_json: feed.schema_json.clone(),
            schema_id: schema_id.clone(),
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
        let object = topic_object(&first.topic_name)?;
        if let Some(gate) = &self.header_gate {
            gate.entered.notify_one();
            gate.release.notified().await;
        }

        let events = self.harness.feeds[object].events.subscribe();
        let stream = futures_util::stream::unfold(
            (events, requests),
            |(mut events, mut requests)| async move {
                loop {
                    tokio::select! {
                        request = requests.message() => match request {
                            Ok(Some(_)) => {}
                            Ok(None) => return None,
                            Err(error) => return Some((Err(error), (events, requests))),
                        },
                        event = events.recv() => match event {
                            Ok(response) => return Some((Ok(response), (events, requests))),
                            Err(broadcast::error::RecvError::Lagged(count)) => {
                                return Some((Err(Status::resource_exhausted(format!("fixture lost {count} events"))), (events, requests)));
                            }
                            Err(broadcast::error::RecvError::Closed) => return None,
                        }
                    }
                }
            },
        );

        Ok(Response::new(Box::pin(stream)))
    }
}

fn topic_object(topic: &str) -> std::result::Result<&str, Status> {
    let object = topic
        .strip_prefix("/data/")
        .and_then(|value| value.strip_suffix("ChangeEvent"))
        .ok_or_else(|| Status::invalid_argument("unexpected topic"))?;
    match object {
        "Account" | "Contact" => Ok(object),
        _ => Err(Status::not_found("unknown fixture object")),
    }
}

fn change_event_schema_json(object: &str) -> String {
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
        "name": format!("{object}ChangeEvent"),
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

fn change_payload(schema: &Schema, object: &str, operation: &str, ids: &[&str]) -> Vec<u8> {
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
                ("entityName".to_string(), Avro::String(object.to_string())),
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

fn matches_change(entry: &ResultDiff, kind: &str, fields: &[(&str, &str)]) -> bool {
    match (kind, entry) {
        ("ADD", ResultDiff::Add { data, .. })
        | ("DELETE", ResultDiff::Delete { data, .. })
        | ("UPDATE_AFTER", ResultDiff::Update { after: data, .. }) => matches_fields(data, fields),
        _ => false,
    }
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

async fn wait_for_change<F>(
    subscription: &mut drasi_reaction_application::subscription::Subscription,
    timeout: Duration,
    mut matcher: F,
) -> Result<ResultDiff>
where
    F: FnMut(&ResultDiff) -> bool,
{
    let mut last_results = Vec::new();
    tokio::time::timeout(timeout, async {
        loop {
            let result = subscription
                .recv()
                .await
                .context("reaction subscription closed")?;
            last_results = result.results.clone();
            for entry in &result.results {
                if matcher(entry) {
                    return Ok(entry.clone());
                }
            }
        }
    })
    .await
    .with_context(|| {
        format!("timed out waiting for expected change. last results: {last_results:?}")
    })?
}

#[tokio::test(flavor = "current_thread")]
async fn scans_and_shutdown_progress_while_subscribe_headers_are_pending() -> Result<()> {
    use drasi_lib::sources::Source;

    let temp = TempDir::new()?;
    let rest = RestHarness::default();
    rest.upsert(ID_1, "Already present").await;
    let app = Router::new()
        .route("/oauth/token", post(oauth_token))
        .route(
            "/services/data/v68.0/sobjects/:object/describe",
            get(describe_object),
        )
        .route("/services/data/v68.0/query", get(query_records))
        .with_state(rest.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let instance = format!("http://{}", listener.local_addr()?);
    let rest_server = tokio::spawn(async move { axum::serve(listener, app).await });

    let headers = Arc::new(HeaderGate::default());
    let service = FakePubSub {
        harness: PubSubHarness::new(),
        header_gate: Some(headers.clone()),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}", listener.local_addr()?);
    let grpc_server = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(proto::pub_sub_server::PubSubServer::new(service))
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
    });

    let config: SalesforceSourceConfig = serde_json::from_value(json!({
        "connection": {
            "instanceUrl": instance,
            "expectedOrgId": ORG_ID,
            "auth": {
                "type": "clientCredentials",
                "tokenUrl": format!("{instance}/oauth/token"),
                "clientId": "fixture",
                "clientSecret": "fixture"
            }
        },
        "objects": ["Account"],
        "pubsubEndpoint": endpoint,
        "spoolRoot": temp.path().join("source"),
        "fullScanIntervalSeconds": 1
    }))?;
    let source = SalesforceSource::builder(SOURCE_ID)
        .with_config(config)
        .build()?;
    let mut subscription = source
        .subscribe(drasi_lib::config::SourceSubscriptionSettings {
            source_id: SOURCE_ID.into(),
            query_id: QUERY_ID.into(),
            enable_bootstrap: false,
            nodes: ["Account".into()].into(),
            relations: Default::default(),
            resume_from: None,
            resume_sequence: None,
            request_position_handle: true,
        })
        .await?;
    source.start().await?;
    let entered = tokio::time::timeout(Duration::from_secs(3), headers.entered.notified()).await;
    let observed = tokio::time::timeout(Duration::from_secs(3), subscription.receiver.recv()).await;
    rest.upsert(ID_1, "Changed while headers are pending").await;
    let repaired = tokio::time::timeout(Duration::from_secs(3), subscription.receiver.recv()).await;
    let stopped = tokio::time::timeout(Duration::from_secs(1), source.stop()).await;
    headers.release.notify_one();
    grpc_server.abort();
    rest_server.abort();
    let _ = grpc_server.await;
    let _ = rest_server.await;

    entered.expect("test did not reach the pending Subscribe response");
    let event = observed.expect("REST scan waited for the first native event")?;
    assert!(event.source_position.is_some());
    match &event.event {
        drasi_lib::channels::SourceEvent::Change(change) => {
            assert!(change.get_reference().element_id.ends_with(ID_1));
        }
        other => panic!("expected the existing row, got {other:?}"),
    }
    let repair = repaired.expect("periodic reconciliation waited for Subscribe headers")?;
    assert!(repair.sequence > event.sequence);
    match &repair.event {
        drasi_lib::channels::SourceEvent::Change(drasi_core::models::SourceChange::Update {
            element,
        }) => assert_eq!(
            element.get_property("Name"),
            &drasi_core::models::ElementValue::String("Changed while headers are pending".into())
        ),
        other => panic!("expected REST repair while headers are pending, got {other:?}"),
    }
    stopped.expect("stop waited for Subscribe response headers")?;
    Ok(())
}

#[tokio::test]
async fn test_salesforce_source_detects_cdc_and_rest_repair() -> Result<()> {
    let timeout = Duration::from_secs(60);
    tokio::time::timeout(timeout, async {
        let temp = TempDir::new().map_err(|e| anyhow!("failed to create temp directory: {e}"))?;
        let spool_root = temp.path().join("spool");

        let rest = RestHarness::default();
        let rest_app = Router::new()
            .route("/oauth/token", post(oauth_token))
            .route(
                "/services/data/v68.0/sobjects/:object/describe",
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
            header_gate: None,
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
                full_scan_interval_seconds: 300,
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
            .map_err(|e| anyhow!("failed to build Salesforce source: {e:#}"))?;

        let query = Query::cypher(QUERY_ID)
            .query("MATCH (a:Account) RETURN a.Id AS id, a.Name AS name")
            .from_source(SOURCE_ID)
            .enable_bootstrap(true)
            .auto_start(true)
            .build();

        let (reaction, handle) = ApplicationReaction::builder("app-reaction")
            .with_query(QUERY_ID)
            .build();

        let drasi = DrasiLib::builder()
            .with_id("salesforce-integration-test")
            .with_source(source)
            .with_query(query)
            .with_reaction(reaction)
            .build()
            .await
            .map_err(|e| anyhow!("failed to build drasi lib: {e:#}"))?;

        drasi
            .start()
            .await
            .map_err(|e| anyhow!("failed to start drasi lib: {e:#}"))?;
        let mut subscription = handle
            .subscribe_with_options(
                SubscriptionOptions::default().with_timeout(Duration::from_secs(5)),
            )
            .await
            .map_err(|e| anyhow!("failed to subscribe to application reaction: {e:#}"))?;

        tokio::time::timeout(Duration::from_secs(5), async {
            while pubsub_harness.receiver_count("Account") == 0
                || rest.full_scans.load(std::sync::atomic::Ordering::Relaxed) < 2
                || drasi.get_query_status(QUERY_ID).await? != drasi_lib::ComponentStatus::Running
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;

        rest.upsert(ID_1, "Acme").await;
        pubsub_harness.emit_invalidation("CREATE", &[ID_1]);
        wait_for_change(&mut subscription, Duration::from_secs(20), |entry| {
            matches_change(entry, "ADD", &[("id", ID_1), ("name", "Acme")])
        })
        .await
        .map_err(|e| {
            anyhow!("did not observe CREATE via CDC invalidation + REST hydration: {e:#}")
        })?;

        rest.upsert(ID_1, "Acme Updated").await;
        pubsub_harness.emit_invalidation("UPDATE", &[ID_1]);
        wait_for_change(&mut subscription, Duration::from_secs(20), |entry| {
            matches_update(
                entry,
                &[("id", ID_1), ("name", "Acme")],
                &[("id", ID_1), ("name", "Acme Updated")],
            )
        })
        .await
        .map_err(|e| {
            anyhow!("did not observe UPDATE via CDC invalidation + REST hydration: {e:#}")
        })?;

        rest.remove(ID_1).await;
        pubsub_harness.emit_invalidation("DELETE", &[ID_1]);
        wait_for_change(&mut subscription, Duration::from_secs(20), |entry| {
            matches_change(entry, "DELETE", &[("id", ID_1), ("name", "Acme Updated")])
        })
        .await
        .map_err(|e| {
            anyhow!("did not observe DELETE via CDC invalidation + REST hydration: {e:#}")
        })?;

        // GAP_OVERFLOW has no usable IDs and requires an immediate complete scan.
        rest.upsert(ID_2, "Recovered By Full Scan").await;
        pubsub_harness.emit_invalidation("GAP_OVERFLOW", &[]);
        wait_for_change(&mut subscription, Duration::from_secs(20), |entry| {
            matches_change(
                entry,
                "ADD",
                &[("id", ID_2), ("name", "Recovered By Full Scan")],
            )
        })
        .await
        .map_err(|e| anyhow!("did not observe full-scan repair after native gap: {e:#}"))?;

        drasi
            .stop()
            .await
            .map_err(|e| anyhow!("failed to stop drasi lib: {e:#}"))?;
        let _ = grpc_shutdown_tx.send(());
        let _ = tokio::time::timeout(Duration::from_secs(5), grpc_server).await;
        rest_server.abort();
        Ok::<_, anyhow::Error>(())
    })
    .await
    .map_err(|_| anyhow!("integration test timed out"))??;

    Ok(())
}
