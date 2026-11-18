// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use crate::{plugin, HttpDeliveryFactory, HttpSinkFactory};
use async_trait::async_trait;
use drasi_computation_network_common::config::{SecretString, TlsMaterial};
use drasi_computation_plugin_sdk::lifecycle::{
    ApplyError, LifecycleErrorKind, StopMode, UpdateBoundary,
};
use drasi_computation_plugin_sdk::testing::{
    contexts, create_sink, update_request, SinkConformance, SinkProbe,
};
use drasi_computation_plugin_sdk::{
    ApplyUpdateRequest, ConsumerMode, ExportedPlugin, Factory, NativeSink,
};
use serde_json::{json, Value};
use std::time::Instant;

struct Probe(Arc<Destination>);

#[async_trait]
impl SinkProbe for Probe {
    fn input(&self, sequence: u64) -> Result<InputEnvelope> {
        input(sequence)
    }
    async fn expect_delivered(&self, _configuration: &Value, count: usize) -> Result<()> {
        self.0.wait_for(count).await
    }
    async fn set_available(&self, available: bool) -> Result<bool> {
        self.0
            .status
            .store(if available { 200 } else { 503 }, Ordering::Release);
        Ok(true)
    }
}

fn conformance() -> SinkConformance<'static> {
    SinkConformance::new(&HttpSinkFactory, SINK_ID, inputs()).expect("conformance")
}

fn base(server: &Server) -> Value {
    json!({"url": server.url("/events")})
}

fn with(mut value: Value, extra: Value) -> Value {
    if let (Some(value), Some(extra)) = (value.as_object_mut(), extra.as_object()) {
        value.extend(extra.clone());
    }
    value
}

fn sink(configuration: Value) -> Result<Box<dyn NativeSink>> {
    create_sink(
        &HttpSinkFactory,
        &ComponentId::try_new(SINK_ID)?,
        configuration,
        &inputs(),
    )
}

async fn started(configuration: Value) -> Result<Box<dyn NativeSink>> {
    let mut sink = sink(configuration)?;
    sink.initialize(&contexts::initialize()).await?;
    sink.start(&mut contexts::start()).await?;
    Ok(sink)
}

#[test]
fn metadata_is_valid() -> Result<()> {
    ExportedPlugin::new(plugin()?)?;
    let sink = HttpSinkFactory.metadata();
    assert!(sink.capabilities.management && sink.capabilities.erases_owned_state);
    assert!(sink.capabilities.binds_inputs);
    for secret in ["token", "secretHeaders", "tlsClientPrivateKeyPem"] {
        assert!(sink.configuration.fields[secret].secret, "{secret}");
    }
    let consumer = HttpDeliveryFactory.metadata();
    assert_eq!(
        HttpDeliveryFactory
            .metadata()
            .capabilities
            .services
            .consumer,
        Some(ConsumerMode::External)
    );
    ConsumerMode::External.validate_factory(&consumer)?;
    assert!(consumer.capabilities.management && consumer.capabilities.erases_owned_state);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn lifecycle_conformance_current_thread() -> Result<()> {
    let server = Server::start().await?;
    conformance()
        .lifecycle(base(&server), &Probe(server.state.clone()))
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lifecycle_conformance_multi_thread() -> Result<()> {
    let server = Server::start().await?;
    conformance()
        .lifecycle(
            with(base(&server), json!({"token": "conformance-token"})),
            &Probe(server.state.clone()),
        )
        .await
}

#[tokio::test(flavor = "current_thread")]
async fn retry_after_failure_conformance() -> Result<()> {
    let server = Server::start().await?;
    conformance()
        .retry_after_failure(
            with(base(&server), json!({"maxRetries": 1})),
            &Probe(server.state.clone()),
        )
        .await
}

#[tokio::test(flavor = "current_thread")]
async fn erase_is_idempotent() -> Result<()> {
    let server = Server::start().await?;
    conformance().erase(base(&server)).await
}

#[tokio::test(flavor = "current_thread")]
async fn pre_cancelled_start_leaves_the_sink_startable() -> Result<()> {
    let server = Server::start().await?;
    let mut sink = sink(base(&server))?;
    sink.initialize(&contexts::initialize()).await?;
    let error = sink
        .start(&mut contexts::cancelled_start())
        .await
        .err()
        .context("a pre-cancelled start must fail")?;
    assert_eq!(error.kind(), LifecycleErrorKind::Cancelled);
    assert!(sink.handle(input(1)?).await.is_err(), "nothing was started");
    sink.start(&mut contexts::start()).await?;
    sink.handle(input(1)?).await?;
    sink.stop(&contexts::stop(StopMode::Drain)).await?;
    Ok(())
}

fn ca() -> Result<(rcgen::Certificate, rcgen::KeyPair)> {
    let key = rcgen::KeyPair::generate()?;
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new())?;
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params.key_usages = vec![
        rcgen::KeyUsagePurpose::KeyCertSign,
        rcgen::KeyUsagePurpose::DigitalSignature,
        rcgen::KeyUsagePurpose::CrlSign,
    ];
    Ok((params.self_signed(&key)?, key))
}

fn issued(
    ca: &(rcgen::Certificate, rcgen::KeyPair),
    usage: rcgen::ExtendedKeyUsagePurpose,
) -> Result<(String, String)> {
    let key = rcgen::KeyPair::generate()?;
    let mut params =
        rcgen::CertificateParams::new(vec!["localhost".to_owned(), "127.0.0.1".to_owned()])?;
    params.extended_key_usages = vec![usage];
    let certificate = params.signed_by(&key, &ca.0, &ca.1)?;
    Ok((certificate.pem(), key.serialize_pem()))
}

#[test]
fn configuration_round_trips() -> Result<()> {
    let ca = ca()?;
    let (certificate, key) = issued(&ca, rcgen::ExtendedKeyUsagePurpose::ClientAuth)?;
    let url = "http://127.0.0.1:9/events";
    conformance().configuration_round_trip(&[
        json!({"url": url}),
        json!({"url": url, "token": "secret-token", "secretHeaders": {"x-api-key": "k"},
            "headers": {"x-tenant": "t"}, "successStatusCodes": [200, 202],
            "retryBudgetMs": 1000, "maxRetries": 0, "onPermanentFailure": "skip"}),
        json!({"baseUrl": "http://127.0.0.1:9", "batchEndpoint": "/batch",
            "adaptive": {"minBatchSize": 1, "maxBatchSize": 10, "minWaitMs": 1,
                "maxWaitMs": 50, "windowMs": 1000}}),
        json!({"baseUrl": "http://127.0.0.1:9", "outputs": {"q": {"component": OUTPUT_ID, "port": "out"}},
            "outputTemplates": {"routes": {"q": {"added": {"method": "PUT", "url": "/rooms/{{identity.id}}",
                "template": "{{json after}}", "headers": {"x-op": "{{operation}}"}}}}}}),
        json!({"url": "https://127.0.0.1:9/events", "tlsCaPem": ca.0.pem(),
            "tlsClientCertificatePem": certificate, "tlsClientPrivateKeyPem": key,
            "connectTimeoutMs": 500, "poolIdleTimeoutMs": 1000, "poolMaxIdlePerHost": 0}),
    ])
}

#[test]
fn assessment_matrix() -> Result<()> {
    let ca = ca()?;
    let current = json!({"url": "http://127.0.0.1:9/events"});
    let case = |extra: Value| with(current.clone(), extra);
    let adaptive = json!({"baseUrl": "http://127.0.0.1:9", "batchEndpoint": "/batch",
        "adaptive": {"maxBatchSize": 10}});
    conformance().assess_matrix(
        &current,
        &[
            (current.clone(), Some(UpdateBoundary::Pause)),
            (case(json!({"timeoutMs": 5000})), Some(UpdateBoundary::Pause)),
            (case(json!({"headers": {"x-a": "b"}})), Some(UpdateBoundary::Pause)),
            (case(json!({"secretHeaders": {"x-k": "v"}})), Some(UpdateBoundary::Pause)),
            (case(json!({"token": "t"})), Some(UpdateBoundary::Pause)),
            (case(json!({"maxRetries": 0})), Some(UpdateBoundary::Pause)),
            (case(json!({"retryBudgetMs": 10})), Some(UpdateBoundary::Pause)),
            (case(json!({"onPermanentFailure": "skip"})), Some(UpdateBoundary::Replace)),
            (case(json!({"successStatusCodes": [202]})), Some(UpdateBoundary::Pause)),
            (case(json!({"maxRequestBytes": 10})), Some(UpdateBoundary::Pause)),
            (
                case(json!({"outputTemplates": {"defaultTemplate": {"added": {"template": "{{json after}}"}}}})),
                Some(UpdateBoundary::Pause),
            ),
            (json!({"url": "http://127.0.0.1:9/other"}), Some(UpdateBoundary::Stop)),
            (case(json!({"baseUrl": "http://127.0.0.1:9"})), Some(UpdateBoundary::Stop)),
            (case(json!({"tlsCaPem": ca.0.pem()})), Some(UpdateBoundary::Stop)),
            (case(json!({"connectTimeoutMs": 5})), Some(UpdateBoundary::Stop)),
            (case(json!({"poolMaxIdlePerHost": 1})), Some(UpdateBoundary::Stop)),
            (case(json!({"maxBatchBytes": 1024})), Some(UpdateBoundary::Pause)),
            (adaptive.clone(), Some(UpdateBoundary::Stop)),
            (
                case(json!({"outputs": {"q": {"component": OUTPUT_ID}}})),
                Some(UpdateBoundary::Replace),
            ),
            (case(json!({"timeoutMs": 0})), None),
            (case(json!({"headers": {"x-drasi-port": "x"}})), None),
            (
                case(json!({"outputs": {"q": {"component": SINK_ID}}})),
                None,
            ),
        ],
    )?;
    // Adaptive bounds are read by the host before every collection.
    let batched = |extra: Value| with(adaptive.clone(), extra);
    conformance().assess_matrix(
        &adaptive,
        &[
            (
                batched(json!({"adaptive": {"maxBatchSize": 20, "maxWaitMs": 5}})),
                Some(UpdateBoundary::Pause),
            ),
            (
                batched(json!({"maxBatchBytes": 2048})),
                Some(UpdateBoundary::Pause),
            ),
            (batched(json!({"adaptive": {"maxBatchSize": 70000}})), None),
            (
                batched(json!({"adaptive": {"adaptiveMaxBatchSize": 5}})),
                None,
            ),
        ],
    )
}

#[test]
fn changed_input_bindings_replace_the_sink() -> Result<()> {
    let configuration = json!({"url": "http://127.0.0.1:9/events"});
    let id = ComponentId::try_new(SINK_ID)?;
    let mut request = update_request(&HttpSinkFactory, &id, configuration.clone(), configuration);
    request.current.inputs = inputs();
    let mut moved = inputs();
    moved[0].stream = StreamId::try_new("q/other")?;
    request.proposed.inputs = moved;
    let assessment = HttpSinkFactory
        .update_planner()
        .expect("factory planner")
        .assess(&request)?;
    assert_eq!(
        assessment
            .plan()
            .map(|plan| UpdateBoundary::from(plan.mode)),
        Some(UpdateBoundary::Replace)
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn in_place_update_applies_at_pause() -> Result<()> {
    let server = Server::start().await?;
    let current = base(&server);
    let proposed = with(current.clone(), json!({"headers": {"x-updated": "yes"}}));
    conformance()
        .apply(current, proposed, &Probe(server.state.clone()))
        .await?;
    let requests = server.state.requests();
    assert_eq!(requests[0].header("x-updated"), None);
    assert_eq!(requests[1].header("x-updated"), Some("yes"));
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn restart_update_applies_at_stop() -> Result<()> {
    let server = Server::start().await?;
    let current = base(&server);
    let proposed = json!({"url": server.url("/moved")});
    conformance()
        .apply(current, proposed, &Probe(server.state.clone()))
        .await?;
    let paths: Vec<_> = server
        .state
        .requests()
        .into_iter()
        .map(|request| request.path)
        .collect();
    assert_eq!(paths, ["/events", "/moved"]);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn apply_refuses_stale_plans_and_wrong_boundaries() -> Result<()> {
    let server = Server::start().await?;
    let current = base(&server);
    let mut sink = started(current.clone()).await?;
    let id = ComponentId::try_new(SINK_ID)?;
    let request = |from: Value, to: Value| {
        let mut request = update_request(&HttpSinkFactory, &id, from, to);
        request.current.inputs = inputs();
        request.proposed.inputs = inputs();
        request
    };
    let in_place = request(
        current.clone(),
        with(current.clone(), json!({"timeoutMs": 10})),
    );
    let assessment = sink.management().context("management")?.assess(&in_place)?;
    let update = ApplyUpdateRequest {
        operation_id: "test".into(),
        request: in_place,
        plan: assessment.plan().context("plan")?,
        fingerprint: None,
    };
    let refused =
        |result: Result<(), ApplyError>| matches!(result, Err(ApplyError::Refused { .. }));
    let management = sink.management().context("management")?;
    assert!(
        refused(
            management
                .apply(&contexts::apply(UpdateBoundary::Pause), &update)
                .await
        ),
        "an in-place update requires a completed pause"
    );
    let stale = ApplyUpdateRequest {
        request: request(
            with(current.clone(), json!({"maxRetries": 7})),
            with(current.clone(), json!({"timeoutMs": 10})),
        ),
        ..update.clone()
    };
    sink.pause(&contexts::pause()).await?;
    let management = sink.management().context("management")?;
    assert!(
        refused(
            management
                .apply(&contexts::apply(UpdateBoundary::Pause), &stale)
                .await
        ),
        "a plan for another current configuration is stale"
    );
    let restart = request(current.clone(), json!({"url": server.url("/x")}));
    let forged = ApplyUpdateRequest {
        request: restart,
        ..update.clone()
    };
    assert!(
        refused(
            management
                .apply(&contexts::apply(UpdateBoundary::Pause), &forged)
                .await
        ),
        "a plan that no longer matches is refused"
    );
    management
        .apply(&contexts::apply(UpdateBoundary::Pause), &update)
        .await
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    sink.resume(&contexts::resume()).await?;
    assert_eq!(sink.configuration()?["timeoutMs"], 10);
    sink.stop(&contexts::stop(StopMode::Drain)).await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn delivery_id_is_stable_across_retries_and_retry_after_is_honoured() -> Result<()> {
    let server = Server::start().await?;
    server
        .state
        .push(Reply::status(503).header("retry-after", "1"));
    let mut sink = started(with(base(&server), json!({"maxRetries": 2}))).await?;
    let started_at = Instant::now();
    sink.handle(input(7)?).await?;
    assert!(
        started_at.elapsed() >= Duration::from_millis(900),
        "Retry-After: 1 must delay the retry"
    );
    let requests = server.state.requests();
    assert_eq!(requests.len(), 2);
    let expected = format!("{OUTPUT_ID}/out/7.0");
    for request in &requests {
        assert_eq!(
            request.header("x-drasi-delivery-id"),
            Some(expected.as_str())
        );
        assert_eq!(request.header("x-drasi-component"), Some(OUTPUT_ID));
        assert_eq!(request.header("x-drasi-port"), Some("out"));
        assert_eq!(request.header("content-type"), Some("application/json"));
    }
    assert_eq!(requests[0].body, requests[1].body);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn retry_after_beyond_the_budget_stops_retrying() -> Result<()> {
    let server = Server::start().await?;
    server
        .state
        .push(Reply::status(429).header("retry-after", "30"));
    let mut sink = started(with(
        base(&server),
        json!({"maxRetries": 5, "retryBudgetMs": 2000}),
    ))
    .await?;
    let started_at = Instant::now();
    let error = sink.handle(input(1)?).await.unwrap_err();
    assert!(started_at.elapsed() < Duration::from_secs(2));
    assert_eq!(server.state.requests().len(), 1);
    // The graph retries the input in place no sooner than the receiver asked.
    assert_eq!(
        drasi_computation_contracts::delivery_failure_class(&error)
            .and_then(|class| class.retry_after()),
        Some(Duration::from_secs(30)),
        "{error:#}"
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn permanent_rejections_are_not_retried() -> Result<()> {
    let server = Server::start().await?;
    server.state.push(Reply::status(400).body("bad\nrequest"));
    let mut sink = started(with(base(&server), json!({"maxRetries": 3}))).await?;
    let error = sink.handle(input(1)?).await.err().context("400 fails")?;
    let status = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<crate::StatusError>())
        .context("typed status error")?;
    assert_eq!(status.status.as_u16(), 400);
    assert!(!status.retryable);
    assert_eq!(status.body, "bad\\nrequest", "the body is sanitized");
    assert!(permanent(&error), "{error:#}");
    assert_eq!(server.state.requests().len(), 1);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn accepted_is_not_success_unless_configured() -> Result<()> {
    let server = Server::start().await?;
    server.state.status.store(202, Ordering::Release);
    let mut sink = started(with(base(&server), json!({"maxRetries": 3}))).await?;
    assert!(sink.handle(input(1)?).await.is_err(), "202 is not Handled");
    assert_eq!(server.state.requests().len(), 1, "202 is not retried");
    let mut accepting = started(with(
        base(&server),
        json!({"successStatusCodes": [200, 202]}),
    ))
    .await?;
    accepting.handle(input(2)?).await?;
    server.state.status.store(204, Ordering::Release);
    sink.handle(input(3)?).await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn redirects_are_not_followed() -> Result<()> {
    let server = Server::start().await?;
    server
        .state
        .push(Reply::status(302).header("location", server.url("/elsewhere")));
    let mut sink = started(base(&server)).await?;
    assert!(sink.handle(input(1)?).await.is_err());
    let requests = server.state.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, "/events");
    Ok(())
}

#[test]
fn reserved_template_headers_reject_at_construction() {
    for name in ["x-drasi-delivery-id", "X-Drasi-Component", "x-drasi-port"] {
        let configuration = json!({"url": "http://127.0.0.1:9/events",
            "outputTemplates": {"defaultTemplate": {"added": {"headers": {name: "{{sequence}}"}}}}});
        assert!(sink(configuration).is_err(), "{name}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn secret_headers_and_token_are_sent() -> Result<()> {
    let server = Server::start().await?;
    let mut sink = started(with(
        base(&server),
        json!({"token": "tok", "headers": {"x-tenant": "t1"},
            "secretHeaders": {"X-Api-Key": "key-1"}}),
    ))
    .await?;
    sink.handle(input(1)?).await?;
    let request = &server.state.requests()[0];
    assert_eq!(request.header("authorization"), Some("Bearer tok"));
    assert_eq!(request.header("x-api-key"), Some("key-1"));
    assert_eq!(request.header("x-tenant"), Some("t1"));
    let config = crate::HttpSinkConfig::parse(with(
        base(&server),
        json!({"token": "tok-secret", "secretHeaders": {"x-api-key": "key-secret"}}),
    ))?;
    let debug = format!("{config:?}");
    assert!(!debug.contains("tok-secret") && !debug.contains("key-secret"));
    assert_eq!(
        config.token.as_ref().map(SecretString::expose),
        Some("tok-secret")
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn templates_render_url_method_headers_and_body() -> Result<()> {
    let server = Server::start().await?;
    let mut sink = started(json!({
        "baseUrl": server.url(""),
        "outputs": {"rooms": {"component": OUTPUT_ID, "port": "out"}},
        "outputTemplates": {"routes": {"rooms": {"added": {
            "method": "PUT", "url": "/rooms/{{identity.id}}",
            "headers": {"x-op": "{{operation}}"},
            "template": "{\"name\":{{json after.name}},\"alias\":\"{{output.alias}}\"}"
        }}}}
    }))
    .await?;
    sink.handle(input(3)?).await?;
    let request = &server.state.requests()[0];
    assert_eq!(request.method, Method::PUT);
    assert_eq!(request.path, "/rooms/room-3");
    assert_eq!(request.header("x-op"), Some("ADD"));
    assert_eq!(request.json()?, json!({"name": "one", "alias": "rooms"}));
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn render_failures_follow_on_permanent_failure() -> Result<()> {
    let server = Server::start().await?;
    // The delete renders a URL off the configured origin.
    let templates = json!({"defaultTemplate": {
        "added": {"url": "/ok"},
        "deleted": {"url": "{{#if before}}/ok{{else}}http://elsewhere.example/{{identity.id}}{{/if}}"}
    }});
    let changes = [insert("r1", "one"), delete("r1")];
    let mut stall =
        started(json!({"baseUrl": server.url(""), "outputTemplates": templates})).await?;
    let error = stall.handle(envelope(1, &changes)?).await.unwrap_err();
    assert!(permanent(&error), "{error:#}");
    assert_eq!(server.state.requests().len(), 1, "the add was delivered");
    // Under skip the input's other changes are still delivered and the input
    // completes as permanently failed, so the graph counts it as skipped.
    let changes = [delete("r1"), insert("r2", "two")];
    let mut skip = started(
        json!({"baseUrl": server.url(""), "outputTemplates": templates,
        "onPermanentFailure": "skip"}),
    )
    .await?;
    let error = skip.handle(envelope(2, &changes)?).await.unwrap_err();
    assert!(permanent(&error), "{error:#}");
    assert_eq!(
        server.state.requests().len(),
        2,
        "the delete was skipped and the later add delivered"
    );
    Ok(())
}

fn permanent(error: &anyhow::Error) -> bool {
    drasi_computation_contracts::delivery_failure_class(error)
        .is_some_and(|class| class.is_permanent())
}

fn batch_config(server: &Server, extra: Value) -> Value {
    with(
        json!({"baseUrl": server.url(""), "batchEndpoint": "/batch",
            "adaptive": {"minBatchSize": 1, "maxBatchSize": 100, "maxWaitMs": 10}}),
        extra,
    )
}

#[tokio::test(flavor = "current_thread")]
async fn batch_template_failures_follow_on_permanent_failure() -> Result<()> {
    let server = Server::start().await?;
    let templates = json!({"defaultTemplate": {
        "added": {"template": "{\"id\":{{json identity.id}}}"},
        "deleted": {"template": "not json {{identity.id}}"}
    }});
    let changes = [insert("r1", "one"), delete("r1")];
    let mut strict = started(batch_config(&server, json!({"outputTemplates": templates}))).await?;
    assert!(
        strict
            .handle_batch(vec![envelope(1, &changes)?])
            .await
            .is_err(),
        "strict never falls back to the default body"
    );
    assert!(server.state.requests().is_empty());
    let mut skip = started(batch_config(
        &server,
        json!({"outputTemplates": templates, "onPermanentFailure": "skip"}),
    ))
    .await?;
    let error = skip
        .handle_batch(vec![envelope(2, &changes)?])
        .await
        .unwrap_err();
    assert!(permanent(&error), "{error:#}");
    let requests = server.state.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, "/batch");
    assert_eq!(requests[0].json()?, json!({"batch": [{"id": "r1"}]}));
    assert_eq!(
        requests[0].header("x-drasi-delivery-id"),
        Some(format!("{OUTPUT_ID}/out/2.0").as_str()),
        "the identity names the changes the request carries"
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn batches_split_at_max_request_bytes() -> Result<()> {
    let server = Server::start().await?;
    let mut sink = started(batch_config(
        &server,
        json!({"outputTemplates": {"defaultTemplate": {"added": {"template": "{\"n\":{{json after.name}}}"}}},
            "maxRequestBytes": 30}),
    ))
    .await?;
    sink.handle_batch(vec![envelope(
        4,
        &[insert("a", "one"), insert("b", "two"), insert("c", "six")],
    )?])
    .await?;
    let requests = server.state.requests();
    assert!(requests.len() >= 2, "{requests:?}");
    let mut items = Vec::new();
    for request in &requests {
        assert!(request.body.len() <= 30);
        items.extend(
            request.json()?["batch"]
                .as_array()
                .cloned()
                .unwrap_or_default(),
        );
    }
    assert_eq!(
        items,
        [json!({"n":"one"}), json!({"n":"two"}), json!({"n":"six"})]
    );
    let ids: std::collections::BTreeSet<_> = requests
        .iter()
        .filter_map(|request| request.header("x-drasi-delivery-id").map(str::to_owned))
        .collect();
    assert_eq!(ids.len(), requests.len(), "each request names its own run");
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn a_dropped_in_flight_delivery_leaves_the_sink_usable() -> Result<()> {
    let server = Server::start().await?;
    server.state.stall.store(true, Ordering::Release);
    let mut sink = started(base(&server)).await?;
    assert!(
        tokio::time::timeout(Duration::from_millis(100), sink.handle(input(1)?))
            .await
            .is_err(),
        "a stalled destination cannot complete the delivery"
    );
    server.state.stall.store(false, Ordering::Release);
    sink.handle(input(1)?).await?;
    sink.stop(&contexts::stop(StopMode::Abort)).await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn private_ca_and_client_certificate() -> Result<()> {
    let ca = ca()?;
    let (server_certificate, server_key) = issued(&ca, rcgen::ExtendedKeyUsagePurpose::ServerAuth)?;
    let (client_certificate, client_key) = issued(&ca, rcgen::ExtendedKeyUsagePurpose::ClientAuth)?;
    let server = Server::start_with(
        Some(TlsMaterial {
            certificate_pem: server_certificate,
            private_key_pem: SecretString::new(server_key),
            client_ca_pem: Some(ca.0.pem()),
        }),
        None,
    )
    .await?;
    let url = format!("https://127.0.0.1:{}/events", server.address.port());
    let mutual = json!({"url": url, "maxRetries": 0, "tlsCaPem": ca.0.pem(),
        "tlsClientCertificatePem": client_certificate, "tlsClientPrivateKeyPem": client_key});
    let mut sink = started(mutual.clone()).await?;
    sink.handle(input(1)?).await?;
    assert_eq!(server.state.requests().len(), 1);
    let mut untrusted = started(json!({"url": url, "maxRetries": 0})).await?;
    assert!(untrusted.handle(input(2)?).await.is_err(), "unknown CA");
    let mut anonymous =
        started(json!({"url": url, "maxRetries": 0, "tlsCaPem": ca.0.pem()})).await?;
    assert!(
        anonymous.handle(input(3)?).await.is_err(),
        "the server requires a client certificate"
    );
    assert_eq!(server.state.requests().len(), 1);
    Ok(())
}
