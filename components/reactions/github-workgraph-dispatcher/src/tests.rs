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

use crate::config::GitHubWorkGraphDispatcherConfig;
use crate::descriptor::GitHubWorkGraphDispatcherDescriptor;
use crate::dispatcher::{Clock, DispatcherEngine, LeaseIdGenerator};
use crate::github::{GitHubApi, PostDisposition, RemoteComment, RestGitHubApi};
use crate::model::{
    sha256_digest, CapacityRow, DispatchableTask, Reservation, ReservationPhase, RESERVATION_PREFIX,
};
use crate::reaction::INBOX_PREFIX;
use crate::{GitHubWorkGraphDispatcher, GitHubWorkGraphDispatcherBuilder};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use drasi_github_workgraph::{canonical_task_lease_body, TaskLease};
use drasi_lib::channels::{ComponentStatus, QueryResult, ResultDiff};
use drasi_lib::component_graph::ComponentUpdate;
use drasi_lib::context::ReactionRuntimeContext;
use drasi_lib::queries::{FetchError, OutboxStream, SnapshotStream};
use drasi_lib::reactions::{BootstrapBackend, BootstrapContext, ReactionCheckpoint};
use drasi_lib::state_store::StateStoreProvider;
use drasi_lib::{MemoryStateStoreProvider, Reaction};
use drasi_plugin_sdk::ReactionPluginDescriptor;
use drasi_state_store_redb::RedbStateStoreProvider;
use serde_json::json;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tempfile::TempDir;
use tokio::sync::Mutex;
use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const REACTION_ID: &str = "dispatcher";
const QUERY_ID: &str = "workqueue-capacity";

struct SnapshotBackend {
    rows: Vec<(u64, serde_json::Value)>,
    sequence: u64,
}

#[async_trait]
impl BootstrapBackend for SnapshotBackend {
    async fn fetch_snapshot(&self) -> Result<SnapshotStream, FetchError> {
        Ok(SnapshotStream::from_keyed_stream(
            tokio_stream::iter(self.rows.clone()),
            self.sequence,
            99,
        ))
    }

    async fn fetch_outbox(&self, _after_sequence: u64) -> Result<OutboxStream, FetchError> {
        Ok(OutboxStream::from_stream(tokio_stream::empty(), 0, 99))
    }

    async fn read_checkpoint(&self) -> anyhow::Result<Option<ReactionCheckpoint>> {
        Ok(None)
    }

    async fn write_checkpoint(&self, _checkpoint: &ReactionCheckpoint) -> anyhow::Result<()> {
        Ok(())
    }
}

#[derive(Clone)]
struct FixedClock(DateTime<Utc>);

#[async_trait]
impl Clock for FixedClock {
    async fn now(&self) -> DateTime<Utc> {
        self.0
    }
}

struct FixedIds(Mutex<VecDeque<String>>);

impl FixedIds {
    fn new(ids: &[&str]) -> Self {
        Self(Mutex::new(ids.iter().map(|id| (*id).to_string()).collect()))
    }
}

impl LeaseIdGenerator for FixedIds {
    fn generate(&self) -> String {
        self.0
            .try_lock()
            .expect("ID generator is only used by the serialized dispatcher")
            .pop_front()
            .expect("test supplied enough lease IDs")
    }
}

#[derive(Clone, Debug)]
enum FakePost {
    Accept,
    AmbiguousWrite,
    AmbiguousAbsent,
    Reject,
}

#[derive(Default)]
struct FakeGitHubState {
    behavior: VecDeque<FakePost>,
    comments: Vec<RemoteComment>,
    post_bodies: Vec<String>,
    next_id: u64,
}

#[derive(Default)]
struct FakeGitHub {
    state: Mutex<FakeGitHubState>,
}

impl FakeGitHub {
    async fn with_behavior(behavior: Vec<FakePost>) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(FakeGitHubState {
                behavior: behavior.into(),
                comments: Vec::new(),
                post_bodies: Vec::new(),
                next_id: 1,
            }),
        })
    }

    async fn add_comment(&self, body: String) {
        let mut state = self.state.lock().await;
        let id = state.next_id;
        state.next_id += 1;
        state.comments.push(RemoteComment {
            database_id: id,
            node_id: format!("IC_{id}"),
            body,
        });
    }

    async fn post_bodies(&self) -> Vec<String> {
        self.state.lock().await.post_bodies.clone()
    }

    async fn comments(&self) -> Vec<RemoteComment> {
        self.state.lock().await.comments.clone()
    }
}

#[async_trait]
impl GitHubApi for FakeGitHub {
    async fn post_comment(
        &self,
        _owner: &str,
        _repository: &str,
        _issue_number: u64,
        body: &str,
    ) -> PostDisposition {
        let mut state = self.state.lock().await;
        state.post_bodies.push(body.to_string());
        let behavior = state.behavior.pop_front().unwrap_or(FakePost::Accept);
        match behavior {
            FakePost::Reject => PostDisposition::Rejected("definitive 422".to_string()),
            FakePost::AmbiguousAbsent => PostDisposition::Ambiguous {
                reason: "transport timeout".to_string(),
                retry_after: None,
            },
            FakePost::Accept | FakePost::AmbiguousWrite => {
                let id = state.next_id;
                state.next_id += 1;
                let comment = RemoteComment {
                    database_id: id,
                    node_id: format!("IC_{id}"),
                    body: body.to_string(),
                };
                state.comments.push(comment.clone());
                if matches!(behavior, FakePost::Accept) {
                    PostDisposition::Accepted(comment)
                } else {
                    PostDisposition::Ambiguous {
                        reason: "response lost".to_string(),
                        retry_after: None,
                    }
                }
            }
        }
    }

    async fn list_comments(
        &self,
        _owner: &str,
        _repository: &str,
        _issue_number: u64,
    ) -> anyhow::Result<Vec<RemoteComment>> {
        Ok(self.state.lock().await.comments.clone())
    }
}

fn config() -> GitHubWorkGraphDispatcherConfig {
    GitHubWorkGraphDispatcherConfig {
        token: "test-token".to_string(),
        initial_retry_delay_ms: 1,
        ..Default::default()
    }
}

fn row(slot_count: u32, task_count: usize) -> CapacityRow {
    let free_slot_ids = (1..=slot_count)
        .map(|number| format!("validator-1/{number}"))
        .collect::<Vec<_>>();
    let dispatchable_tasks = (1..=task_count)
        .map(|number| DispatchableTask {
            task_node_id: format!("I_task_{number}"),
            task_number: number as u64,
            repository_owner: "drasi-project".to_string(),
            repository_name: "demo".to_string(),
            assignment_comment_node_id: format!("IC_assignment_{number}"),
            worker_id: "validator-1".to_string(),
            task_type: "validate-issue".to_string(),
            queue_priority: number as i64,
            assignment_created_at: format!("2026-08-19T22:00:{number:02}Z"),
        })
        .collect::<Vec<_>>();
    CapacityRow {
        repository_owner: "drasi-project".to_string(),
        repository_name: "demo".to_string(),
        worker_id: "validator-1".to_string(),
        agent_profile: "issue-validator".to_string(),
        lease_duration_seconds: 900,
        configured_slot_count: slot_count,
        active_lease_count: 0,
        active_lease_ids: Vec::new(),
        free_slot_ids,
        dispatchable_task_ids: dispatchable_tasks
            .iter()
            .map(|task| task.task_node_id.clone())
            .collect(),
        dispatchable_tasks,
    }
}

fn event(sequence: u64, signature: u64, row: &CapacityRow) -> QueryResult {
    QueryResult::new(
        QUERY_ID.to_string(),
        sequence,
        DateTime::parse_from_rfc3339("2026-08-19T22:00:00Z")
            .unwrap()
            .with_timezone(&Utc),
        vec![ResultDiff::Add {
            data: serde_json::to_value(row).unwrap(),
            row_signature: signature,
        }],
        HashMap::new(),
    )
}

fn deleted_event(sequence: u64, signature: u64, row: &CapacityRow) -> QueryResult {
    QueryResult::new(
        QUERY_ID.to_string(),
        sequence,
        DateTime::parse_from_rfc3339("2026-08-19T22:00:00Z")
            .unwrap()
            .with_timezone(&Utc),
        vec![ResultDiff::Delete {
            data: serde_json::to_value(row).unwrap(),
            row_signature: signature,
        }],
        HashMap::new(),
    )
}

fn clock() -> Arc<dyn Clock> {
    Arc::new(FixedClock(
        DateTime::parse_from_rfc3339("2026-08-19T22:00:00Z")
            .unwrap()
            .with_timezone(&Utc),
    ))
}

fn ids(values: &[&str]) -> Arc<dyn LeaseIdGenerator> {
    Arc::new(FixedIds::new(values))
}

fn memory_store() -> Arc<dyn StateStoreProvider> {
    Arc::new(MemoryStateStoreProvider::new())
}

fn engine(
    store: Arc<dyn StateStoreProvider>,
    github: Arc<dyn GitHubApi>,
    ids: &[&str],
) -> DispatcherEngine {
    engine_with_config(store, github, ids, config())
}

fn engine_with_config(
    store: Arc<dyn StateStoreProvider>,
    github: Arc<dyn GitHubApi>,
    ids: &[&str],
    config: GitHubWorkGraphDispatcherConfig,
) -> DispatcherEngine {
    DispatcherEngine::new(
        REACTION_ID.to_string(),
        QUERY_ID.to_string(),
        config,
        store,
        github,
        clock(),
        self::ids(ids),
    )
}

async fn reservations(store: &Arc<dyn StateStoreProvider>) -> Vec<Reservation> {
    let mut records = Vec::new();
    for key in store.list_keys(REACTION_ID).await.unwrap() {
        if key.starts_with(RESERVATION_PREFIX) {
            let bytes = store.get(REACTION_ID, &key).await.unwrap().unwrap();
            records.push(serde_json::from_slice(&bytes).unwrap());
        }
    }
    records.sort_by(|left: &Reservation, right: &Reservation| left.lease_id.cmp(&right.lease_id));
    records
}

fn reservation(phase: ReservationPhase, lease_id: &str) -> Reservation {
    let lease = TaskLease {
        lease_id: lease_id.to_string(),
        assignment_comment_node_id: "IC_assignment_1".to_string(),
        worker_id: "validator-1".to_string(),
        slot_id: "validator-1/1".to_string(),
        acquired_at: "2026-08-19T22:00:00Z".to_string(),
        expires_at: "2026-08-19T22:15:00Z".to_string(),
    };
    let canonical_body = canonical_task_lease_body(&lease).unwrap();
    Reservation {
        schema_version: 1,
        lease_id: lease.lease_id,
        query_id: QUERY_ID.to_string(),
        worker_id: lease.worker_id,
        agent_profile: "issue-validator".to_string(),
        repository_owner: "drasi-project".to_string(),
        repository_name: "demo".to_string(),
        task_node_id: "I_task_1".to_string(),
        task_number: 1,
        assignment_comment_node_id: lease.assignment_comment_node_id,
        slot_id: lease.slot_id,
        task_type: "validate-issue".to_string(),
        acquired_at: lease.acquired_at,
        expires_at: lease.expires_at,
        body_digest: sha256_digest(&canonical_body),
        canonical_body,
        phase,
        attempt_count: if matches!(phase, ReservationPhase::Reserved) {
            0
        } else {
            1
        },
        last_error: None,
        origin_sequence: 1,
        origin_row_signature: 10,
        lease_comment_node_id: None,
        lease_comment_database_id: None,
    }
}

async fn put_reservation(store: &Arc<dyn StateStoreProvider>, reservation: &Reservation) {
    store
        .set(
            REACTION_ID,
            &reservation.key(),
            serde_json::to_vec(reservation).unwrap(),
        )
        .await
        .unwrap();
    store.sync().await.unwrap();
}

#[test]
fn config_and_builder_require_one_query_and_a_token() {
    let error = GitHubWorkGraphDispatcherBuilder::new("d")
        .with_token("token")
        .build()
        .err()
        .expect("missing query must fail");
    assert!(error.to_string().contains("exactly one"));

    let reaction = GitHubWorkGraphDispatcher::builder("d")
        .with_query(QUERY_ID)
        .with_token("token")
        .build()
        .unwrap();
    assert!(reaction.is_durable());
    assert!(reaction.needs_snapshot_on_fresh_start());
    assert_eq!(
        reaction.default_recovery_policy(),
        drasi_lib::recovery::ReactionRecoveryPolicy::Strict
    );

    let mut invalid = config();
    invalid
        .headers
        .insert("Authorization".to_string(), "other".to_string());
    assert!(invalid.validate(&[QUERY_ID.to_string()]).is_err());
    let mut credentialed_url = config();
    credentialed_url.api_url = "https://user:password@api.github.com".to_string();
    assert!(credentialed_url.validate(&[QUERY_ID.to_string()]).is_err());
    assert!(!format!("{:?}", config()).contains("test-token"));
    let serialized = serde_json::to_string(&config()).unwrap();
    assert!(!serialized.contains("test-token"));
    assert!(!serialized.contains("\"token\""));
    assert!(!serialized.contains("\"headers\""));
}

#[tokio::test]
async fn dynamic_descriptor_requires_a_named_secret_token() {
    let descriptor = GitHubWorkGraphDispatcherDescriptor;
    let schema = descriptor.config_schema_json();
    for definition in [
        "ConfigValueString",
        "ConfigValueU32",
        "ConfigValueU64",
        "reaction.github_workgraph_dispatcher.Config",
    ] {
        assert!(
            schema.contains(definition),
            "descriptor schema is missing {definition}"
        );
    }
    for invalid in [
        json!({ "token": "literal-token" }),
        json!({ "token": { "kind": "Secret", "name": "" } }),
        json!({ "token": { "kind": "EnvironmentVariable", "name": "TOKEN" } }),
    ] {
        let error = descriptor
            .create_reaction("dispatcher", vec![QUERY_ID.to_string()], &invalid, true)
            .await
            .err()
            .expect("non-Secret token configuration must be rejected");
        assert!(format!("{error:#}").contains("Secret"));
    }
}

#[tokio::test]
async fn inbox_failure_latches_ingest_until_restart() {
    let reaction = GitHubWorkGraphDispatcher::builder(REACTION_ID)
        .with_query(QUERY_ID)
        .with_token("token")
        .build()
        .unwrap();
    let (tx, _rx) = tokio::sync::mpsc::channel::<ComponentUpdate>(16);
    reaction
        .initialize(ReactionRuntimeContext::new(
            "instance",
            REACTION_ID,
            None,
            tx,
            None,
        ))
        .await;
    assert!(reaction
        .enqueue_query_result(deleted_event(1, 10, &row(1, 1)))
        .await
        .is_err());
    let second = reaction
        .enqueue_query_result(deleted_event(2, 11, &row(1, 1)))
        .await
        .expect_err("later input must not advance past a failed durable inbox write");
    assert!(format!("{second:#}").contains("fail-stopped"));
    assert_eq!(reaction.status().await, ComponentStatus::Error);
}

#[tokio::test]
async fn durable_inbox_is_recovered_before_volatile_queue_delivery() {
    let directory = TempDir::new().unwrap();
    let store: Arc<dyn StateStoreProvider> =
        Arc::new(RedbStateStoreProvider::new(directory.path().join("inbox.redb")).unwrap());

    let first = GitHubWorkGraphDispatcher::builder(REACTION_ID)
        .with_query(QUERY_ID)
        .with_token("token")
        .build()
        .unwrap();
    let (tx, _rx) = tokio::sync::mpsc::channel::<ComponentUpdate>(16);
    first
        .initialize(ReactionRuntimeContext::new(
            "instance",
            REACTION_ID,
            Some(Arc::clone(&store)),
            tx,
            None,
        ))
        .await;
    first
        .enqueue_query_result(deleted_event(1, 10, &row(1, 1)))
        .await
        .unwrap();
    assert_eq!(
        store
            .list_keys(REACTION_ID)
            .await
            .unwrap()
            .iter()
            .filter(|key| key.starts_with(INBOX_PREFIX))
            .count(),
        1
    );
    drop(first);

    let restarted = GitHubWorkGraphDispatcher::builder(REACTION_ID)
        .with_query(QUERY_ID)
        .with_token("token")
        .build()
        .unwrap();
    let (tx, _rx) = tokio::sync::mpsc::channel::<ComponentUpdate>(16);
    restarted
        .initialize(ReactionRuntimeContext::new(
            "instance",
            REACTION_ID,
            Some(Arc::clone(&store)),
            tx,
            None,
        ))
        .await;
    restarted.start().await.unwrap();
    assert!(store
        .list_keys(REACTION_ID)
        .await
        .unwrap()
        .iter()
        .all(|key| !key.starts_with(INBOX_PREFIX)));
    restarted.stop().await.unwrap();
}

#[tokio::test]
async fn fresh_bootstrap_processes_keyed_snapshot_and_suppresses_covered_events() {
    let directory = TempDir::new().unwrap();
    let store: Arc<dyn StateStoreProvider> =
        Arc::new(RedbStateStoreProvider::new(directory.path().join("bootstrap.redb")).unwrap());
    let reaction = GitHubWorkGraphDispatcher::builder(REACTION_ID)
        .with_query(QUERY_ID)
        .with_token("token")
        .build()
        .unwrap();
    let (tx, _rx) = tokio::sync::mpsc::channel::<ComponentUpdate>(16);
    reaction
        .initialize(ReactionRuntimeContext::new(
            "instance",
            REACTION_ID,
            Some(Arc::clone(&store)),
            tx,
            None,
        ))
        .await;
    reaction.start().await.unwrap();
    reaction
        .bootstrap(BootstrapContext::from_backend(
            QUERY_ID.to_string(),
            false,
            Box::new(SnapshotBackend {
                rows: vec![(10, serde_json::to_value(row(1, 0)).unwrap())],
                sequence: 5,
            }),
        ))
        .await
        .unwrap();

    reaction
        .enqueue_query_result(event(4, 11, &row(1, 1)))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert!(reservations(&store).await.is_empty());
    assert!(store
        .list_keys(REACTION_ID)
        .await
        .unwrap()
        .iter()
        .all(|key| !key.starts_with(INBOX_PREFIX)));
    reaction.stop().await.unwrap();
}

#[tokio::test]
async fn startup_requires_a_durable_state_store() {
    for store in [
        None,
        Some(Arc::new(MemoryStateStoreProvider::new()) as Arc<dyn StateStoreProvider>),
    ] {
        let reaction = GitHubWorkGraphDispatcher::builder(REACTION_ID)
            .with_query(QUERY_ID)
            .with_token("token")
            .build()
            .unwrap();
        let (tx, _rx) = tokio::sync::mpsc::channel::<ComponentUpdate>(16);
        reaction
            .initialize(ReactionRuntimeContext::new(
                "instance",
                REACTION_ID,
                store,
                tx,
                None,
            ))
            .await;
        let error = reaction
            .start()
            .await
            .expect_err("startup must reject missing or volatile state");
        assert!(format!("{error:#}").contains("durable state store"));
        assert_eq!(reaction.status().await, ComponentStatus::Error);
    }

    let directory = TempDir::new().unwrap();
    let store: Arc<dyn StateStoreProvider> =
        Arc::new(RedbStateStoreProvider::new(directory.path().join("startup.redb")).unwrap());
    let reaction = GitHubWorkGraphDispatcher::builder(REACTION_ID)
        .with_query(QUERY_ID)
        .with_token("token")
        .build()
        .unwrap();
    let (tx, _rx) = tokio::sync::mpsc::channel::<ComponentUpdate>(16);
    reaction
        .initialize(ReactionRuntimeContext::new(
            "instance",
            REACTION_ID,
            Some(store),
            tx,
            None,
        ))
        .await;
    reaction.start().await.unwrap();
    assert_eq!(reaction.status().await, ComponentStatus::Running);
    reaction.stop().await.unwrap();
}

#[test]
fn capacity_row_accepts_integer_floats_and_rejects_misalignment() {
    let value = json!({
        "repositoryOwner": "drasi-project",
        "repositoryName": "demo",
        "workerId": "validator-1",
        "agentProfile": "issue-validator",
        "leaseDurationSeconds": 900.0,
        "configuredSlotCount": 1.0,
        "activeLeaseCount": 0.0,
        "activeLeaseIds": [],
        "freeSlotIds": ["validator-1/1"],
        "dispatchableTaskIds": ["I_task_1"],
        "dispatchableTasks": [{
            "taskNodeId": "I_task_1",
            "taskNumber": 1.0,
            "repositoryOwner": "drasi-project",
            "repositoryName": "demo",
            "assignmentCommentNodeId": "IC_assignment_1",
            "workerId": "validator-1",
            "taskType": "validate-issue",
            "queuePriority": 1.0,
            "assignmentCreatedAt": "2026-08-19T22:00:01Z"
        }]
    });
    serde_json::from_value::<CapacityRow>(value)
        .unwrap()
        .validate()
        .unwrap();

    let mut misaligned = row(1, 1);
    misaligned.dispatchable_task_ids[0] = "other".to_string();
    assert!(misaligned.validate().is_err());
    let mut mismatched_count = row(1, 1);
    mismatched_count.active_lease_count = 1;
    assert!(mismatched_count.validate().is_err());
    let mut duplicate_task_number = row(2, 2);
    duplicate_task_number.dispatchable_tasks[1].task_number = 1;
    assert!(duplicate_task_number.validate().is_err());
    let mut duplicate_assignment = row(2, 2);
    duplicate_assignment.dispatchable_tasks[1].assignment_comment_node_id =
        "IC_assignment_1".to_string();
    assert!(duplicate_assignment.validate().is_err());
    let mut incompatible_profile = row(1, 1);
    incompatible_profile.agent_profile = "issue-info-requester".to_string();
    assert!(incompatible_profile.validate().is_err());
}

#[tokio::test]
async fn fills_multiple_slots_and_overlays_pending_reservations() {
    let store = memory_store();
    let github = FakeGitHub::with_behavior(vec![FakePost::Accept, FakePost::Accept]).await;
    let mut engine = engine(Arc::clone(&store), github.clone(), &["lease-1", "lease-2"]);
    engine.process(&event(1, 10, &row(2, 2))).await.unwrap();
    assert_eq!(github.post_bodies().await.len(), 2);
    let records = reservations(&store).await;
    assert_eq!(records.len(), 2);
    assert!(records
        .iter()
        .all(|record| record.phase == ReservationPhase::AwaitingProjection));

    engine.process(&event(2, 11, &row(2, 2))).await.unwrap();
    assert_eq!(github.post_bodies().await.len(), 2);
}

#[tokio::test]
async fn one_result_cannot_double_book_a_global_worker_across_repositories() {
    let store = memory_store();
    let github = FakeGitHub::with_behavior(Vec::new()).await;
    let mut engine = engine(Arc::clone(&store), github.clone(), &["unused"]);
    let first = row(1, 1);
    let mut second = row(1, 1);
    second.repository_name = "demo-two".to_string();
    second.dispatchable_tasks[0].repository_name = "demo-two".to_string();
    let event = QueryResult::new(
        QUERY_ID.to_string(),
        1,
        Utc::now(),
        vec![
            ResultDiff::Add {
                data: serde_json::to_value(first).unwrap(),
                row_signature: 10,
            },
            ResultDiff::Add {
                data: serde_json::to_value(second).unwrap(),
                row_signature: 11,
            },
        ],
        HashMap::new(),
    );
    assert!(engine.process(&event).await.is_err());
    assert!(github.post_bodies().await.is_empty());
    assert!(reservations(&store).await.is_empty());
}

#[tokio::test]
async fn duplicate_generated_lease_id_cannot_overwrite_a_durable_reservation() {
    let store = memory_store();
    let github = FakeGitHub::with_behavior(vec![FakePost::Accept]).await;
    let mut engine = engine(Arc::clone(&store), github.clone(), &["lease-1", "lease-1"]);
    engine.process(&event(1, 10, &row(1, 1))).await.unwrap();
    let original = reservations(&store).await[0].clone();

    let error = engine
        .process(&event(2, 11, &row(2, 2)))
        .await
        .expect_err("duplicate generated lease ID must fail before persistence");
    assert!(format!("{error:#}").contains("duplicate identifier"));
    assert_eq!(reservations(&store).await, vec![original]);
    assert_eq!(github.post_bodies().await.len(), 1);
}

#[tokio::test]
async fn result_confirmation_releases_one_slot_and_dispatches_next_task() {
    let store = memory_store();
    let github = FakeGitHub::with_behavior(vec![FakePost::Accept, FakePost::Accept]).await;
    let mut engine = engine(Arc::clone(&store), github.clone(), &["lease-1", "lease-2"]);
    engine.process(&event(1, 10, &row(1, 2))).await.unwrap();
    assert_eq!(github.post_bodies().await.len(), 1);

    engine.process(&event(2, 11, &row(1, 2))).await.unwrap();
    assert_eq!(github.post_bodies().await.len(), 1);

    let mut next = row(1, 1);
    next.dispatchable_tasks[0] = row(1, 2).dispatchable_tasks[1].clone();
    next.dispatchable_task_ids[0] = next.dispatchable_tasks[0].task_node_id.clone();
    let mut confirmed = next.clone();
    confirmed.active_lease_count = 1;
    confirmed.active_lease_ids = vec!["lease-1".to_string()];
    confirmed.free_slot_ids.clear();
    engine.process(&event(3, 12, &confirmed)).await.unwrap();
    assert!(reservations(&store).await.is_empty());

    engine.process(&event(4, 13, &next)).await.unwrap();
    assert_eq!(github.post_bodies().await.len(), 2);
}

#[tokio::test]
async fn ambiguous_success_reconciles_without_duplicate_post() {
    let store = memory_store();
    let github = FakeGitHub::with_behavior(vec![FakePost::AmbiguousWrite]).await;
    let mut engine = engine(Arc::clone(&store), github.clone(), &["lease-1"]);
    engine.process(&event(1, 10, &row(1, 1))).await.unwrap();
    assert_eq!(github.post_bodies().await.len(), 1);
    assert_eq!(github.comments().await.len(), 1);
    assert_eq!(
        reservations(&store).await[0].phase,
        ReservationPhase::AwaitingProjection
    );
}

#[tokio::test]
async fn proven_absent_write_retries_same_lease_and_body() {
    let store = memory_store();
    let github = FakeGitHub::with_behavior(vec![FakePost::AmbiguousAbsent, FakePost::Accept]).await;
    let mut engine = engine(Arc::clone(&store), github.clone(), &["lease-1"]);
    engine.process(&event(1, 10, &row(1, 1))).await.unwrap();
    let bodies = github.post_bodies().await;
    assert_eq!(bodies.len(), 2);
    assert_eq!(bodies[0], bodies[1]);
    assert!(bodies[0].contains("\"leaseId\": \"lease-1\""));
}

#[tokio::test]
async fn conflicting_or_duplicate_remote_lease_fails_closed() {
    let store = memory_store();
    let github = FakeGitHub::with_behavior(vec![FakePost::AmbiguousAbsent]).await;
    let canonical = reservation(ReservationPhase::WriteInFlight, "lease-1").canonical_body;
    github.add_comment(canonical.clone()).await;
    github.add_comment(canonical).await;
    let record = reservation(ReservationPhase::WriteInFlight, "lease-1");
    put_reservation(&store, &record).await;
    let mut engine = engine(Arc::clone(&store), github, &["unused"]);
    assert!(engine.recover().await.is_err());
    assert_eq!(
        reservations(&store).await[0].phase,
        ReservationPhase::ReconcileRequired
    );
}

#[tokio::test]
async fn conflicting_remote_lease_body_fails_closed() {
    let store = memory_store();
    let github = FakeGitHub::with_behavior(Vec::new()).await;
    let record = reservation(ReservationPhase::WriteInFlight, "lease-1");
    let mut conflicting_lease = record.task_lease();
    conflicting_lease.slot_id = "validator-1/2".to_string();
    github
        .add_comment(canonical_task_lease_body(&conflicting_lease).unwrap())
        .await;
    put_reservation(&store, &record).await;
    let mut engine = engine(Arc::clone(&store), github, &["unused"]);
    assert!(engine.recover().await.is_err());
    assert_eq!(
        reservations(&store).await[0].phase,
        ReservationPhase::ReconcileRequired
    );
}

#[tokio::test]
async fn exhausted_absent_retries_retain_a_reconciliation_record() {
    let store = memory_store();
    let github = FakeGitHub::with_behavior(vec![FakePost::AmbiguousAbsent; 4]).await;
    let mut engine = engine(Arc::clone(&store), github.clone(), &["lease-1"]);
    assert!(engine.process(&event(1, 10, &row(1, 1))).await.is_err());
    let records = reservations(&store).await;
    assert_eq!(records[0].phase, ReservationPhase::ReconcileRequired);
    assert_eq!(records[0].attempt_count, 4);
    assert_eq!(github.post_bodies().await.len(), 4);
}

#[tokio::test]
async fn awaiting_projection_comment_disappearance_fails_closed() {
    let store = memory_store();
    let github = FakeGitHub::with_behavior(Vec::new()).await;
    put_reservation(
        &store,
        &reservation(ReservationPhase::AwaitingProjection, "lease-1"),
    )
    .await;
    let mut engine = engine(Arc::clone(&store), github, &["unused"]);
    assert!(engine.recover().await.is_err());
    assert_eq!(
        reservations(&store).await[0].phase,
        ReservationPhase::ReconcileRequired
    );
}

#[tokio::test]
async fn missing_slot_or_task_without_exact_confirmation_fails_closed() {
    let store = memory_store();
    let github = FakeGitHub::with_behavior(vec![FakePost::Accept]).await;
    let mut engine = engine(Arc::clone(&store), github, &["lease-1"]);
    engine.process(&event(1, 10, &row(1, 1))).await.unwrap();

    let mut changed = row(1, 0);
    changed.free_slot_ids = vec!["validator-1/1".to_string()];
    assert!(engine.process(&event(2, 11, &changed)).await.is_err());
    assert_eq!(
        reservations(&store).await[0].phase,
        ReservationPhase::ReconcileRequired
    );
}

#[tokio::test]
async fn exact_active_id_cannot_confirm_while_its_task_or_slot_is_still_available() {
    let store = memory_store();
    let github = FakeGitHub::with_behavior(vec![FakePost::Accept]).await;
    let mut engine = engine(Arc::clone(&store), github, &["lease-1"]);
    engine.process(&event(1, 10, &row(1, 1))).await.unwrap();

    let mut contradictory = row(1, 1);
    contradictory.active_lease_count = 1;
    contradictory.active_lease_ids = vec!["lease-1".to_string()];
    assert!(engine.process(&event(2, 11, &contradictory)).await.is_err());
    assert_eq!(
        reservations(&store).await[0].phase,
        ReservationPhase::ReconcileRequired
    );
}

#[tokio::test]
async fn delete_rows_never_dispatch_and_replays_do_not_duplicate() {
    let store = memory_store();
    let github = FakeGitHub::with_behavior(vec![FakePost::Accept]).await;
    let mut engine = engine(Arc::clone(&store), github.clone(), &["lease-1"]);
    engine
        .process(&deleted_event(1, 10, &row(1, 1)))
        .await
        .unwrap();
    assert!(github.post_bodies().await.is_empty());

    engine.process(&event(2, 11, &row(1, 1))).await.unwrap();
    engine.process(&event(2, 11, &row(1, 1))).await.unwrap();
    engine.process(&event(1, 10, &row(1, 1))).await.unwrap();
    assert_eq!(github.post_bodies().await.len(), 1);
    assert!(engine.process(&event(2, 99, &row(1, 1))).await.is_err());
}

#[tokio::test]
async fn validates_every_row_before_mutating_any_state() {
    let store = memory_store();
    let github = FakeGitHub::with_behavior(vec![FakePost::Accept]).await;
    let mut engine = engine(Arc::clone(&store), github.clone(), &["lease-1"]);
    let valid = row(1, 1);
    let mut invalid = row(1, 1);
    invalid.active_lease_count = 1;
    let event = QueryResult::new(
        QUERY_ID.to_string(),
        1,
        Utc::now(),
        vec![
            ResultDiff::Add {
                data: serde_json::to_value(valid).unwrap(),
                row_signature: 1,
            },
            ResultDiff::Add {
                data: serde_json::to_value(invalid).unwrap(),
                row_signature: 2,
            },
        ],
        HashMap::new(),
    );
    assert!(engine.process(&event).await.is_err());
    assert!(github.post_bodies().await.is_empty());
    assert!(reservations(&store).await.is_empty());
}

#[tokio::test]
async fn restart_recovers_each_nonterminal_phase() {
    for phase in [
        ReservationPhase::Reserved,
        ReservationPhase::WriteInFlight,
        ReservationPhase::AwaitingProjection,
        ReservationPhase::Confirmed,
    ] {
        let store = memory_store();
        let github = FakeGitHub::with_behavior(vec![FakePost::Accept]).await;
        let record = reservation(phase, "lease-1");
        if matches!(
            phase,
            ReservationPhase::WriteInFlight | ReservationPhase::AwaitingProjection
        ) {
            github.add_comment(record.canonical_body.clone()).await;
        }
        put_reservation(&store, &record).await;
        let mut engine = engine(Arc::clone(&store), github.clone(), &["unused"]);
        engine.recover().await.unwrap();
        let records = reservations(&store).await;
        if phase == ReservationPhase::Confirmed {
            assert!(records.is_empty());
        } else {
            assert_eq!(records[0].phase, ReservationPhase::AwaitingProjection);
        }
        assert_eq!(
            github.post_bodies().await.len(),
            usize::from(phase == ReservationPhase::Reserved)
        );
    }

    let store = memory_store();
    let github = FakeGitHub::with_behavior(Vec::new()).await;
    put_reservation(
        &store,
        &reservation(ReservationPhase::ReconcileRequired, "lease-1"),
    )
    .await;
    assert!(engine(store, github, &["unused"]).recover().await.is_err());
}

#[tokio::test]
async fn exact_active_lease_allows_capacity_reduction_cleanup() {
    let store = memory_store();
    let github = FakeGitHub::with_behavior(vec![FakePost::Accept]).await;
    let mut engine = engine(Arc::clone(&store), github, &["lease-1"]);
    engine.process(&event(1, 10, &row(1, 1))).await.unwrap();
    let mut reduced = row(1, 0);
    reduced.free_slot_ids.clear();
    reduced.active_lease_count = 1;
    reduced.active_lease_ids = vec!["lease-1".to_string()];
    engine.process(&event(2, 11, &reduced)).await.unwrap();
    assert!(reservations(&store).await.is_empty());
}

#[tokio::test]
async fn redb_restart_retains_overlay_and_prevents_duplicate_lease() {
    let directory = TempDir::new().unwrap();
    let path = directory.path().join("state.redb");
    let store: Arc<dyn StateStoreProvider> = Arc::new(RedbStateStoreProvider::new(&path).unwrap());
    let github = FakeGitHub::with_behavior(vec![FakePost::Accept]).await;
    let mut first = engine(Arc::clone(&store), github.clone(), &["lease-1"]);
    first.process(&event(1, 10, &row(1, 1))).await.unwrap();
    drop(first);

    let mut restarted = engine(Arc::clone(&store), github.clone(), &["unused"]);
    restarted.recover().await.unwrap();
    restarted.process(&event(2, 11, &row(1, 1))).await.unwrap();
    assert_eq!(github.post_bodies().await.len(), 1);
    assert_eq!(reservations(&store).await.len(), 1);
}

#[tokio::test]
async fn bootstrap_watermark_suppresses_all_superseded_buffered_rows() {
    let store = memory_store();
    let github = FakeGitHub::with_behavior(vec![FakePost::Accept]).await;
    let mut engine = engine(Arc::clone(&store), github.clone(), &["lease-1"]);
    engine.recover().await.unwrap();
    engine.persist_bootstrap_watermark(10).await.unwrap();
    engine.process(&event(9, 90, &row(1, 1))).await.unwrap();
    assert!(github.post_bodies().await.is_empty());
    assert!(reservations(&store).await.is_empty());

    engine.process(&event(11, 110, &row(1, 1))).await.unwrap();
    assert_eq!(github.post_bodies().await.len(), 1);
}

#[tokio::test]
async fn durable_state_rejects_a_changed_github_api_target_before_recovery() {
    let store = memory_store();
    let github = FakeGitHub::with_behavior(Vec::new()).await;
    engine(Arc::clone(&store), github.clone(), &["unused"])
        .recover()
        .await
        .unwrap();

    let mut changed = config();
    changed.api_url = "https://github.example/api/v3".to_string();
    let error = engine_with_config(store, github, &["unused"], changed)
        .recover()
        .await
        .expect_err("durable state must not move between GitHub API targets");
    assert!(format!("{error:#}").contains("bound to"));
}

#[tokio::test]
async fn definitive_http_rejection_is_durable_fail_stop() {
    let store = memory_store();
    let github = FakeGitHub::with_behavior(vec![FakePost::Reject]).await;
    let mut engine = engine(Arc::clone(&store), github, &["lease-1"]);
    assert!(engine.process(&event(1, 10, &row(1, 1))).await.is_err());
    let records = reservations(&store).await;
    assert_eq!(records[0].phase, ReservationPhase::ReconcileRequired);
    assert_eq!(records[0].attempt_count, 1);
}

#[tokio::test]
async fn rest_client_writes_exact_body_and_required_headers() {
    let server = MockServer::start().await;
    let body = reservation(ReservationPhase::Reserved, "lease-1").canonical_body;
    Mock::given(method("POST"))
        .and(path("/repos/drasi-project/demo/issues/1/comments"))
        .and(header("authorization", "Bearer test-token"))
        .and(header("accept", "application/vnd.github+json"))
        .and(header("x-github-api-version", "2022-11-28"))
        .and(body_json(json!({ "body": body })))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": 1,
            "node_id": "IC_1",
            "body": body
        })))
        .expect(1)
        .mount(&server)
        .await;
    let mut rate_limited_config = config();
    rate_limited_config.api_url = server.uri();
    let client = RestGitHubApi::new(&rate_limited_config).unwrap();
    assert!(matches!(
        client.post_comment("drasi-project", "demo", 1, &body).await,
        PostDisposition::Accepted(_)
    ));
}

#[tokio::test]
async fn rest_client_treats_bad_success_json_and_server_errors_as_ambiguous() {
    for (status, response) in [
        (201, ResponseTemplate::new(201).set_body_string("not-json")),
        (503, ResponseTemplate::new(503)),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(response)
            .mount(&server)
            .await;
        let mut forbidden_config = config();
        forbidden_config.api_url = server.uri();
        let client = RestGitHubApi::new(&forbidden_config).unwrap();
        let disposition = client
            .post_comment("drasi-project", "demo", status, "body")
            .await;
        assert!(matches!(disposition, PostDisposition::Ambiguous { .. }));
    }
}

#[tokio::test]
async fn rest_client_retries_only_rate_limited_forbidden_responses() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/repos/drasi-project/demo/issues/1/comments"))
        .respond_with(ResponseTemplate::new(403).insert_header("Retry-After", "7"))
        .expect(1)
        .mount(&server)
        .await;
    let mut rate_limited_config = config();
    rate_limited_config.api_url = server.uri();
    let client = RestGitHubApi::new(&rate_limited_config).unwrap();
    assert!(matches!(
        client
            .post_comment("drasi-project", "demo", 1, "body")
            .await,
        PostDisposition::Ambiguous {
            retry_after: Some(delay),
            ..
        } if delay == std::time::Duration::from_secs(7)
    ));

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(403)
                .set_body_json(json!({ "message": "Resource not accessible by integration" })),
        )
        .expect(1)
        .mount(&server)
        .await;
    let mut forbidden_config = config();
    forbidden_config.api_url = server.uri();
    let client = RestGitHubApi::new(&forbidden_config).unwrap();
    assert!(matches!(
        client
            .post_comment("drasi-project", "demo", 1, "body")
            .await,
        PostDisposition::Rejected(_)
    ));
}
