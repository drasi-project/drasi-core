// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use crate::{
    delivery::{HttpDeliveryError, DELIVERY_PATH},
    HttpDeliveryConfig, HttpDeliveryFactory,
};
use drasi_computation_contracts::{
    DeliveryBatch, DeliveryHandler, DeliveryScope, EnvelopeCodec, GraphProducerIdentity,
    GraphProducerProgress,
};
use drasi_computation_plugin_sdk::lifecycle::{StopMode, UpdateBoundary};
use drasi_computation_plugin_sdk::testing::{contexts, create_request, update_request};
use drasi_computation_plugin_sdk::{
    ApplyUpdateRequest, Component, ControlSender, Factory, NativeConsumer,
};
use serde_json::{json, Value};
use std::num::NonZeroUsize;

const CONSUMER_ID: &str = "4c2b9e1a-7d3f-4a5b-8c6d-9e0f1a2b3c4d";

/// Echoes the request's identity with 200, like a confirming receiver.
fn echo(request: &Captured) -> Reply {
    match request.json() {
        Ok(body) => Reply::status(200).body(body["identity"].to_string()),
        Err(_) => Reply::status(400),
    }
}

/// Confirms a different batch.
fn mismatch(request: &Captured) -> Reply {
    match request.json() {
        Ok(mut body) => {
            let sequence = body["identity"]["sequence"].as_u64().unwrap_or_default();
            body["identity"]["sequence"] = json!(sequence + 1);
            Reply::status(200).body(body["identity"].to_string())
        }
        Err(_) => Reply::status(400),
    }
}

fn consumer(configuration: Value) -> Result<Box<dyn NativeConsumer>> {
    let factory = HttpDeliveryFactory;
    let request = create_request(&factory, &ComponentId::try_new(CONSUMER_ID)?, configuration);
    match factory
        .create(&request, ControlSender::default())?
        .component
    {
        Component::Consumer(consumer) => Ok(consumer),
        _ => anyhow::bail!("factory did not create a consumer"),
    }
}

async fn started(configuration: Value) -> Result<Box<dyn NativeConsumer>> {
    let mut consumer = consumer(configuration)?;
    consumer.initialize(&contexts::initialize()).await?;
    consumer.start(&mut contexts::start()).await?;
    Ok(consumer)
}

fn batch(sequence: u64) -> Result<DeliveryBatch> {
    let mut codec = EnvelopeCodec::new(NonZeroUsize::new(1 << 20).context("bound")?);
    codec.register_schema(GraphChangeCodec::schema())?;
    let producer = GraphProducerIdentity::new(
        "instance".into(),
        "graph".into(),
        ComponentId::try_new(OUTPUT_ID)?,
        StreamId::try_new(STREAM)?,
        true,
    )?;
    let mut input = envelope(sequence, &[insert("r1", "one"), delete("r1")])?;
    GraphProducerProgress::annotate(&mut input.envelope, &producer, sequence)?;
    DeliveryBatch::new(
        DeliveryScope::new(
            "instance".into(),
            "graph".into(),
            ComponentId::try_new(CONSUMER_ID)?,
        )?,
        input,
        &codec,
    )
}

fn url(server: &Server) -> String {
    server.url(DELIVERY_PATH)
}

#[tokio::test(flavor = "current_thread")]
async fn confirms_each_batch_once_with_its_identity() -> Result<()> {
    let server = Server::start_with(None, Some(echo)).await?;
    let mut consumer = started(json!({"url": url(&server), "token": "delivery-token"})).await?;
    let batch = batch(5)?;
    consumer.handle(batch.item(0)?).await?;
    consumer.handle(batch.item(1)?).await?;
    let requests = server.state.requests();
    assert_eq!(
        requests.len(),
        1,
        "a confirmed batch completes later operations"
    );
    let request = &requests[0];
    assert_eq!(request.path, DELIVERY_PATH);
    assert_eq!(
        request.header("authorization"),
        Some("Bearer delivery-token")
    );
    let identity = batch.identity();
    let expected = format!("{}/{}", identity.stream(), identity.sequence());
    assert_eq!(
        request.header("x-drasi-delivery-id"),
        Some(expected.as_str())
    );
    let body = request.json()?;
    assert_eq!(body["version"], 1);
    assert_eq!(body["port"], "in");
    assert_eq!(body["identity"], serde_json::to_value(identity)?);
    consumer.stop(&contexts::stop(StopMode::Drain)).await?;
    assert!(
        consumer.handle(batch.item(0)?).await.is_err(),
        "a stopped consumer refuses delivery"
    );
    Ok(())
}

fn permanent(error: &anyhow::Error) -> bool {
    drasi_computation_contracts::delivery_failure_class(error)
        .is_some_and(|class| class.is_permanent())
}

#[tokio::test(flavor = "current_thread")]
async fn a_mismatched_confirmation_is_a_permanent_failure() -> Result<()> {
    let server = Server::start_with(None, Some(mismatch)).await?;
    let mut consumer = started(json!({"url": url(&server)})).await?;
    let batch = batch(1)?;
    let error = consumer
        .handle(batch.item(0)?)
        .await
        .err()
        .context("mismatch fails")?;
    assert!(matches!(
        error
            .chain()
            .find_map(|cause| cause.downcast_ref::<HttpDeliveryError>()),
        Some(HttpDeliveryError::ConfirmationMismatch)
    ));
    assert!(permanent(&error));
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn rejections_and_transport_failures_are_classified() -> Result<()> {
    let server = Server::start().await?;
    let mut consumer = started(json!({"url": url(&server), "timeoutMs": 1000})).await?;
    let batch = batch(1)?;
    // The same classification as the HTTP sink (HSA-3): 409 is transient.
    for (status, retryable) in [
        (503, true),
        (429, true),
        (500, true),
        (409, true),
        (400, false),
    ] {
        server
            .state
            .push(Reply::status(status).body("internal\ndetail"));
        let error = consumer
            .handle(batch.item(0)?)
            .await
            .err()
            .with_context(|| format!("{status} fails"))?;
        assert_eq!(!permanent(&error), retryable, "{status}");
        assert!(
            !format!("{error}").contains('\n'),
            "rejection text is sanitized"
        );
    }
    // A 200 without a confirmation body is not a confirmation.
    server.state.push(Reply::status(200));
    let error = consumer
        .handle(batch.item(0)?)
        .await
        .err()
        .context("empty 200")?;
    assert!(permanent(&error));
    let address = server.address;
    drop(server);
    tokio::task::yield_now().await;
    let mut unreachable =
        started(json!({"url": format!("http://{address}{DELIVERY_PATH}"), "timeoutMs": 1000}))
            .await?;
    let error = unreachable
        .handle(batch.item(0)?)
        .await
        .err()
        .context("connection refused")?;
    assert!(!permanent(&error), "{error:#}");
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn lifecycle_and_erase() -> Result<()> {
    let server = Server::start_with(None, Some(echo)).await?;
    let mut consumer = consumer(json!({"url": url(&server)}))?;
    consumer.initialize(&contexts::initialize()).await?;
    consumer.stop(&contexts::stop(StopMode::Drain)).await?;
    let error = consumer
        .start(&mut contexts::cancelled_start())
        .await
        .err()
        .context("cancelled start")?;
    assert_eq!(
        error.kind(),
        drasi_computation_plugin_sdk::lifecycle::LifecycleErrorKind::Cancelled
    );
    consumer.start(&mut contexts::start()).await?;
    assert!(consumer.start(&mut contexts::start()).await.is_err());
    consumer.pause(&contexts::pause()).await?;
    consumer.resume(&contexts::resume()).await?;
    consumer.handle(batch(1)?.item(0)?).await?;
    consumer.stop(&contexts::stop(StopMode::Abort)).await?;
    for attempt in [1, 2] {
        consumer
            .dispose(&contexts::dispose(
                drasi_computation_plugin_sdk::lifecycle::DisposeReason::Removal,
                drasi_computation_plugin_sdk::lifecycle::StateIntent::Erase,
                attempt,
            )?)
            .await?;
    }
    Ok(())
}

#[test]
fn configuration_round_trips_and_validates() -> Result<()> {
    let factory = HttpDeliveryFactory;
    let metadata = factory.metadata();
    let details = factory.configuration_details().context("details")?;
    details.validate_schema(&metadata.configuration)?;
    for configuration in [
        json!({"url": "http://127.0.0.1:9/drasi/delivery/v1"}),
        json!({"url": "https://127.0.0.1:9/d", "token": "t", "timeoutMs": 5000,
            "connectTimeoutMs": 100, "maxMessageBytes": 1024}),
    ] {
        let exposed = consumer(configuration.clone())?.configuration()?;
        assert!(details.equivalent_with_defaults(
            &metadata.configuration,
            &configuration,
            &exposed
        )?);
        assert_eq!(consumer(exposed.clone())?.configuration()?, exposed);
    }
    for invalid in [
        json!({}),
        json!({"url": "ftp://x/d"}),
        json!({"url": "http://x/d", "timeoutMs": 60001}),
        json!({"url": "http://x/d", "maxMessageBytes": 16777217}),
        json!({"url": "http://x/d", "tlsCaPem": "nope"}),
        json!({"url": "http://x/d", "extra": 1}),
    ] {
        assert!(consumer(invalid.clone()).is_err(), "{invalid}");
    }
    let config = HttpDeliveryConfig::parse(json!({"url": "http://x/d", "token": "hidden-token"}))?;
    assert!(!format!("{config:?}").contains("hidden-token"));
    Ok(())
}

#[test]
fn assessment_matrix() -> Result<()> {
    let id = ComponentId::try_new(CONSUMER_ID)?;
    let current = json!({"url": "http://127.0.0.1:9/d"});
    let boundary = |proposed: Value| {
        HttpDeliveryFactory
            .assess(&update_request(
                &HttpDeliveryFactory,
                &id,
                current.clone(),
                proposed,
            ))
            .ok()
            .and_then(|assessment| assessment.plan())
            .map(|plan| UpdateBoundary::from(plan.mode))
    };
    assert_eq!(boundary(current.clone()), Some(UpdateBoundary::Pause));
    assert_eq!(
        boundary(json!({"url": "http://127.0.0.1:9/d", "token": "t"})),
        Some(UpdateBoundary::Pause)
    );
    assert_eq!(
        boundary(json!({"url": "http://127.0.0.1:9/d", "timeoutMs": 10})),
        Some(UpdateBoundary::Pause)
    );
    assert_eq!(
        boundary(json!({"url": "http://127.0.0.1:9/e"})),
        Some(UpdateBoundary::Stop)
    );
    assert_eq!(
        boundary(json!({"url": "http://127.0.0.1:9/d", "connectTimeoutMs": 10})),
        Some(UpdateBoundary::Stop)
    );
    assert_eq!(
        boundary(json!({"url": "http://127.0.0.1:9/d", "timeoutMs": 0})),
        None
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn in_place_update_changes_the_token_at_pause() -> Result<()> {
    let server = Server::start_with(None, Some(echo)).await?;
    let current = json!({"url": url(&server)});
    let proposed = json!({"url": url(&server), "token": "rotated"});
    let mut consumer = started(current.clone()).await?;
    let request = update_request(
        &HttpDeliveryFactory,
        &ComponentId::try_new(CONSUMER_ID)?,
        current,
        proposed,
    );
    let assessment = consumer
        .management()
        .context("management")?
        .assess(&request)?;
    let update = ApplyUpdateRequest {
        operation_id: "rotate".into(),
        request,
        plan: assessment.plan().context("plan")?,
        fingerprint: None,
    };
    consumer.pause(&contexts::pause()).await?;
    consumer
        .management()
        .context("management")?
        .apply(&contexts::apply(UpdateBoundary::Pause), &update)
        .await
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    consumer.resume(&contexts::resume()).await?;
    consumer.handle(batch(2)?.item(0)?).await?;
    assert_eq!(
        server.state.requests()[0].header("authorization"),
        Some("Bearer rotated")
    );
    assert_eq!(consumer.configuration()?["token"], "rotated");
    Ok(())
}

fn reject(_: &Captured) -> Reply {
    Reply::status(422).body("{\"error\":\"unprocessable\"}")
}

#[tokio::test(flavor = "current_thread")]
async fn a_rejected_batch_is_settled_as_skipped_only_under_skip() -> Result<()> {
    use drasi_computation_contracts::{delivery_failure_class, DeliveryFailureClass};
    let server = Server::start_with(None, Some(reject)).await?;
    let batch = batch(3)?;
    let mut stalling = started(json!({"url": url(&server)})).await?;
    let error = stalling.handle(batch.item(0)?).await.unwrap_err();
    assert_eq!(
        delivery_failure_class(&error),
        Some(DeliveryFailureClass::Permanent)
    );
    stalling.stop(&contexts::stop(StopMode::Drain)).await?;
    let mut skipping = started(json!({"url": url(&server), "onPermanentFailure": "skip"})).await?;
    for index in 0..2 {
        let error = skipping.handle(batch.item(index)?).await.unwrap_err();
        assert_eq!(
            delivery_failure_class(&error),
            Some(DeliveryFailureClass::Skipped),
            "{error:#}"
        );
    }
    assert_eq!(
        server.state.requests().len(),
        2,
        "the rejected batch is not resent for its later operations"
    );
    skipping.stop(&contexts::stop(StopMode::Drain)).await?;
    Ok(())
}
