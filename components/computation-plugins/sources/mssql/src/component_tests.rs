// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Lifecycle, pause, stop, reconnect, erase and management against the
//! in-memory CDC database. Every wait is bounded so a regression fails
//! instead of hanging.

use super::*;
use drasi_computation_contracts::{
    lifecycle::LifecycleErrorKind, ConsumerProgressProvider, ConsumerProgressSnapshot,
    ConsumerProgressUpdates, SourceProgressSnapshot,
};
use drasi_computation_plugin_sdk::lifecycle::{DisposeReason, OperationScope, StateIntent};
use drasi_core_types::interface::SourceCheckpoint;
use drasi_mssql_common::native::{
    memory::{lsn, MemoryDatabase},
    Cursor,
};
use serde_json::json;
use std::collections::BTreeMap;

const SOURCE_ID: &str = "585da1dd-2d7d-5cec-bcf7-e84ac13a88f8";
const CONSUMER_A: &str = "18f30619-401b-516d-805d-3b849c066b31";
const CONSUMER_B: &str = "030b9047-5dd4-5f27-8bec-8f5d02ca14a9";
const STEP: Duration = Duration::from_secs(10);

async fn step<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(STEP, future)
        .await
        .expect("test step timed out")
}

struct Progress {
    id: ComponentId,
    state: watch::Sender<ConsumerProgressSnapshot>,
}

struct Updates(watch::Receiver<ConsumerProgressSnapshot>);

#[async_trait]
impl ConsumerProgressUpdates for Updates {
    fn snapshot(&mut self) -> Result<ConsumerProgressSnapshot> {
        Ok(self.0.borrow_and_update().clone())
    }
    async fn changed(&mut self) -> Result<()> {
        Ok(self.0.changed().await?)
    }
}

impl ConsumerProgressProvider for Progress {
    fn graph_id(&self) -> &str {
        "graph"
    }
    fn producer_id(&self) -> &ComponentId {
        &self.id
    }
    fn snapshot(&self) -> Result<ConsumerProgressSnapshot> {
        Ok(self.state.borrow().clone())
    }
    fn subscribe(&self) -> Result<Box<dyn ConsumerProgressUpdates>> {
        Ok(Box::new(Updates(self.state.subscribe())))
    }
}

/// Consumer states: `(ready, committed cursor)` for A and B.
fn snapshot(members: [(bool, Option<Cursor>); 2]) -> ConsumerProgressSnapshot {
    let mut consumers = BTreeMap::new();
    for ((ready, cursor), id) in members.into_iter().zip([CONSUMER_A, CONSUMER_B]) {
        let mut state = SourceProgressSnapshot {
            ready,
            ..Default::default()
        };
        if let Some(cursor) = cursor {
            state.checkpoints.insert(
                SourceProgressKey::Source(ComponentId::try_new(SOURCE_ID).unwrap()),
                SourceCheckpoint::new(5, Some(cursor.encode().unwrap().into())),
            );
        }
        consumers.insert(ComponentId::try_new(id).unwrap(), Arc::new(state));
    }
    ConsumerProgressSnapshot { consumers }
}

struct Fixture {
    db: MemoryDatabase,
    progress: Arc<Progress>,
    source: MsSqlSource,
}

fn config(extra: serde_json::Value) -> SourceConfig {
    let mut value = json!({
        "stream": "items/out", "database": "app", "user": "drasi", "password": "p",
        "pollIntervalMs": 5, "maxRows": 2, "reconnectBudgetMs": 5000
    });
    for (key, field) in extra.as_object().unwrap() {
        value[key] = field.clone();
    }
    SourceConfig::from_value(&value).unwrap()
}

fn connectors(db: &MemoryDatabase) -> ConnectorFactory {
    let db = db.clone();
    Arc::new(move |_, _| Arc::new(db.clone()) as Arc<dyn CdcConnector>)
}

fn fixture_with(extra: serde_json::Value, identity_bound: bool) -> Fixture {
    let db = MemoryDatabase::new();
    db.add_table("Items", 7);
    let id = ComponentId::try_new(SOURCE_ID).unwrap();
    let progress = Arc::new(Progress {
        id: id.clone(),
        state: watch::channel(snapshot([(true, None), (true, None)])).0,
    });
    let source = MsSqlSource::new(
        id,
        config(extra),
        ConsumerProgressView::new(progress.clone()),
        &[
            ComponentId::try_new(CONSUMER_A).unwrap(),
            ComponentId::try_new(CONSUMER_B).unwrap(),
        ],
        identity_bound,
        connectors(&db),
    )
    .unwrap();
    Fixture {
        db,
        progress,
        source,
    }
}

fn fixture() -> Fixture {
    fixture_with(json!({}), false)
}

fn scope(deadline: Option<Duration>) -> OperationScope {
    OperationScope::new(
        "test",
        "component",
        1,
        deadline.map(|d| Instant::now() + d),
        CancellationToken::new(),
    )
}
fn start_ctx() -> StartContext {
    StartContext::new(scope(Some(STEP)), None)
}
fn pause_ctx(deadline: Duration) -> PauseContext {
    PauseContext::new(scope(Some(deadline)))
}
fn resume_ctx() -> ResumeContext {
    ResumeContext::new(scope(None))
}
fn stop_ctx(mode: StopMode, deadline: Duration) -> StopContext {
    StopContext::new(scope(Some(deadline)), mode)
}
fn dispose_ctx(intent: StateIntent, attempt: u32) -> DisposeContext {
    DisposeContext::new(scope(Some(STEP)), DisposeReason::Removal, intent, attempt).unwrap()
}

fn element_id(envelope: &OutputEnvelope) -> String {
    format!("{:?}", envelope.envelope.changes().operations()[0])
}

async fn next(source: &mut MsSqlSource) -> OutputEnvelope {
    step(source.next()).await.unwrap().expect("an envelope")
}

/// The CDC cursor of a progress-only record.
fn progress_cursor(envelope: &OutputEnvelope) -> Cursor {
    assert!(
        envelope.envelope.changes().operations().is_empty(),
        "expected a progress record"
    );
    Cursor::decode(
        envelope
            .envelope
            .system()
            .source_position()
            .expect("record position"),
    )
    .unwrap()
}

/// Commits `cursors` for A and B, as their queries would.
fn commit(f: &Fixture, cursors: [Option<Cursor>; 2]) {
    let [a, b] = cursors;
    f.progress
        .state
        .send_replace(snapshot([(true, a), (true, b)]));
}

/// Starts a fresh group and commits its origin record for both members, as
/// their queries would; changes flow only after that.
async fn start_streaming(f: &mut Fixture) -> Cursor {
    step(f.source.start(&mut start_ctx())).await.unwrap();
    let origin = progress_cursor(&next(&mut f.source).await);
    commit(f, [Some(origin.clone()), Some(origin.clone())]);
    origin
}

async fn quiet(source: &mut MsSqlSource) -> bool {
    tokio::time::timeout(Duration::from_millis(100), source.next())
        .await
        .is_err()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streams_every_change_once_across_pages_and_restarts_with_one_connection() {
    let mut f = fixture();
    let _origin = start_streaming(&mut f).await;
    assert!(
        step(f.source.start(&mut start_ctx())).await.is_err(),
        "second start refused"
    );
    for n in 1..=5 {
        f.db.change(7, 10 + n, 1, 2, n as i64, 0);
    }
    let mut seen = Vec::new();
    for _ in 0..5 {
        seen.push(element_id(&next(&mut f.source).await));
    }
    seen.dedup();
    assert_eq!(seen.len(), 5);
    assert_eq!(
        f.db.with(|s| s.connects),
        1,
        "one connection per activation"
    );
    let sequence = f.source.sequence();
    step(f.source.stop(&stop_ctx(StopMode::Drain, STEP)))
        .await
        .unwrap();
    assert_eq!(f.db.with(|s| (s.open_sessions, s.closed_sessions)), (0, 1));
    // Restart from the consumers' committed cursor at LSN 13: replay 14, 15.
    let committed = Cursor::boundary(binding(&f).await, lsn(13));
    f.progress.state.send_replace(snapshot([
        (true, Some(committed.clone())),
        (true, Some(committed)),
    ]));
    step(f.source.start(&mut start_ctx())).await.unwrap();
    next(&mut f.source).await;
    next(&mut f.source).await;
    assert!(
        quiet(&mut f.source).await,
        "nothing before the committed cursor replays"
    );
    assert!(
        f.source.sequence() > sequence,
        "sequences never repeat across activations"
    );
    step(f.source.stop(&stop_ctx(StopMode::Abort, STEP)))
        .await
        .unwrap();
}

async fn binding(f: &Fixture) -> [u8; 32] {
    let mut session = f.db.connect().await.unwrap();
    let settings = f.source.settings().unwrap();
    Poller::open(session.as_mut(), &settings, SOURCE_ID)
        .await
        .unwrap()
        .catalog
        .binding
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pause_waits_for_the_in_flight_page_then_reads_nothing_until_resume() {
    let mut f = fixture();
    let _origin = start_streaming(&mut f).await;
    f.db.block();
    step(f.db.wait_blocked()).await;
    // The in-flight key read cannot finish before this deadline: the pause
    // is withdrawn and the source keeps running.
    let error = step(f.source.pause(&pause_ctx(Duration::from_millis(50))))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), LifecycleErrorKind::Retryable, "{error:#}");
    assert!(
        !f.source.pause.is_pause_requested(),
        "an expired pause is withdrawn"
    );
    f.db.release();
    step(f.source.pause(&pause_ctx(STEP))).await.unwrap();
    let queries = f.db.with(|s| s.key_queries);
    f.db.change(7, 20, 1, 2, 1, 1);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        f.db.with(|s| s.key_queries),
        queries,
        "no query runs while paused"
    );
    assert!(
        quiet(&mut f.source).await,
        "nothing is emitted while paused"
    );
    step(f.source.resume(&resume_ctx())).await.unwrap();
    next(&mut f.source).await;
    step(f.source.stop(&stop_ctx(StopMode::Drain, STEP)))
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drain_stop_times_out_while_a_query_is_in_flight_and_a_retry_completes_it() {
    let mut f = fixture();
    let _origin = start_streaming(&mut f).await;
    f.db.block();
    step(f.db.wait_blocked()).await;
    let error = step(
        f.source
            .stop(&stop_ctx(StopMode::Drain, Duration::from_millis(50))),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), LifecycleErrorKind::Retryable);
    assert!(
        f.source.worker.get_mut().is_some(),
        "the worker stays owned"
    );
    f.db.release();
    step(f.source.stop(&stop_ctx(StopMode::Drain, STEP)))
        .await
        .unwrap();
    assert!(f.source.worker.get_mut().is_none());
    assert_eq!(f.db.with(|s| s.open_sessions), 0);
}

#[tokio::test]
async fn abort_stop_drops_the_connection_promptly_even_mid_query() {
    let mut f = fixture();
    let _origin = start_streaming(&mut f).await;
    f.db.block();
    step(f.db.wait_blocked()).await;
    step(f.source.stop(&stop_ctx(StopMode::Abort, STEP)))
        .await
        .unwrap();
    assert_eq!(f.db.with(|s| s.open_sessions), 0, "Abort drops the session");
    f.db.release();
    // The object starts again after Abort.
    step(f.source.start(&mut start_ctx())).await.unwrap();
    step(f.source.stop(&stop_ctx(StopMode::Abort, STEP)))
        .await
        .unwrap();
}

#[tokio::test]
async fn cancelled_and_expired_starts_leave_nothing_running() {
    let mut f = fixture();
    let cancelled = scope(Some(STEP));
    cancelled.cancellation().cancel();
    let error = step(f.source.start(&mut StartContext::new(cancelled, None)))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), LifecycleErrorKind::Cancelled);
    assert_eq!(f.db.with(|s| s.connects), 0);
    // Consumers that never become ready: the start deadline expires.
    f.progress
        .state
        .send_replace(snapshot([(false, None), (true, None)]));
    let error = step(f.source.start(&mut StartContext::new(
        scope(Some(Duration::from_millis(50))),
        None,
    )))
    .await
    .unwrap_err();
    assert_eq!(error.kind(), LifecycleErrorKind::Retryable);
    assert!(f.source.worker.get_mut().is_none());
    assert!(
        step(f.source.start(&mut start_ctx())).await.is_err(),
        "stop first"
    );
    step(f.source.stop(&stop_ctx(StopMode::Abort, STEP)))
        .await
        .unwrap();
    f.progress
        .state
        .send_replace(snapshot([(true, None), (true, None)]));
    step(f.source.start(&mut start_ctx())).await.unwrap();
    step(f.source.stop(&stop_ctx(StopMode::Drain, STEP)))
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transport_failures_reconnect_without_loss_and_permanent_failures_do_not() {
    let mut f = fixture();
    let _origin = start_streaming(&mut f).await;
    f.db.change(7, 11, 1, 2, 1, 0);
    next(&mut f.source).await;
    f.db.with(|s| {
        s.failing_queries = 2;
        s.failing_connects = 1;
    });
    f.db.change(7, 12, 1, 2, 2, 0);
    f.db.change(7, 13, 1, 2, 3, 0);
    let second = element_id(&next(&mut f.source).await);
    let third = element_id(&next(&mut f.source).await);
    assert_ne!(second, third);
    assert!(
        quiet(&mut f.source).await,
        "no duplicate after reconnecting"
    );
    assert!(f.db.with(|s| s.connects) >= 2, "the session was replaced");
    // A capture-instance change is typed and never reconnects.
    let connects = f.db.with(|s| s.connects);
    f.db.with(|s| {
        s.tables[0].capture.capture = "dbo_Items_v2".into();
        s.fingerprint += 1;
    });
    let error = step(f.source.next()).await.unwrap_err();
    assert!(
        matches!(
            drasi_computation_contracts::source_failure(&error),
            Some(drasi_computation_contracts::SourceFailure::SchemaChanged(_))
        ),
        "{error:#}"
    );
    assert_eq!(f.db.with(|s| s.connects), connects);
    step(f.source.stop(&stop_ctx(StopMode::Drain, STEP)))
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_zero_reconnect_budget_reports_the_transport_failure() {
    let mut f = fixture_with(json!({"reconnectBudgetMs": 0}), false);
    let _origin = start_streaming(&mut f).await;
    f.db.with(|s| s.failing_queries = 1);
    let error = step(f.source.next()).await.unwrap_err();
    assert!(
        format!("{error:#}").contains("reconnectBudgetMs is 0"),
        "{error:#}"
    );
    step(f.source.stop(&stop_ctx(StopMode::Drain, STEP)))
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restarting_consumer_holds_polling_instead_of_failing_the_source() {
    let mut f = fixture();
    let origin = start_streaming(&mut f).await;
    f.progress.state.send_replace(snapshot([
        (true, Some(origin.clone())),
        (false, Some(origin.clone())),
    ]));
    tokio::time::sleep(Duration::from_millis(30)).await;
    f.db.change(7, 20, 1, 2, 1, 0);
    assert!(
        quiet(&mut f.source).await,
        "held while a member is not ready"
    );
    f.progress.state.send_replace(snapshot([
        (true, Some(origin.clone())),
        (true, Some(origin)),
    ]));
    next(&mut f.source).await;
    step(f.source.stop(&stop_ctx(StopMode::Drain, STEP)))
        .await
        .unwrap();
}

#[tokio::test]
async fn erase_removes_only_this_sources_orphan_markers_and_is_idempotent() {
    let mut f = fixture_with(json!({"barrierTable": "dbo.DrasiBarrier"}), false);
    let mine = markers::marker_prefix("graph", SOURCE_ID);
    let other = markers::marker_prefix("graph", CONSUMER_A);
    f.db.with(|s| {
        s.markers = Some(vec![
            markers::new_marker(&mine).unwrap(),
            markers::new_marker(&mine).unwrap().to_lowercase(),
            markers::new_marker(&other).unwrap(),
        ])
    });
    step(f.source.dispose(&dispose_ctx(StateIntent::Preserve, 1)))
        .await
        .unwrap();
    assert_eq!(f.db.with(|s| s.markers.as_ref().unwrap().len()), 3);
    for attempt in [1, 2] {
        step(f.source.dispose(&dispose_ctx(StateIntent::Erase, attempt)))
            .await
            .unwrap();
    }
    let left = f.db.with(|s| s.markers.clone().unwrap());
    assert_eq!(left.len(), 1);
    assert!(markers::owns(&other, &left[0]));
    // Without a barrier table the source owns nothing and connects nowhere.
    let mut plain = fixture();
    step(plain.source.dispose(&dispose_ctx(StateIntent::Erase, 1)))
        .await
        .unwrap();
    assert_eq!(plain.db.with(|s| s.connects), 0);
    // Dispose requires a stopped source.
    step(plain.source.start(&mut start_ctx())).await.unwrap();
    assert!(
        step(plain.source.dispose(&dispose_ctx(StateIntent::Erase, 1)))
            .await
            .is_err()
    );
    step(plain.source.stop(&stop_ctx(StopMode::Drain, STEP)))
        .await
        .unwrap();
}

/// MS-22: a provider-bound source reconstructed after a restart has never
/// started, so it holds no credentials of its own. An erasing dispose
/// connects with the credentials the host resolves for it; without them it
/// refuses (naming the manual cleanup) and never falls back to literals.
#[tokio::test]
async fn erase_authenticates_with_fresh_dispose_credentials_without_a_start() {
    let db = MemoryDatabase::new();
    db.add_table("Items", 7);
    let mine = markers::marker_prefix("graph", SOURCE_ID);
    db.with(|s| s.markers = Some(vec![markers::new_marker(&mine).unwrap()]));
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let connectors: ConnectorFactory = {
        let (db, seen) = (db.clone(), seen.clone());
        Arc::new(move |_, authentication: &Authentication| {
            seen.lock().unwrap().push(authentication.clone());
            Arc::new(db.clone()) as Arc<dyn CdcConnector>
        })
    };
    let id = ComponentId::try_new(SOURCE_ID).unwrap();
    let progress = Arc::new(Progress {
        id: id.clone(),
        state: watch::channel(snapshot([(true, None), (true, None)])).0,
    });
    let mut source = MsSqlSource::new(
        id,
        config(json!({"barrierTable": "dbo.DrasiBarrier", "user": "", "password": null})),
        ConsumerProgressView::new(progress),
        &[
            ComponentId::try_new(CONSUMER_A).unwrap(),
            ComponentId::try_new(CONSUMER_B).unwrap(),
        ],
        true,
        connectors,
    )
    .unwrap();
    let error = step(source.dispose(&dispose_ctx(StateIntent::Erase, 1)))
        .await
        .expect_err("no credentials to erase with");
    assert!(format!("{error:#}").contains("DELETE FROM"), "{error:#}");
    assert!(
        seen.lock().unwrap().is_empty(),
        "connected without credentials"
    );
    let fresh = dispose_ctx(StateIntent::Erase, 2).with_credentials(Some(
        ActivationCredentials::UsernamePassword {
            username: "provider".into(),
            password: "fresh".into(),
        },
    ));
    step(source.dispose(&fresh)).await.unwrap();
    assert_eq!(
        seen.lock().unwrap().as_slice(),
        [Authentication::Sql {
            user: "provider".into(),
            password: "fresh".into(),
        }]
    );
    assert_eq!(db.with(|s| s.markers.as_ref().unwrap().len()), 0);
}

fn update(
    current: &SourceConfig,
    proposed: &SourceConfig,
    plan: &Assessment,
) -> ApplyUpdateRequest {
    let id = ComponentId::try_new(SOURCE_ID).unwrap();
    let request = drasi_computation_plugin_sdk::testing::update_request(
        &crate::MsSqlSourceFactory,
        &id,
        current.to_value().unwrap(),
        proposed.to_value().unwrap(),
    );
    ApplyUpdateRequest {
        operation_id: "op".into(),
        request,
        plan: plan.plan().expect("supported plan"),
        fingerprint: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn in_place_updates_need_a_completed_pause_and_restart_updates_a_stopped_source() {
    let mut f = fixture();
    let current = f.source.source_config().unwrap();
    let mut proposed = current.clone();
    proposed.settings.max_rows = 1;
    let request = update(&current, &proposed, &management::plan(&current, &proposed));
    step(f.source.start(&mut start_ctx())).await.unwrap();
    let pause = ApplyContext::new(scope(None), UpdateBoundary::Pause);
    assert!(
        step(f.source.apply(&pause, &request)).await.is_err(),
        "pause first"
    );
    step(f.source.pause(&pause_ctx(STEP))).await.unwrap();
    step(f.source.apply(&pause, &request)).await.unwrap();
    assert_eq!(f.source.configuration().unwrap()["maxRows"], 1);
    let stale = step(f.source.apply(&pause, &request)).await;
    assert!(
        stale.is_err(),
        "the current configuration no longer matches"
    );
    step(f.source.resume(&resume_ctx())).await.unwrap();
    let current = f.source.source_config().unwrap();
    let mut moved = current.clone();
    moved.settings.host = "sqlag-listener.corp".into();
    let restart = update(&current, &moved, &management::plan(&current, &moved));
    let stop = ApplyContext::new(scope(None), UpdateBoundary::Stop);
    assert!(
        step(f.source.apply(&stop, &restart)).await.is_err(),
        "stop first"
    );
    step(f.source.stop(&stop_ctx(StopMode::Drain, STEP)))
        .await
        .unwrap();
    step(f.source.apply(&stop, &restart)).await.unwrap();
    assert_eq!(
        f.source.configuration().unwrap()["host"],
        "sqlag-listener.corp"
    );
}

#[tokio::test]
async fn schema_reader_describes_while_alive_and_is_fenced_after_drop() {
    let f = fixture();
    let reader = f.source.source_schema_reader().unwrap();
    let again = f.source.source_schema_reader().unwrap();
    assert!(Arc::ptr_eq(&reader, &again), "stable at creation");
    let schema = step(reader.describe_schema()).await.unwrap().unwrap();
    assert_eq!(schema.nodes[0].label, "Items");
    drop(f);
    let error = step(reader.describe_schema()).await.unwrap_err();
    assert!(format!("{error:#}").contains("retired"));
}

#[tokio::test]
async fn activation_identities_replace_literals_and_tokens_are_access_tokens() {
    let mut f = fixture_with(json!({"user": "", "password": null}), true);
    assert!(
        step(f.source.start(&mut start_ctx())).await.is_err(),
        "needs credentials"
    );
    step(f.source.stop(&stop_ctx(StopMode::Abort, STEP)))
        .await
        .unwrap();
    let settings = f.source.settings().unwrap();
    let token = f
        .source
        .authentication(
            &settings,
            Some(ActivationCredentials::Token {
                username: String::new(),
                token: "eyJ".into(),
            }),
        )
        .unwrap();
    assert!(matches!(token, Authentication::AccessToken { .. }));
    let mut ctx = StartContext::new(
        scope(Some(STEP)),
        Some(ActivationCredentials::UsernamePassword {
            username: "provider".into(),
            password: "secret".into(),
        }),
    );
    step(f.source.start(&mut ctx)).await.unwrap();
    step(f.source.stop(&stop_ctx(StopMode::Drain, STEP)))
        .await
        .unwrap();
    let unbound = fixture();
    assert!(unbound
        .source
        .authentication(
            &settings,
            Some(ActivationCredentials::UsernamePassword {
                username: "u".into(),
                password: "p".into()
            })
        )
        .is_err());
}

#[test]
fn membership_uses_the_slowest_consumer_and_fences_failures_resets_and_regressions() {
    let id = ComponentId::try_new(SOURCE_ID).unwrap();
    let members: BTreeSet<_> = [CONSUMER_A, CONSUMER_B]
        .into_iter()
        .map(|c| ComponentId::try_new(c).unwrap())
        .collect();
    let at = |n: u64| Some(Cursor::boundary([7; 32], lsn(n)));
    let mut membership = Membership::new(
        id.clone(),
        StreamId::try_new("items/out").unwrap(),
        members.clone(),
    );
    let observed = membership
        .observe(&snapshot([(true, at(4)), (true, at(2))]))
        .unwrap();
    assert_eq!(observed.minimum.unwrap().lsn, lsn(2));
    assert!(
        membership
            .observe(&snapshot([(true, at(4)), (true, at(1))]))
            .is_err(),
        "regression"
    );
    let mut fresh = Membership::new(
        id.clone(),
        StreamId::try_new("items/out").unwrap(),
        members.clone(),
    );
    let mixed = fresh
        .observe(&snapshot([(true, at(3)), (true, None)]))
        .unwrap();
    assert_eq!((mixed.missing, mixed.minimum.unwrap().lsn), (1, lsn(3)));
    let not_ready = fresh
        .observe(&snapshot([(false, at(3)), (true, at(3))]))
        .unwrap();
    assert!(!not_ready.ready, "not ready is held, not fatal");
    let mut failed = snapshot([(true, at(3)), (true, at(3))]);
    Arc::make_mut(failed.consumers.values_mut().next().unwrap()).failure = Some("x".into());
    assert!(fresh.observe(&failed).is_err());
    let mut reset = snapshot([(true, at(3)), (true, at(3))]);
    Arc::make_mut(reset.consumers.values_mut().next().unwrap()).reset_generation += 1;
    assert!(fresh.observe(&reset).is_err());
    let mut foreign = snapshot([(true, at(3)), (true, None)]);
    foreign.consumers.values_mut().last().map(|state| {
        Arc::make_mut(state).checkpoints.insert(
            SourceProgressKey::Source(id.clone()),
            SourceCheckpoint::new(
                1,
                Some(Cursor::boundary([8; 32], lsn(3)).encode().unwrap().into()),
            ),
        )
    });
    assert!(
        fresh.observe(&foreign).is_err(),
        "cursors under another binding"
    );
}

/// A new source object over the same database and committed progress, as
/// after a process restart.
fn restarted(f: &Fixture, extra: serde_json::Value) -> MsSqlSource {
    MsSqlSource::new(
        ComponentId::try_new(SOURCE_ID).unwrap(),
        config(extra),
        ConsumerProgressView::new(f.progress.clone()),
        &[
            ComponentId::try_new(CONSUMER_A).unwrap(),
            ComponentId::try_new(CONSUMER_B).unwrap(),
        ],
        false,
        connectors(&f.db),
    )
    .unwrap()
}

fn lsn_of(envelope: &OutputEnvelope) -> [u8; 10] {
    Cursor::decode(
        envelope
            .envelope
            .system()
            .source_position()
            .expect("position"),
    )
    .unwrap()
    .lsn
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fresh_origin_is_committed_first_and_survives_a_restart_with_a_new_source_object() {
    let mut f = fixture();
    f.db.with(|s| s.max_lsn = lsn(10));
    step(f.source.start(&mut start_ctx())).await.unwrap();
    let origin = progress_cursor(&next(&mut f.source).await);
    assert_eq!(
        origin.lsn,
        lsn(10),
        "startPosition current resolves the maximum LSN"
    );
    f.db.change(7, 11, 1, 2, 1, 0);
    assert!(
        quiet(&mut f.source).await,
        "no change is published before every member committed the origin"
    );
    commit(&f, [Some(origin.clone()), Some(origin.clone())]);
    assert_eq!(lsn_of(&next(&mut f.source).await), lsn(11));
    // The process exits before any member commits a change.
    step(f.source.stop(&stop_ctx(StopMode::Abort, STEP)))
        .await
        .unwrap();
    // The old object is gone, as after a process exit.
    let replacement = restarted(&f, json!({}));
    drop(std::mem::replace(&mut f.source, replacement));
    f.db.change(7, 12, 1, 2, 2, 0);
    f.db.with(|s| s.max_lsn = lsn(20));
    let mut source = restarted(&f, json!({}));
    step(source.start(&mut start_ctx())).await.unwrap();
    assert_eq!(lsn_of(&next(&mut source).await), lsn(11), "not re-resolved");
    assert_eq!(lsn_of(&next(&mut source).await), lsn(12), "downtime change");
    step(source.stop(&stop_ctx(StopMode::Drain, STEP)))
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_crash_between_the_members_first_commits_restarts_from_the_committed_origin() {
    let mut f = fixture();
    f.db.with(|s| s.max_lsn = lsn(10));
    step(f.source.start(&mut start_ctx())).await.unwrap();
    let origin = progress_cursor(&next(&mut f.source).await);
    // A commits the origin; the process dies before B does.
    commit(&f, [Some(origin.clone()), None]);
    step(f.source.stop(&stop_ctx(StopMode::Abort, STEP)))
        .await
        .unwrap();
    // The old object is gone, as after a process exit.
    let replacement = restarted(&f, json!({}));
    drop(std::mem::replace(&mut f.source, replacement));
    f.db.change(7, 11, 1, 2, 1, 0);
    f.db.with(|s| s.max_lsn = lsn(15));
    // B has no cursor: it joins at the group minimum, A's committed origin.
    let mut source = restarted(&f, json!({}));
    step(source.start(&mut start_ctx())).await.unwrap();
    assert_eq!(lsn_of(&next(&mut source).await), lsn(11));
    assert!(quiet(&mut source).await, "exactly the uncommitted range");
    step(source.stop(&stop_ctx(StopMode::Drain, STEP)))
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_selected_tables_commit_progress_so_a_restart_survives_cdc_cleanup() {
    let mut f = fixture();
    f.source.set_idle_progress(Duration::from_millis(20));
    let origin = start_streaming(&mut f).await;
    // Unrelated tables keep the database busy; the selected table is idle.
    f.db.with(|s| s.max_lsn = lsn(40));
    let idle = progress_cursor(&next(&mut f.source).await);
    assert_eq!(idle.lsn, lsn(40));
    assert!(
        quiet(&mut f.source).await,
        "no further record until the maximum LSN moves again"
    );
    commit(&f, [Some(idle.clone()), Some(idle)]);
    step(f.source.stop(&stop_ctx(StopMode::Drain, STEP)))
        .await
        .unwrap();
    // The old object is gone, as after a process exit.
    let replacement = restarted(&f, json!({}));
    drop(std::mem::replace(&mut f.source, replacement));
    f.db.cleanup(30);
    f.db.change(7, 41, 1, 2, 1, 0);
    let mut source = restarted(&f, json!({}));
    step(source.start(&mut start_ctx())).await.unwrap();
    assert_eq!(lsn_of(&next(&mut source).await), lsn(41));
    step(source.stop(&stop_ctx(StopMode::Drain, STEP)))
        .await
        .unwrap();
    // Without the idle record the committed origin is behind cleanup.
    commit(&f, [Some(origin.clone()), Some(origin)]);
    let mut stranded = restarted(&f, json!({}));
    let error = step(stranded.start(&mut start_ctx())).await;
    let error = match error {
        Err(error) => format!("{error:#}"),
        Ok(()) => format!("{:#}", step(stranded.next()).await.unwrap_err()),
    };
    assert!(error.contains("CDC cleanup"), "{error}");
    let _ = step(stranded.stop(&stop_ctx(StopMode::Abort, STEP))).await;
}
