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
use proto::source::{self, source_service_client::SourceServiceClient};
use serde_json::{json, Value};
use std::time::Duration;
use tonic::{
    metadata::MetadataValue,
    transport::{Certificate, Channel, ClientTlsConfig},
    Code,
};

const ID: &str = "6a1d2c3b-4e5f-4a6b-8c7d-9e0f1a2b3c4d";
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

fn event(n: i64) -> source::SourceChange {
    source::SourceChange {
        r#type: source::ChangeType::Insert as i32,
        source_id: ID.into(),
        timestamp: None,
        change: Some(source::source_change::Change::Element(source::Element {
            element: Some(source::element::Element::Node(source::Node {
                metadata: Some(source::ElementMetadata {
                    reference: Some(source::ElementReference {
                        namespace: "test".into(),
                        element_id: format!("e{n}"),
                    }),
                    labels: vec!["Item".into()],
                    effective_from: 1_700_000_000_000_000_000,
                }),
                properties: None,
            })),
        })),
    }
}

async fn channel(port: u64) -> Result<Channel, tonic::transport::Error> {
    Channel::from_shared(format!("http://127.0.0.1:{port}"))
        .expect("endpoint")
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(5))
        .connect()
        .await
}

type Client = SourceServiceClient<
    tonic::service::interceptor::InterceptedService<
        Channel,
        Box<dyn FnMut(tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> + Send>,
    >,
>;

fn client(channel: Channel, token: Option<&'static str>) -> Client {
    let interceptor: Box<
        dyn FnMut(tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> + Send,
    > = Box::new(move |mut request: tonic::Request<()>| {
        if let Some(token) = token {
            let value = MetadataValue::try_from(format!("Bearer {token}"))
                .map_err(|_| tonic::Status::internal("token"))?;
            request.metadata_mut().insert("authorization", value);
        }
        Ok(request)
    });
    SourceServiceClient::with_interceptor(channel, interceptor)
}

async fn submit(
    port: u64,
    token: Option<&'static str>,
    n: i64,
) -> Result<Result<(), tonic::Status>> {
    let channel = channel(port).await?;
    Ok(client(channel, token)
        .submit_event(source::SubmitEventRequest {
            event: Some(event(n)),
        })
        .await
        .map(|response| assert!(response.into_inner().success)))
}

struct Probe {
    token: Option<&'static str>,
}

#[async_trait]
impl SourceProbe for Probe {
    async fn offer(&self, configuration: &Value) -> Result<Offer> {
        let port = configuration["port"].as_u64().context("port")?;
        let channel = match channel(port).await {
            Ok(channel) => channel,
            Err(_) => return Ok(Offer::Refused),
        };
        match client(channel, self.token)
            .submit_event(source::SubmitEventRequest {
                event: Some(event(1)),
            })
            .await
        {
            Ok(response) if response.get_ref().success => Ok(Offer::Accepted),
            Ok(response) => anyhow::bail!("rejected event: {:?}", response.into_inner()),
            Err(status) if status.code() == Code::Unavailable => Ok(Offer::Refused),
            Err(status) => anyhow::bail!("unexpected status {status}"),
        }
    }
}

fn conformance() -> SourceConformance<'static> {
    SourceConformance::new(&GrpcSourceFactory, ID).expect("conformance")
}

async fn started(configuration: Value) -> Result<Box<dyn NativeSource>> {
    let mut source = create_source(
        &GrpcSourceFactory,
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
    let metadata = GrpcSourceFactory.metadata();
    assert!(metadata.capabilities.management && metadata.capabilities.erases_owned_state);
    Ok(())
}

#[test]
fn transport_deadline_exceeds_admission_deadline() {
    for millis in [1, 1_000, 60_000] {
        let admission = Duration::from_millis(millis);
        assert!(component::transport_timeout(admission) > admission);
    }
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

#[test]
fn configuration_round_trips() -> Result<()> {
    conformance().configuration_round_trip(&[
        base(9200),
        with(
            base(9201),
            json!({"authToken": TOKEN, "rateLimitPerSecond": 10, "maxConcurrentStreams": 4}),
        ),
    ])
}

#[test]
fn assessment_matrix() -> Result<()> {
    let current = base(50051);
    conformance().assess_matrix(
        &current,
        &[
            (current.clone(), Some(UpdateBoundary::Pause)),
            (
                with(current.clone(), json!({"authToken": TOKEN})),
                Some(UpdateBoundary::Pause),
            ),
            (
                with(current.clone(), json!({"streamIdleTimeoutMs": 10})),
                Some(UpdateBoundary::Pause),
            ),
            (
                with(current.clone(), json!({"rateLimitPerSecond": 3})),
                Some(UpdateBoundary::Pause),
            ),
            (
                with(current.clone(), json!({"timeoutMs": 5000})),
                Some(UpdateBoundary::Stop),
            ),
            (
                with(current.clone(), json!({"port": 50052})),
                Some(UpdateBoundary::Stop),
            ),
            (
                with(current.clone(), json!({"maxMessageBytes": 1024})),
                Some(UpdateBoundary::Stop),
            ),
            (
                with(current.clone(), json!({"stream": "other"})),
                Some(UpdateBoundary::Replace),
            ),
            (with(current.clone(), json!({"host": "0.0.0.0"})), None),
            (
                with(current.clone(), json!({"maxConcurrentStreams": 0})),
                None,
            ),
        ],
    )
}

#[tokio::test(flavor = "current_thread")]
async fn in_place_and_restart_updates_apply_to_a_running_source() -> Result<()> {
    let port = free_port();
    let probe = Probe { token: None };
    conformance()
        .apply(
            base(port),
            with(base(port), json!({"streamIdleTimeoutMs": 1000})),
            &probe,
        )
        .await?;
    conformance()
        .apply(base(free_port()), base(free_port()), &probe)
        .await
}

#[tokio::test(flavor = "current_thread")]
async fn bearer_token_is_required_on_every_rpc() -> Result<()> {
    let config = with(base(free_port()), json!({"authToken": TOKEN}));
    let port = config["port"].as_u64().context("port")?;
    let mut source = started(config).await?;
    let denied = submit(port, None, 1).await?.expect_err("missing token");
    assert_eq!(denied.code(), Code::Unauthenticated);
    let denied = submit(port, Some("wrong-token-wrong-token"), 1)
        .await?
        .expect_err("wrong token");
    assert_eq!(denied.code(), Code::Unauthenticated);
    submit(port, Some(TOKEN), 1).await??;
    assert!(source.next().await?.is_some());
    // The health check is an RPC too and requires the token.
    let health = client(channel(port).await?, None).health_check(()).await;
    assert_eq!(
        health.expect_err("health without token").code(),
        Code::Unauthenticated
    );
    source.stop(&contexts::stop(StopMode::Drain)).await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn rate_limit_rejects_bursts_per_client() -> Result<()> {
    let config = with(
        base(free_port()),
        json!({"rateLimitPerSecond": 1, "rateLimitBurst": 1}),
    );
    let port = config["port"].as_u64().context("port")?;
    let mut source = started(config).await?;
    submit(port, None, 1).await??;
    let limited = submit(port, None, 2).await?.expect_err("rate limited");
    assert_eq!(limited.code(), Code::ResourceExhausted);
    source.stop(&contexts::stop(StopMode::Drain)).await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn streams_report_cumulative_counts_and_end_when_paused() -> Result<()> {
    let config = base(free_port());
    let port = config["port"].as_u64().context("port")?;
    let mut source = started(config).await?;
    let (sender, receiver) = tokio::sync::mpsc::channel(4);
    let mut responses = client(channel(port).await?, None)
        .stream_events(tokio_stream::wrappers::ReceiverStream::new(receiver))
        .await?
        .into_inner();
    for n in 1..=3 {
        sender.send(event(n)).await?;
        let response = responses.message().await?.context("response")?;
        assert!(response.success);
        assert_eq!(response.events_processed, n as u64);
        assert!(source.next().await?.is_some());
    }
    source.pause(&contexts::pause()).await?;
    sender.send(event(4)).await?;
    let ended = responses.message().await.expect_err("paused stream ends");
    assert_eq!(ended.code(), Code::Unavailable);
    source.resume(&contexts::resume()).await?;
    source.stop(&contexts::stop(StopMode::Drain)).await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn stream_slots_are_separate_and_bounded() -> Result<()> {
    let config = with(base(free_port()), json!({"maxConcurrentStreams": 1}));
    let port = config["port"].as_u64().context("port")?;
    let mut source = started(config).await?;
    let (_held_sender, held) = tokio::sync::mpsc::channel::<source::SourceChange>(1);
    let _held = client(channel(port).await?, None)
        .stream_events(tokio_stream::wrappers::ReceiverStream::new(held))
        .await?;
    let (_second_sender, second) = tokio::sync::mpsc::channel::<source::SourceChange>(1);
    let refused = client(channel(port).await?, None)
        .stream_events(tokio_stream::wrappers::ReceiverStream::new(second))
        .await
        .expect_err("second stream");
    assert_eq!(refused.code(), Code::ResourceExhausted);
    // Unary admission is unaffected by open streams.
    submit(port, None, 1).await??;
    assert!(source.next().await?.is_some());
    source.stop(&contexts::stop(StopMode::Abort)).await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn idle_streams_are_closed() -> Result<()> {
    let config = with(base(free_port()), json!({"streamIdleTimeoutMs": 100}));
    let port = config["port"].as_u64().context("port")?;
    let mut source = started(config).await?;
    let (_sender, receiver) = tokio::sync::mpsc::channel::<source::SourceChange>(1);
    let mut responses = client(channel(port).await?, None)
        .stream_events(tokio_stream::wrappers::ReceiverStream::new(receiver))
        .await?
        .into_inner();
    let ended = tokio::time::timeout(Duration::from_secs(5), responses.message())
        .await?
        .expect_err("idle stream ends");
    assert_eq!(ended.code(), Code::DeadlineExceeded);
    source.stop(&contexts::stop(StopMode::Drain)).await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn paused_health_is_degraded() -> Result<()> {
    let config = base(free_port());
    let port = config["port"].as_u64().context("port")?;
    let mut source = started(config).await?;
    source.pause(&contexts::pause()).await?;
    let health = client(channel(port).await?, None)
        .health_check(())
        .await?
        .into_inner();
    assert_eq!(
        health.status,
        source::health_check_response::Status::Degraded as i32
    );
    source.resume(&contexts::resume()).await?;
    source.stop(&contexts::stop(StopMode::Drain)).await?;
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
    let tls = ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(server.cert.pem()))
        .domain_name("localhost");
    let secure = Channel::from_shared(format!("https://127.0.0.1:{port}"))?
        .tls_config(tls.clone())?
        .connect_timeout(Duration::from_secs(5))
        .connect()
        .await?;
    let accepted = client(secure, None)
        .submit_event(source::SubmitEventRequest {
            event: Some(event(1)),
        })
        .await?;
    assert!(accepted.into_inner().success);
    assert!(source.next().await?.is_some());
    assert!(
        submit(port as u64, None, 2)
            .await
            .map(|result| result.is_err())
            .unwrap_or(true),
        "plaintext must not be accepted by a TLS listener"
    );
    source.stop(&contexts::stop(StopMode::Drain)).await?;

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
    let anonymous = async {
        let channel = Channel::from_shared(format!("https://127.0.0.1:{port}"))?
            .tls_config(tls)?
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(5))
            .connect()
            .await?;
        client(channel, None)
            .submit_event(source::SubmitEventRequest {
                event: Some(event(3)),
            })
            .await?;
        anyhow::Ok(())
    }
    .await;
    assert!(
        anonymous.is_err(),
        "mutual TLS requires a client certificate"
    );
    source.stop(&contexts::stop(StopMode::Drain)).await?;
    Ok(())
}

#[test]
fn unauthenticated_non_loopback_listeners_are_refused_at_construction() -> Result<()> {
    let id = drasi_computation_contracts::ComponentId::try_new(ID)?;
    assert!(create_source(
        &GrpcSourceFactory,
        &id,
        json!({"stream":"s","host":"0.0.0.0"})
    )
    .is_err());
    assert!(create_source(
        &GrpcSourceFactory,
        &id,
        json!({"stream":"s","host":"0.0.0.0","authToken":TOKEN})
    )
    .is_ok());
    Ok(())
}
