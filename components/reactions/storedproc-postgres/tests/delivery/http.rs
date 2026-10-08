// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use anyhow::Result;
use axum::{body::Body, extract::Request, http::StatusCode, middleware::Next};
use drasi_computation_network::delivery::{
    HttpDeliveryEndpoint, HttpDeliveryError, HttpDeliveryHandler, DELIVERY_PATH,
};
use std::sync::{atomic::AtomicUsize, Mutex};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Responses {
    requests: AtomicUsize,
    hold: AtomicBool,
    ready: Notify,
    release: Notify,
    replacement: Mutex<Option<(StatusCode, Vec<u8>)>>,
}

struct Server {
    endpoint: HttpDeliveryEndpoint,
    stop: CancellationToken,
    worker: RwLock<Option<tokio::task::JoinHandle<std::io::Result<()>>>>,
    url: reqwest::Url,
}

fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()?)
}

impl Server {
    async fn start(endpoint: HttpDeliveryEndpoint, responses: Arc<Responses>) -> Result<Self> {
        let router = endpoint
            .router()
            .layer(axum::middleware::from_fn(
                move |request: Request, next: Next| {
                    let responses = responses.clone();
                    async move {
                        responses.requests.fetch_add(1, Ordering::AcqRel);
                        let mut response = next.run(request).await;
                        if responses.hold.load(Ordering::Acquire) {
                            responses.ready.notify_one();
                            responses.release.notified().await;
                        }
                        if let Some((status, bytes)) =
                            responses.replacement.lock().expect("response lock").take()
                        {
                            *response.status_mut() = status;
                            *response.body_mut() = Body::from(bytes);
                            response.headers_mut().remove("content-length");
                        }
                        response
                    }
                },
            ))
            .route("/health", axum::routing::get(|| async { "ready" }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let stop = CancellationToken::new();
        let shutdown = stop.clone();
        let worker = RwLock::new(None);
        spawn_owned_worker(&worker, async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(shutdown.cancelled_owned())
                .await
        })
        .await?;
        let response = client()?
            .get(format!("{base}/health"))
            .timeout(Duration::from_secs(5))
            .send()
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        Ok(Self {
            endpoint,
            stop,
            worker,
            url: format!("{base}{DELIVERY_PATH}").parse()?,
        })
    }

    async fn stop(mut self) -> Result<()> {
        self.endpoint.shutdown().await?;
        self.stop.cancel();
        match join_owned_worker(self.worker.get_mut(), Duration::from_secs(5)).await? {
            WorkerCompletion::Completed(result) => result?,
            other => anyhow::bail!("HTTP server did not finish: {other:?}"),
        }
        Ok(())
    }
}

async fn endpoint(
    database: &mut Database,
    path: &Path,
    effect: Effect,
) -> Result<HttpDeliveryEndpoint> {
    let mut handler = PostgresDeliveryHandler::new(database.connect().await?, effect);
    handler.initialize().await?;
    HttpDeliveryEndpoint::new(
        runner(path).await?,
        codec()?,
        Box::new(handler),
        size(1 << 20),
    )
}

fn sender(server: &Server, timeout: Duration) -> Result<HttpDeliveryHandler> {
    HttpDeliveryHandler::new(
        client()?,
        server.url.clone(),
        codec()?,
        size(1 << 20),
        timeout,
    )
}

fn request(delivery: &DeliveryRunner, batch: &InputEnvelope) -> Result<serde_json::Value> {
    Ok(serde_json::json!({
        "version": 1, "port": batch.port,
        "identity": delivery.batch_identity(batch)?,
        "envelope": serde_json::from_slice::<serde_json::Value>(&codec()?.encode(&batch.envelope)?)?,
    }))
}

#[tokio::test(flavor = "current_thread")]
async fn http_remote_progress_loss_and_partial_batches_resume_only_unfinished_effects() -> Result<()>
{
    let mut database = Database::new().await?;
    let observer = database.connect().await?;
    setup(&observer).await?;
    let remote = tempfile::tempdir()?;
    let local = tempfile::tempdir()?;
    let mut delivery = runner(local.path()).await?;
    let batch = input(1, 3)?;
    let mut postgres = PostgresDeliveryHandler::new(database.connect().await?, Effect::default());
    postgres.initialize().await?;
    let lost = HttpDeliveryEndpoint::new(
        runner(remote.path()).await?,
        codec()?,
        Box::new(LostResponse {
            inner: postgres,
            lose: true,
        }),
        size(1 << 20),
    )?;
    let server = Server::start(lost, Arc::new(Responses::default())).await?;
    let mut handler = sender(&server, Duration::from_secs(5))?;
    assert!(delivery.deliver(&batch, &mut handler).await.is_err());
    assert_eq!(effects(&observer).await?, 1);
    assert_eq!(delivery.progress().await?[0].completed_operations, 0);
    drop(handler);
    server.stop().await?;
    let server = Server::start(
        endpoint(
            &mut database,
            remote.path(),
            Effect {
                fail_second: true,
                ..Default::default()
            },
        )
        .await?,
        Arc::new(Responses::default()),
    )
    .await?;
    let mut handler = sender(&server, Duration::from_secs(5))?;
    assert!(delivery.deliver(&batch, &mut handler).await.is_err());
    assert_eq!(effects(&observer).await?, 1);
    assert_eq!(delivery.progress().await?[0].completed_operations, 0);
    drop(handler);
    server.stop().await?;
    let server = Server::start(
        endpoint(&mut database, remote.path(), Effect::default()).await?,
        Arc::new(Responses::default()),
    )
    .await?;
    let mut handler = sender(&server, Duration::from_secs(5))?;
    delivery.deliver(&batch, &mut handler).await?;
    assert_eq!(effects(&observer).await?, 3);
    assert_eq!(delivery.progress().await?[0].completed_operations, 3);
    delivery.shutdown().await?;
    drop(handler);
    server.stop().await?;
    drop(observer);
    database.shutdown().await
}

struct NonAtomicEffect {
    effects: Arc<AtomicUsize>,
    fail_once: bool,
}

#[tokio::test(flavor = "current_thread")]
async fn http_rejects_state_runners_before_accepting_external_handlers() -> Result<()> {
    let path = tempfile::tempdir()?;
    let indexes = LegacyIndexProviderAdapter::new(Arc::new(RocksDbIndexProvider::new(
        path.path(),
        false,
        false,
    )))
    .create_indexes("delivery", "consumer")
    .await?;
    let cleanup = indexes.cleanup().expect("cleanup").clone();
    let delivery = DeliveryRunner::new_transactional(
        DeliveryScope::new(
            "scope".into(),
            "graph".into(),
            ComponentId::try_new("sink")?,
        )?,
        indexes,
        codec()?,
        DeliveryOptions {
            scope: RecoveryScope::Failure(FailureMode::ProcessRestart),
            max_streams: size(2),
            receipts_per_stream: size(2),
            retry: DeliveryRetryPolicy::default(),
        },
    )?;
    let effects = Arc::new(AtomicUsize::new(0));
    let result = HttpDeliveryEndpoint::new(
        delivery,
        codec()?,
        Box::new(NonAtomicEffect {
            effects: effects.clone(),
            fail_once: false,
        }),
        size(1 << 20),
    );
    let Err(error) = result else {
        panic!("a transactional-state runner must not host external effects");
    };
    assert!(matches!(
        error.downcast_ref(),
        Some(DeliveryError::ModeMismatch)
    ));
    assert_eq!(effects.load(Ordering::Acquire), 0);
    cleanup.cancel();
    cleanup.shutdown().await?;
    drop(cleanup);
    let mut delivery = runner(path.path()).await?;
    assert!(delivery.progress().await?.is_empty());
    delivery.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn http_exact_message_limits_and_explicit_retry_classification_are_enforced() -> Result<()> {
    let local = tempfile::tempdir()?;
    let remote = tempfile::tempdir()?;
    let mut delivery = runner(local.path()).await?;
    let batch = input(u64::MAX, 1)?;
    let limit = serde_json::to_vec(&request(&delivery, &batch)?)?.len();
    let effects = Arc::new(AtomicUsize::new(0));
    let endpoint = HttpDeliveryEndpoint::new(
        runner(remote.path()).await?,
        codec()?,
        Box::new(NonAtomicEffect {
            effects: effects.clone(),
            fail_once: false,
        }),
        size(limit),
    )?;
    let responses = Arc::new(Responses::default());
    let server = Server::start(endpoint, responses.clone()).await?;
    let mut too_small = HttpDeliveryHandler::new(
        client()?,
        server.url.clone(),
        codec()?,
        size(limit - 1),
        Duration::from_secs(5),
    )?;
    assert!(delivery.deliver(&batch, &mut too_small).await.is_err());
    assert_eq!(responses.requests.load(Ordering::Acquire), 0);
    assert_eq!(effects.load(Ordering::Acquire), 0);
    let mut exact = HttpDeliveryHandler::new(
        client()?,
        server.url.clone(),
        codec()?,
        size(limit),
        Duration::from_secs(5),
    )?;
    for (status, retryable) in [
        (StatusCode::TOO_MANY_REQUESTS, true),
        (StatusCode::SERVICE_UNAVAILABLE, true),
        (StatusCode::BAD_REQUEST, false),
        (StatusCode::CONFLICT, false),
        (StatusCode::UNPROCESSABLE_ENTITY, false),
        (StatusCode::ACCEPTED, false),
    ] {
        assert_eq!(
            exact.retryable(
                &HttpDeliveryError::Rejected {
                    status,
                    message: "fixture".into()
                }
                .into()
            ),
            retryable
        );
    }
    for (url, bytes, timeout) in [
        ("ftp://localhost/delivery", 1024, Duration::from_secs(1)),
        (
            "http://localhost/delivery#fragment",
            1024,
            Duration::from_secs(1),
        ),
        (
            "http://localhost/delivery",
            (16 << 20) + 1,
            Duration::from_secs(1),
        ),
        ("http://localhost/delivery", 1024, Duration::ZERO),
        (
            "http://localhost/delivery",
            1024,
            Duration::from_secs(60) + Duration::from_nanos(1),
        ),
    ] {
        assert!(
            HttpDeliveryHandler::new(client()?, url.parse()?, codec()?, size(bytes), timeout)
                .is_err()
        );
    }
    HttpDeliveryHandler::new(
        client()?,
        server.url.clone(),
        codec()?,
        size(16 << 20),
        Duration::from_secs(60),
    )?;
    delivery.deliver(&batch, &mut exact).await?;
    assert_eq!(responses.requests.load(Ordering::Acquire), 1);
    assert_eq!(effects.load(Ordering::Acquire), 1);
    assert_eq!(delivery.progress().await?[0].handled_sequence, u64::MAX);
    delivery.shutdown().await?;
    drop(exact);
    drop(too_small);
    server.stop().await
}

#[async_trait]
impl DeliveryHandler for NonAtomicEffect {
    async fn handle(&mut self, _item: DeliveryItem<'_>) -> Result<()> {
        self.effects.fetch_add(1, Ordering::AcqRel);
        if self.fail_once {
            self.fail_once = false;
            anyhow::bail!("failure after an external nontransactional effect");
        }
        Ok(())
    }
}

#[tokio::test(flavor = "current_thread")]
async fn http_nontransactional_effects_explicitly_remain_at_least_once() -> Result<()> {
    let remote = tempfile::tempdir()?;
    let local = tempfile::tempdir()?;
    let effects = Arc::new(AtomicUsize::new(0));
    let endpoint = HttpDeliveryEndpoint::new(
        runner(remote.path()).await?,
        codec()?,
        Box::new(NonAtomicEffect {
            effects: effects.clone(),
            fail_once: true,
        }),
        size(1 << 20),
    )?;
    let server = Server::start(endpoint, Arc::new(Responses::default())).await?;
    let mut delivery = runner(local.path()).await?;
    let mut handler = sender(&server, Duration::from_secs(5))?;
    let batch = input(1, 1)?;
    assert!(delivery.deliver(&batch, &mut handler).await.is_err());
    assert_eq!(delivery.progress().await?[0].completed_operations, 0);
    delivery.deliver(&batch, &mut handler).await?;
    assert_eq!(
        effects.load(Ordering::Acquire),
        2,
        "identities alone must not imply external deduplication"
    );
    assert_eq!(delivery.progress().await?[0].completed_operations, 1);
    delivery.shutdown().await?;
    drop(handler);
    server.stop().await
}

#[tokio::test(flavor = "current_thread")]
async fn http_lost_response_and_two_sided_reconstruction_preserve_once_only_effects() -> Result<()>
{
    let mut database = Database::new().await?;
    let observer = database.connect().await?;
    setup(&observer).await?;
    let remote = tempfile::tempdir()?;
    let responses = Arc::new(Responses::default());
    let server = Server::start(
        endpoint(&mut database, remote.path(), Effect::default()).await?,
        responses.clone(),
    )
    .await?;
    let local = tempfile::tempdir()?;
    let mut delivery = runner(local.path()).await?;
    let batch = input(1, 3)?;
    responses.hold.store(true, Ordering::Release);
    let mut handler = sender(&server, Duration::from_secs(2))?;
    {
        let pending = delivery.deliver(&batch, &mut handler);
        tokio::pin!(pending);
        tokio::select! {
            result = &mut pending => panic!("destination must finish before response timeout: {result:?}"),
            _ = responses.ready.notified() => {}
        }
        assert_eq!(effects(&observer).await?, 3);
        assert!(pending.await.is_err());
    }
    assert_eq!(delivery.progress().await?[0].completed_operations, 0);
    responses.hold.store(false, Ordering::Release);
    responses.release.notify_one();
    delivery.shutdown().await?;
    drop(delivery);
    drop(handler);
    server.stop().await?;
    let server = Server::start(
        endpoint(&mut database, remote.path(), Effect::default()).await?,
        responses.clone(),
    )
    .await?;
    let mut delivery = runner(local.path()).await?;
    let mut handler = sender(&server, Duration::from_secs(5))?;
    delivery.deliver(&batch, &mut handler).await?;
    assert_eq!(effects(&observer).await?, 3);
    assert_eq!(delivery.progress().await?[0].completed_operations, 3);
    assert_eq!(
        responses.requests.load(Ordering::Acquire),
        2,
        "whole-batch encoding/request must not repeat per operation"
    );
    delivery.deliver(&batch, &mut handler).await?;
    assert_eq!(responses.requests.load(Ordering::Acquire), 2);
    delivery.shutdown().await?;
    drop(handler);
    server.stop().await?;
    drop(observer);
    database.shutdown().await
}

#[tokio::test(flavor = "current_thread")]
async fn http_validates_actual_content_and_rejects_queued_or_forged_confirmations() -> Result<()> {
    let mut database = Database::new().await?;
    let observer = database.connect().await?;
    setup(&observer).await?;
    let remote = tempfile::tempdir()?;
    let responses = Arc::new(Responses::default());
    let server = Server::start(
        endpoint(&mut database, remote.path(), Effect::default()).await?,
        responses.clone(),
    )
    .await?;
    let local = tempfile::tempdir()?;
    let mut delivery = runner(local.path()).await?;
    let batch = input(1, 1)?;
    let original = request(&delivery, &batch)?;
    let mut changed_version = original.clone();
    changed_version["version"] = serde_json::json!(2);
    let mut changed_digest = original.clone();
    changed_digest["identity"]["digest"] = serde_json::to_value([0_u8; 32])?;
    let mut changed_scope = original.clone();
    changed_scope["identity"]["stream"] = serde_json::json!("wrong-consumer");
    let mut changed_content = original.clone();
    changed_content["envelope"] = request(&delivery, &input(1, 2)?)?["envelope"].clone();
    let mut changed_port = original.clone();
    changed_port["port"] = serde_json::json!("different");
    for request in [
        changed_version,
        changed_digest,
        changed_scope,
        changed_content,
        changed_port,
    ] {
        let response = client()?
            .post(server.url.clone())
            .json(&request)
            .send()
            .await?;
        assert!(matches!(
            response.status(),
            StatusCode::BAD_REQUEST | StatusCode::CONFLICT
        ));
        assert_eq!(effects(&observer).await?, 0);
    }
    let response = client()?
        .post(server.url.clone())
        .header("content-type", "application/json")
        .body(" ".repeat((1 << 20) + 1))
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(effects(&observer).await?, 0);
    let identity = serde_json::to_vec(&delivery.batch_identity(&batch)?)?;
    *responses.replacement.lock().expect("response lock") = Some((StatusCode::ACCEPTED, identity));
    let mut handler = sender(&server, Duration::from_secs(5))?;
    assert!(delivery.deliver(&batch, &mut handler).await.is_err());
    assert_eq!(delivery.progress().await?[0].completed_operations, 0);
    assert_eq!(effects(&observer).await?, 1);
    let mut forged = serde_json::to_value(delivery.batch_identity(&batch)?)?;
    forged["sequence"] = serde_json::json!(99);
    *responses.replacement.lock().expect("response lock") =
        Some((StatusCode::OK, serde_json::to_vec(&forged)?));
    assert!(delivery.deliver(&batch, &mut handler).await.is_err());
    assert_eq!(delivery.progress().await?[0].completed_operations, 0);
    *responses.replacement.lock().expect("response lock") =
        Some((StatusCode::OK, vec![b' '; 4097]));
    assert!(delivery.deliver(&batch, &mut handler).await.is_err());
    assert_eq!(delivery.progress().await?[0].completed_operations, 0);
    delivery.deliver(&batch, &mut handler).await?;
    assert_eq!(effects(&observer).await?, 1);
    delivery.shutdown().await?;
    drop(handler);
    server.stop().await?;
    drop(observer);
    database.shutdown().await
}

#[tokio::test(flavor = "current_thread")]
async fn http_bounded_admission_and_shutdown_cancel_sql_without_advancing_progress() -> Result<()> {
    let mut database = Database::new().await?;
    let observer = database.connect().await?;
    setup(&observer).await?;
    let remote = tempfile::tempdir()?;
    let entered = Arc::new(Notify::new());
    let hold = Arc::new(AtomicBool::new(true));
    let server = Server::start(
        endpoint(
            &mut database,
            remote.path(),
            Effect {
                hold: Some((hold.clone(), entered.clone())),
                ..Default::default()
            },
        )
        .await?,
        Arc::new(Responses::default()),
    )
    .await?;
    let local = tempfile::tempdir()?;
    let mut delivery = runner(local.path()).await?;
    let batch = input(1, 1)?;
    let payload = request(&delivery, &batch)?;
    let mut handler = sender(&server, Duration::from_secs(5))?;
    {
        let pending = delivery.deliver(&batch, &mut handler);
        tokio::pin!(pending);
        tokio::select! {
            result = &mut pending => panic!("SQL effect must remain held: {result:?}"),
            _ = entered.notified() => {}
        }
        let response = client()?
            .post(server.url.clone())
            .json(&payload)
            .send()
            .await?;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(effects(&observer).await?, 0);
        tokio::time::timeout(Duration::from_secs(5), server.endpoint.shutdown()).await??;
        assert!(pending.await.is_err());
    }
    assert_eq!(delivery.progress().await?[0].completed_operations, 0);
    drop(handler);
    server.stop().await?;
    let server = Server::start(
        endpoint(&mut database, remote.path(), Effect::default()).await?,
        Arc::new(Responses::default()),
    )
    .await?;
    let mut handler = sender(&server, Duration::from_secs(5))?;
    delivery.deliver(&batch, &mut handler).await?;
    assert_eq!(effects(&observer).await?, 1);
    delivery.shutdown().await?;
    drop(handler);
    server.stop().await?;
    drop(observer);
    database.shutdown().await
}
