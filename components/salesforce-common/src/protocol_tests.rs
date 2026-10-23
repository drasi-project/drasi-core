#![allow(clippy::unwrap_used)]

use std::pin::Pin;
use std::time::Duration;

use anyhow::Result;
use serde_json::json;
use tokio::sync::mpsc;
use tokio_stream::wrappers::TcpListenerStream;
use tokio_stream::{Stream, StreamExt};
use tonic::{Response, Status};
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

use crate::auth::SalesforceSessionToken;
use crate::config::{SalesforceAuthConfig, SalesforceConnectionConfig};
use crate::mapping::tests::{fixture, schema_json};
use crate::mapping::{Decoder as NativeDecoder, Operation};
use crate::projection::ObjectPlan;
use crate::proto;
use crate::proto::pub_sub_server::{PubSub, PubSubServer};
use crate::pubsub::{ReplayCursor, SalesforcePubSubClient};
use crate::rest::SalesforceRestClient;

fn auth_connection(server: &MockServer) -> SalesforceConnectionConfig {
    SalesforceConnectionConfig {
        instance_url: server.uri(),
        expected_org_id: "00D000000000001EAA".into(),
        api_version: "68.0".into(),
        auth: SalesforceAuthConfig::ClientCredentials {
            token_url: format!("{}/oauth/token", server.uri()),
            client_id: "fixture-client".into(),
            client_secret: "fixture-secret".into(),
        },
    }
}

#[tokio::test]
async fn oauth_cache_refresh_redirect_and_debug_contracts() -> Result<()> {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "fixture-access-token", "expires_in": 60
        })))
        .mount(&server)
        .await;
    let auth = crate::SalesforceAuthManager::new(auth_connection(&server), None)?;
    let session = auth.session_token().await?;
    assert!(!format!("{session:?}").contains("fixture-access-token"));
    auth.session_token().await?;
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    auth.invalidate_oauth_cache().await;
    auth.session_token().await?;
    assert_eq!(server.received_requests().await.unwrap().len(), 2);

    server.reset().await;
    let destination = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(307).insert_header("Location", destination.uri()))
        .mount(&server)
        .await;
    auth.invalidate_oauth_cache().await;
    assert!(auth.session_token().await.is_err());
    assert!(destination.received_requests().await.unwrap().is_empty());
    Ok(())
}

#[tokio::test]
async fn transient_http_errors_are_distinct_from_permanent_statuses() -> Result<()> {
    let server = MockServer::start().await;
    for (status, retryable, unauthorized) in [
        (429, true, false),
        (503, true, false),
        (401, false, true),
        (403, false, false),
    ] {
        server.reset().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(status))
            .mount(&server)
            .await;
        let error: anyhow::Error = reqwest::Client::new()
            .get(server.uri())
            .send()
            .await?
            .error_for_status()
            .unwrap_err()
            .into();
        assert_eq!(crate::rest::is_retryable(&error), retryable);
        assert_eq!(crate::rest::is_unauthorized(&error), unauthorized);
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let reset = tokio::spawn(async move {
        let (socket, _) = listener.accept().await?;
        drop(socket);
        Ok::<_, std::io::Error>(())
    });
    let error: anyhow::Error = reqwest::Client::new()
        .get(format!("http://{address}"))
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .unwrap_err()
        .into();
    reset.await??;
    assert!(crate::rest::is_retryable(&error), "{error:#}");
    Ok(())
}

#[tokio::test]
async fn query_errors_identify_only_schema_invalidations_and_bound_the_body() -> Result<()> {
    let server = MockServer::start().await;
    let config = auth_connection(&server);
    let session = SalesforceSessionToken {
        access_token: "fixture".into(),
        instance_url: server.uri(),
        org_id: config.expected_org_id.clone(),
    };
    let rest = SalesforceRestClient::new(config)?;
    for (status, code, invalid_field, retryable, unauthorized) in [
        (400, "INVALID_FIELD", true, false, false),
        (400, "MALFORMED_QUERY", false, false, false),
        (403, "INSUFFICIENT_ACCESS", false, false, false),
        (401, "INVALID_SESSION_ID", false, false, true),
        (503, "SERVER_UNAVAILABLE", false, true, false),
    ] {
        server.reset().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(status)
                    .set_body_json(json!([{"errorCode": code, "message": "fixture"}])),
            )
            .mount(&server)
            .await;
        let error = rest
            .full_scan(&session, &ObjectPlan::account(), 10, 1024)
            .await
            .unwrap_err();
        assert_eq!(
            error.is::<crate::rest::InvalidField>(),
            invalid_field,
            "{error:#}"
        );
        assert_eq!(crate::rest::is_retryable(&error), retryable, "{error:#}");
        assert_eq!(
            crate::rest::is_unauthorized(&error),
            unauthorized,
            "{error:#}"
        );
    }
    server.reset().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(400).set_body_json(
                json!([{"errorCode": "INVALID_FIELD", "message": "x".repeat(65536)}]),
            ),
        )
        .mount(&server)
        .await;
    let error = rest
        .full_scan(&session, &ObjectPlan::account(), 10, 1024)
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("byte limit"), "{error:#}");
    Ok(())
}

#[tokio::test]
async fn response_budget_is_enforced_on_chunked_bodies() -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await?;
        let mut socket = BufReader::new(socket);
        let mut request = String::new();
        while !request.ends_with("\r\n\r\n") {
            assert!(socket.read_line(&mut request).await? > 0);
        }
        socket.get_mut().write_all(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n8\r\n12345678\r\n8\r\n90123456\r\n0\r\n\r\n"
        ).await?;
        Ok::<_, std::io::Error>(())
    });
    let response = reqwest::Client::new()
        .get(format!("http://{address}"))
        .send()
        .await?;
    let error = crate::rest::read_json_bounded::<serde_json::Value>(response, 12)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("byte limit"), "{error:#}");
    server.await??;
    Ok(())
}

struct FakePubSub {
    observed: mpsc::UnboundedSender<proto::FetchRequest>,
    schema: String,
    payload: Vec<u8>,
    can_subscribe: bool,
}

#[tonic::async_trait]
impl PubSub for FakePubSub {
    async fn get_topic(
        &self,
        request: tonic::Request<proto::TopicRequest>,
    ) -> std::result::Result<Response<proto::TopicInfo>, Status> {
        assert_eq!(request.metadata().get("tenantid").unwrap(), "test-org");
        assert_eq!(request.metadata().get("accesstoken").unwrap(), "test-token");
        Ok(Response::new(proto::TopicInfo {
            topic_name: request.get_ref().topic_name.clone(),
            can_subscribe: self.can_subscribe,
            schema_id: "schema-1".into(),
            ..Default::default()
        }))
    }

    async fn get_schema(
        &self,
        request: tonic::Request<proto::SchemaRequest>,
    ) -> std::result::Result<Response<proto::SchemaInfo>, Status> {
        assert_eq!(request.get_ref().schema_id, "schema-1");
        Ok(Response::new(proto::SchemaInfo {
            schema_id: "schema-1".into(),
            schema_json: self.schema.clone(),
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
        let first = requests.message().await?.unwrap();
        self.observed.send(first.clone()).unwrap();
        if first.replay_id == b"expired" {
            return Err(Status::out_of_range("Replay position expired"));
        }

        let response = proto::FetchResponse {
            events: vec![proto::ConsumerEvent {
                event: Some(proto::ProducerEvent {
                    schema_id: "schema-1".into(),
                    payload: self.payload.clone(),
                    ..Default::default()
                }),
                replay_id: vec![0x80, 0x00, 0xff],
            }],
            latest_replay_id: vec![0x80, 0x00, 0xff],
            pending_num_requested: 0,
            ..Default::default()
        };

        let observed = self.observed.clone();
        let keepalives = requests.map(move |request| {
            observed
                .send(request?)
                .map_err(|_| Status::cancelled("test observer closed"))?;
            Ok(proto::FetchResponse {
                latest_replay_id: vec![0x00, 0xff, 0x01],
                pending_num_requested: 100,
                ..Default::default()
            })
        });

        Ok(Response::new(Box::pin(
            tokio_stream::once(Ok(response)).chain(keepalives),
        )))
    }
}

fn local_session(instance_url: String) -> SalesforceSessionToken {
    SalesforceSessionToken {
        access_token: "test-token".into(),
        instance_url,
        org_id: "test-org".into(),
    }
}

#[tokio::test]
async fn grpc_protocol_harness_exercises_schema_fetch_and_replay() -> Result<()> {
    let (decoder, payload) = fixture("CREATE", vec![], vec![], vec![], vec!["001000000000001AAA"]);
    let (observed, mut requests) = mpsc::unbounded_channel();
    let service = FakePubSub {
        observed,
        schema: schema_json(&decoder),
        payload,
        can_subscribe: true,
    };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(PubSubServer::new(service))
            .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async {
                let _ = shutdown_rx.await;
            })
            .await
    });

    let endpoint = format!("http://{address}");
    let client = SalesforcePubSubClient::connect(endpoint.clone()).await?;
    let session = local_session(format!("http://{address}"));
    let plan = ObjectPlan::account();
    let topic = client.get_topic(&session, &plan).await?;
    assert_eq!(topic.schema_id, "schema-1");

    let mut feed = client
        .subscribe(&session, &plan.topic(), ReplayCursor::Latest)
        .await?;
    assert_eq!(
        requests.recv().await.unwrap().replay_preset,
        proto::ReplayPreset::Latest as i32
    );

    let response = feed.next().await?;
    let event = response.events[0].event.as_ref().unwrap();
    let schema = client.get_schema(&session, &event.schema_id).await?;
    let decoded = NativeDecoder::new(&schema, &plan)?.decode(&event.payload)?;
    assert_eq!(decoded.operation, Operation::Create);
    assert_eq!(decoded.ids, vec!["001000000000001AAA"]);

    feed.replenish(response.pending_num_requested).await?;
    let replenished = requests.recv().await.unwrap();
    assert_eq!(replenished.num_requested, 100);

    let keepalive = feed.next().await?;
    assert!(keepalive.events.is_empty());
    drop(feed);

    let mut replay_feed = client
        .subscribe(
            &session,
            &plan.topic(),
            ReplayCursor::Custom(keepalive.latest_replay_id.clone()),
        )
        .await?;
    let replay_req = requests.recv().await.unwrap();
    assert_eq!(replay_req.replay_preset, proto::ReplayPreset::Custom as i32);
    assert_eq!(replay_req.replay_id, keepalive.latest_replay_id);
    drop(replay_feed);

    let error = match client
        .subscribe(
            &session,
            &plan.topic(),
            ReplayCursor::Custom(b"expired".to_vec()),
        )
        .await
    {
        Ok(_) => panic!("expired replay position should fail"),
        Err(error) => error,
    };
    assert_eq!(
        error.downcast_ref::<Status>().unwrap().code(),
        tonic::Code::OutOfRange
    );
    assert_eq!(requests.recv().await.unwrap().replay_id, b"expired");

    shutdown_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), server).await??;
    Ok(())
}

#[tokio::test]
async fn get_topic_returns_typed_topic_unavailable_error() -> Result<()> {
    let (decoder, payload) = fixture("CREATE", vec![], vec![], vec![], vec!["001000000000001AAA"]);
    let (observed, _requests) = mpsc::unbounded_channel();
    let service = FakePubSub {
        observed,
        schema: schema_json(&decoder),
        payload,
        can_subscribe: false,
    };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(PubSubServer::new(service))
            .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async {
                let _ = shutdown_rx.await;
            })
            .await
    });

    let endpoint = format!("http://{address}");
    let client = SalesforcePubSubClient::connect(endpoint).await?;
    let session = local_session(format!("http://{address}"));
    let plan = ObjectPlan::account();
    let error = client
        .get_topic(&session, &plan)
        .await
        .expect_err("non-subscribable topic should return typed error");
    let topic_unavailable = error
        .downcast_ref::<crate::pubsub::TopicUnavailable>()
        .expect("expected TopicUnavailable");
    assert_eq!(topic_unavailable.topic_name, plan.topic());

    shutdown_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), server).await??;
    Ok(())
}

#[tokio::test]
async fn rest_protocol_harness_validates_same_origin_pagination() -> Result<()> {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/services/data/v68.0/query"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "records": [],
            "done": false,
            "nextRecordsUrl": "/services/data/v68.0/query/page-2"
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/services/data/v68.0/query/page-2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "records": [{
                "Id": "001000000000001AAA",
                "Name": "Example Account",
                "Phone": null,
                "Description": null,
                "LastModifiedDate": "2023-11-14T22:13:20.000+0000"
            }],
            "done": true
        })))
        .mount(&server)
        .await;

    let rest = SalesforceRestClient::new(SalesforceConnectionConfig {
        instance_url: server.uri(),
        expected_org_id: "test-org".into(),
        api_version: "68.0".into(),
        auth: SalesforceAuthConfig::ClientCredentials {
            token_url: format!("{}/oauth/token", server.uri()),
            client_id: "id".into(),
            client_secret: "secret".into(),
        },
    })?;

    let rows = rest
        .full_scan(
            &local_session(server.uri()),
            &ObjectPlan::account(),
            10,
            512 * 1024,
        )
        .await?;
    assert_eq!(rows.len(), 1);

    server.reset().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "records": [],
            "done": false,
            "nextRecordsUrl": "https://example.invalid/steal"
        })))
        .mount(&server)
        .await;

    let error = rest
        .full_scan(
            &local_session(server.uri()),
            &ObjectPlan::account(),
            10,
            512 * 1024,
        )
        .await
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("unsafe continuation"),
        "unexpected error: {error:#}"
    );

    Ok(())
}
