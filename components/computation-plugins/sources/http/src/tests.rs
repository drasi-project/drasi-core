// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_computation_plugin_sdk::lifecycle::{StopMode, UpdateBoundary};
use drasi_computation_plugin_sdk::testing::{
    contexts, create_source, Offer, SourceConformance, SourceProbe,
};
use drasi_computation_plugin_sdk::{NativeComponent, NativeSource};
use serde_json::{json, Value};
use std::time::Duration;

const ID: &str = "3f7c9a1e-2b4d-4e6f-8a9b-0c1d2e3f4a5b";
const TOKEN: &str = "0123456789abcdef-token";

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .map(|address| address.port())
        .expect("free port")
}

fn base(port: u16) -> Value {
    json!({ "stream": "events", "port": port })
}

fn with(mut value: Value, extra: Value) -> Value {
    if let (Some(value), Some(extra)) = (value.as_object_mut(), extra.as_object()) {
        value.extend(extra.clone());
    }
    value
}

fn event(n: i64) -> Value {
    json!({"operation":"insert","timestamp":1_700_000_000_000_000_000u64,"element":{
        "type":"node","id":format!("e{n}"),"labels":["Item"],"properties":{"n":n}
    }})
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("client")
}

fn events_url(configuration: &Value) -> String {
    format!(
        "http://{}:{}/sources/{ID}/events",
        configuration["host"].as_str().unwrap_or("127.0.0.1"),
        configuration["port"]
    )
}

struct Probe {
    token: Option<&'static str>,
}

#[async_trait]
impl SourceProbe for Probe {
    async fn offer(&self, configuration: &Value) -> Result<Offer> {
        let mut request = client().post(events_url(configuration)).json(&event(1));
        if let Some(token) = self.token {
            request = request.bearer_auth(token);
        }
        match request.send().await {
            Err(error) if error.is_connect() => Ok(Offer::Refused),
            Err(error) => Err(error.into()),
            Ok(response) if response.status().is_success() => Ok(Offer::Accepted),
            Ok(response) if response.status() == reqwest::StatusCode::SERVICE_UNAVAILABLE => {
                Ok(Offer::Refused)
            }
            Ok(response) => anyhow::bail!("unexpected status {}", response.status()),
        }
    }
}

fn conformance() -> SourceConformance<'static> {
    SourceConformance::new(&HttpSourceFactory, ID).expect("conformance")
}

async fn started(configuration: Value) -> Result<Box<dyn NativeSource>> {
    let mut source = create_source(
        &HttpSourceFactory,
        &drasi_computation_contracts::ComponentId::try_new(ID)?,
        configuration,
    )?;
    source.initialize(&contexts::initialize()).await?;
    source.start(&mut contexts::start()).await?;
    Ok(source)
}

#[test]
fn metadata_is_valid() -> Result<()> {
    plugin()?;
    let metadata = HttpSourceFactory.metadata();
    assert!(metadata.capabilities.management && metadata.capabilities.erases_owned_state);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn lifecycle_conformance_current_thread() -> Result<()> {
    conformance()
        .lifecycle(base(free_port()), &Probe { token: None })
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lifecycle_conformance_multi_thread() -> Result<()> {
    conformance()
        .lifecycle(
            with(base(free_port()), json!({"authToken": TOKEN})),
            &Probe { token: Some(TOKEN) },
        )
        .await
}

#[tokio::test(flavor = "current_thread")]
async fn cancelled_start_conformance() -> Result<()> {
    conformance()
        .cancelled_start(base(free_port()), &Probe { token: None })
        .await
}

#[tokio::test(flavor = "current_thread")]
async fn erase_is_idempotent() -> Result<()> {
    conformance().erase(base(free_port())).await
}

fn webhook_config(port: u16, auth: Value) -> Value {
    let mut route = json!({
        "path":"/hook","methods":["POST"],
        "mappings":[{"operation":"insert","elementType":"node","template":{
            "id":"{{payload.id}}","labels":["Hook"],"properties":{"value":"{{payload.value}}"}
        }}]
    });
    if !auth.is_null() {
        route["auth"] = auth;
    }
    json!({"stream":"events","port":port,"webhooks":{"errorBehavior":"accept_and_log","routes":[route]}})
}

#[test]
fn configuration_round_trips() -> Result<()> {
    conformance().configuration_round_trip(&[
        base(9100),
        with(
            base(9101),
            json!({"authToken": TOKEN, "rateLimitPerSecond": 10, "maxBatchEvents": 5}),
        ),
        with(
            base(9102),
            json!({"adaptiveEnabled": true, "adaptiveMaxBatchSize": 50}),
        ),
        webhook_config(9103, Value::Null),
    ])
}

#[test]
fn assessment_matrix() -> Result<()> {
    let current = base(9000);
    conformance().assess_matrix(
        &current,
        &[
            (current.clone(), Some(UpdateBoundary::Pause)),
            (
                with(current.clone(), json!({"timeoutMs": 5000})),
                Some(UpdateBoundary::Pause),
            ),
            (
                with(current.clone(), json!({"authToken": TOKEN})),
                Some(UpdateBoundary::Pause),
            ),
            (
                with(current.clone(), json!({"rateLimitPerSecond": 3})),
                Some(UpdateBoundary::Pause),
            ),
            (
                with(current.clone(), json!({"maxBatchEvents": 7})),
                Some(UpdateBoundary::Pause),
            ),
            (
                with(current.clone(), json!({"port": 9001})),
                Some(UpdateBoundary::Stop),
            ),
            (
                with(current.clone(), json!({"maxConnections": 7})),
                Some(UpdateBoundary::Stop),
            ),
            (
                with(current.clone(), json!({"adaptiveEnabled": true})),
                Some(UpdateBoundary::Stop),
            ),
            (
                webhook_config(9000, Value::Null),
                Some(UpdateBoundary::Stop),
            ),
            (
                with(current.clone(), json!({"stream": "other"})),
                Some(UpdateBoundary::Replace),
            ),
            (with(current.clone(), json!({"maxBatchEvents": 0})), None),
        ],
    )?;
    let hooks = webhook_config(9000, Value::Null);
    let mut changed_route = hooks.clone();
    changed_route["webhooks"]["routes"][0]["path"] = json!("/other");
    conformance().assess_matrix(&hooks, &[(changed_route, Some(UpdateBoundary::Pause))])
}

#[tokio::test(flavor = "current_thread")]
async fn in_place_and_restart_updates_apply_to_a_running_source() -> Result<()> {
    let port = free_port();
    let probe = Probe { token: None };
    conformance()
        .apply(
            base(port),
            with(base(port), json!({"timeoutMs": 5000, "maxBatchEvents": 9})),
            &probe,
        )
        .await?;
    conformance()
        .apply(base(free_port()), base(free_port()), &probe)
        .await
}

#[tokio::test(flavor = "current_thread")]
async fn bearer_token_protects_data_routes_but_not_health() -> Result<()> {
    let config = with(base(free_port()), json!({"authToken": TOKEN}));
    let mut source = started(config.clone()).await?;
    let url = events_url(&config);
    let missing = client().post(&url).json(&event(1)).send().await?;
    assert_eq!(missing.status(), reqwest::StatusCode::UNAUTHORIZED);
    assert_eq!(missing.headers()["www-authenticate"], "Bearer");
    let wrong = client()
        .post(&url)
        .bearer_auth("wrong-token-wrong-token")
        .json(&event(1))
        .send()
        .await?;
    assert_eq!(wrong.status(), reqwest::StatusCode::UNAUTHORIZED);
    let ok = client()
        .post(&url)
        .bearer_auth(TOKEN)
        .json(&event(1))
        .send()
        .await?;
    assert!(ok.status().is_success());
    assert!(source.next().await?.is_some());
    let health = client()
        .get(format!("http://127.0.0.1:{}/health", config["port"]))
        .send()
        .await?;
    assert!(health.status().is_success());
    source.stop(&contexts::stop(StopMode::Drain)).await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn rate_limit_rejects_bursts_per_client() -> Result<()> {
    let config = with(
        base(free_port()),
        json!({"rateLimitPerSecond": 1, "rateLimitBurst": 1}),
    );
    let mut source = started(config.clone()).await?;
    let url = events_url(&config);
    assert!(client()
        .post(&url)
        .json(&event(1))
        .send()
        .await?
        .status()
        .is_success());
    let limited = client().post(&url).json(&event(2)).send().await?;
    assert_eq!(limited.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(limited.headers()["retry-after"], "1");
    source.stop(&contexts::stop(StopMode::Drain)).await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn oversized_bodies_are_rejected_with_413_and_bad_json_with_400() -> Result<()> {
    let config = with(base(free_port()), json!({"maxMessageBytes": 64}));
    let mut source = started(config.clone()).await?;
    let url = events_url(&config);
    let large = client().post(&url).json(&event(123_456_789)).send().await?;
    assert_eq!(large.status(), reqwest::StatusCode::PAYLOAD_TOO_LARGE);
    let bad = client()
        .post(&url)
        .header("content-type", "application/json")
        .body("{")
        .send()
        .await?;
    assert_eq!(bad.status(), reqwest::StatusCode::BAD_REQUEST);
    let body: Value = bad.json().await?;
    assert_eq!(
        body["success"], false,
        "router rejections use the JSON error shape"
    );
    source.stop(&contexts::stop(StopMode::Drain)).await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn paused_source_reports_paused_health_and_refuses_with_retry_after() -> Result<()> {
    let config = base(free_port());
    let mut source = started(config.clone()).await?;
    source.pause(&contexts::pause()).await?;
    let refused = client()
        .post(events_url(&config))
        .json(&event(1))
        .send()
        .await?;
    assert_eq!(refused.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(refused.headers()["retry-after"], "1");
    let health: Value = client()
        .get(format!("http://127.0.0.1:{}/health", config["port"]))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(health["status"], "paused");
    assert_eq!(health["admission"], "volatile");
    source.resume(&contexts::resume()).await?;
    source.stop(&contexts::stop(StopMode::Drain)).await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn drain_stop_is_not_held_by_a_client_that_never_sends_headers() -> Result<()> {
    let config = with(base(free_port()), json!({"headerReadTimeoutMs": 200}));
    let mut source = started(config.clone()).await?;
    let mut stalled = tokio::net::TcpStream::connect((
        "127.0.0.1",
        config["port"].as_u64().context("port")? as u16,
    ))
    .await?;
    tokio::io::AsyncWriteExt::write_all(&mut stalled, b"POST /sources/").await?;
    tokio::time::timeout(
        Duration::from_secs(5),
        source.stop(&contexts::stop(StopMode::Drain)),
    )
    .await
    .context("drain must not wait for a stalled client")??;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn connection_limit_closes_excess_connections() -> Result<()> {
    let config = with(base(free_port()), json!({"maxConnections": 1}));
    let mut source = started(config.clone()).await?;
    let port = config["port"].as_u64().context("port")? as u16;
    let _held = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut excess = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?;
    let mut buffer = [0u8; 1];
    let read = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::io::AsyncReadExt::read(&mut excess, &mut buffer),
    )
    .await?;
    assert!(
        matches!(read, Ok(0) | Err(_)),
        "excess connection must be closed"
    );
    source.stop(&contexts::stop(StopMode::Abort)).await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn tls_and_mutual_tls_listeners() -> Result<()> {
    let server = rcgen::generate_simple_self_signed(vec!["localhost".into()])?;
    let port = free_port();
    let config = with(
        base(port),
        json!({"tlsCertificatePem": server.cert.pem(), "tlsPrivateKeyPem": server.key_pair.serialize_pem()}),
    );
    let mut source = started(config).await?;
    let address = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let tls_client = reqwest::Client::builder()
        .add_root_certificate(reqwest::Certificate::from_pem(
            server.cert.pem().as_bytes(),
        )?)
        .resolve("localhost", address)
        .timeout(Duration::from_secs(5))
        .build()?;
    let url = format!("https://localhost:{port}/sources/{ID}/events");
    assert!(tls_client
        .post(&url)
        .json(&event(1))
        .send()
        .await?
        .status()
        .is_success());
    assert!(source.next().await?.is_some());
    let plain = client()
        .post(format!("http://127.0.0.1:{port}/sources/{ID}/events"))
        .json(&event(2))
        .send()
        .await;
    assert!(
        plain
            .map(|response| !response.status().is_success())
            .unwrap_or(true),
        "plaintext must not be accepted by a TLS listener"
    );
    source.stop(&contexts::stop(StopMode::Drain)).await?;

    // With a client CA, a client without a certificate cannot connect.
    let client_ca = rcgen::generate_simple_self_signed(vec!["client-ca".into()])?;
    let port = free_port();
    let config = with(
        base(port),
        json!({
            "tlsCertificatePem": server.cert.pem(),
            "tlsPrivateKeyPem": server.key_pair.serialize_pem(),
            "tlsClientCaPem": client_ca.cert.pem(),
        }),
    );
    let mut source = started(config).await?;
    let anonymous = reqwest::Client::builder()
        .add_root_certificate(reqwest::Certificate::from_pem(
            server.cert.pem().as_bytes(),
        )?)
        .resolve(
            "localhost",
            std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        )
        .timeout(Duration::from_secs(5))
        .build()?;
    assert!(anonymous
        .post(format!("https://localhost:{port}/sources/{ID}/events"))
        .json(&event(3))
        .send()
        .await
        .is_err());
    source.stop(&contexts::stop(StopMode::Drain)).await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn webhook_auth_failure_is_always_a_generic_401() -> Result<()> {
    const VARIABLE: &str = "DRASI_HTTP_SOURCE_TEST_WEBHOOK_TOKEN";
    std::env::set_var(VARIABLE, "webhook-secret-token-value");
    let port = free_port();
    let config = webhook_config(port, json!({"bearer":{"tokenEnv": VARIABLE}}));
    let mut source = started(config).await?;
    let url = format!("http://127.0.0.1:{port}/hook");
    let denied = client()
        .post(&url)
        .bearer_auth("wrong")
        .json(&json!({"id":"a","value":1}))
        .send()
        .await?;
    assert_eq!(denied.status(), reqwest::StatusCode::UNAUTHORIZED);
    let text = denied.text().await?;
    assert!(
        !text.contains(VARIABLE),
        "auth failures must not reveal configuration: {text}"
    );
    let accepted = client()
        .post(&url)
        .bearer_auth("webhook-secret-token-value")
        .json(&json!({"id":"a","value":1}))
        .send()
        .await?;
    assert!(accepted.status().is_success());
    assert!(source.next().await?.is_some());
    let schema = source
        .source_schema_reader()
        .context("schema reader")?
        .describe_schema()
        .await?
        .context("webhook schema")?;
    assert_eq!(schema.nodes[0].label, "Hook");
    source.stop(&contexts::stop(StopMode::Drain)).await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn initialize_reports_missing_webhook_secrets() -> Result<()> {
    let config = webhook_config(
        free_port(),
        json!({"bearer":{"tokenEnv":"DRASI_HTTP_SOURCE_TEST_UNSET_TOKEN_93f"}}),
    );
    let mut source = create_source(
        &HttpSourceFactory,
        &drasi_computation_contracts::ComponentId::try_new(ID)?,
        config,
    )?;
    assert!(source.initialize(&contexts::initialize()).await.is_err());
    Ok(())
}

#[test]
fn unauthenticated_non_loopback_listeners_are_refused_at_construction() -> Result<()> {
    let id = drasi_computation_contracts::ComponentId::try_new(ID)?;
    assert!(create_source(
        &HttpSourceFactory,
        &id,
        json!({"stream":"s","host":"0.0.0.0"})
    )
    .is_err());
    assert!(create_source(
        &HttpSourceFactory,
        &id,
        json!({"stream":"s","host":"0.0.0.0","authToken":TOKEN})
    )
    .is_ok());
    Ok(())
}
