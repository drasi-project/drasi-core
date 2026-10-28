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

pub(super) fn auth_connection(server: &MockServer) -> SalesforceConnectionConfig {
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
async fn query_error_body_failures_preserve_http_status() -> Result<()> {
    let server = MockServer::start().await;
    let config = auth_connection(&server);
    let session = SalesforceSessionToken {
        access_token: "fixture".into(),
        instance_url: server.uri(),
        org_id: config.expected_org_id.clone(),
    };
    let rest = SalesforceRestClient::new(config)?;
    for (body, diagnostic) in [
        ("not JSON".to_owned(), "decode"),
        ("{}".to_owned(), "decode"),
        ("[{}]".to_owned(), "decode"),
        ("x".repeat(64 * 1024 + 1), "byte limit"),
    ] {
        server.reset().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(400).set_body_string(body))
            .mount(&server)
            .await;
        let error = rest
            .full_scan(&session, &ObjectPlan::account(), 10, 1024)
            .await
            .unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<reqwest::Error>()
                .and_then(reqwest::Error::status),
            Some(reqwest::StatusCode::BAD_REQUEST),
            "{error:#}"
        );
        assert!(format!("{error:#}").contains(diagnostic), "{error:#}");
        assert!(!crate::rest::is_retryable(&error));
        assert!(!crate::rest::is_unauthorized(&error));
        assert!(!error.is::<crate::rest::InvalidField>());
    }
    Ok(())
}

#[tokio::test]
async fn continuation_paths_reject_encoded_traversal_and_non_path_components() -> Result<()> {
    let server = MockServer::start().await;
    let config = auth_connection(&server);
    let session = SalesforceSessionToken {
        access_token: "fixture".into(),
        instance_url: server.uri(),
        org_id: config.expected_org_id.clone(),
    };
    let rest = SalesforceRestClient::new(config)?;
    for continuation in [
        "/services/data/%2e%2e/%2E%2E/oauth/token",
        "/services/data/v68.0/query%2fpage-2",
        "/services/data/v68.0/query%5Cpage-2",
        "/services/data/%252e%252e/oauth/token",
        "/services/data/v68.0/query\\page-2",
        "/services/data/v68.0/query/page-2?extra=value",
        "/services/data/v68.0/query/page-2#fragment",
        "/services/data/v68.0/query/\npage-2",
        "/services/data/v68.0/query/../page-2",
        "//example.invalid/services/data/v68.0/query/page-2",
        "https://example.invalid/services/data/v68.0/query/page-2",
    ] {
        server.reset().await;
        Mock::given(method("GET"))
            .and(path("/services/data/v68.0/query"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "records": [],
                "done": false,
                "nextRecordsUrl": continuation
            })))
            .mount(&server)
            .await;
        let error = rest
            .full_scan(&session, &ObjectPlan::account(), 10, 1024)
            .await
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("unsafe continuation path"),
            "{continuation}: {error:#}"
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
    Ok(())
}

#[tokio::test]
async fn id_fetch_budget_spans_batches_and_pages() -> Result<()> {
    let server = MockServer::start().await;
    let config = auth_connection(&server);
    let session = SalesforceSessionToken {
        access_token: "fixture".into(),
        instance_url: server.uri(),
        org_id: config.expected_org_id.clone(),
    };
    let rest = SalesforceRestClient::new(config)?;
    let ids: Vec<_> = (0..101).map(|index| format!("001{index:012}AAA")).collect();
    let record = |index: usize| {
        json!({
            "Id": ids[index],
            "Name": "x".repeat(256),
            "Phone": null,
            "Description": null,
            "LastModifiedDate": "2023-11-14T22:13:20.000+0000"
        })
    };
    let pages = [
        json!({
            "records": [record(0)],
            "done": false,
            "nextRecordsUrl": "/services/data/v68.0/query/page-2"
        }),
        json!({"records": [record(1)], "done": true}),
        json!({"records": [record(100)], "done": true}),
    ]
    .map(|page| serde_json::to_vec(&page).unwrap());
    for (index, page) in [(0, &pages[0]), (100, &pages[2])] {
        let quoted_id = format!("'{}'", ids[index]);
        Mock::given(method("GET"))
            .and(path("/services/data/v68.0/query"))
            .and(move |request: &wiremock::Request| {
                request
                    .url
                    .query_pairs()
                    .any(|(name, value)| name == "q" && value.contains(&quoted_id))
            })
            .respond_with(ResponseTemplate::new(200).set_body_bytes(page.clone()))
            .mount(&server)
            .await;
    }
    Mock::given(method("GET"))
        .and(path("/services/data/v68.0/query/page-2"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(pages[1].clone()))
        .mount(&server)
        .await;

    let total_bytes: usize = pages.iter().map(Vec::len).sum();
    let error = rest
        .fetch_records_by_ids(&session, &ObjectPlan::account(), &ids, total_bytes - 1)
        .await
        .expect_err("all batches and pages must share one response-byte budget");
    assert!(format!("{error:#}").contains("byte limit"), "{error:#}");
    assert_eq!(server.received_requests().await.unwrap().len(), 3);

    let rows = rest
        .fetch_records_by_ids(&session, &ObjectPlan::account(), &ids, total_bytes)
        .await?;
    assert_eq!(
        rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
        vec![ids[0].as_str(), ids[1].as_str(), ids[100].as_str()]
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 6);
    Ok(())
}

#[tokio::test]
async fn full_scan_enforces_cumulative_limits_and_preserves_later_page_errors() -> Result<()> {
    let server = MockServer::start().await;
    let config = auth_connection(&server);
    let session = SalesforceSessionToken {
        access_token: "fixture".into(),
        instance_url: server.uri(),
        org_id: config.expected_org_id.clone(),
    };
    let rest = SalesforceRestClient::new(config)?;
    let record = |id: &str| {
        json!({
            "Id": id,
            "Name": "x".repeat(256),
            "Phone": null,
            "Description": null,
            "LastModifiedDate": "2023-11-14T22:13:20.000+0000"
        })
    };
    let pages = [
        json!({
            "records": [record("001000000000001AAA")],
            "done": false,
            "nextRecordsUrl": "/services/data/v68.0/query/page-2"
        }),
        json!({"records": [record("001000000000002AAA")], "done": true}),
    ]
    .map(|page| serde_json::to_vec(&page).unwrap());
    let total_bytes = pages.iter().map(Vec::len).sum::<usize>();
    for (page, page_path) in [
        (&pages[0], "/services/data/v68.0/query"),
        (&pages[1], "/services/data/v68.0/query/page-2"),
    ] {
        Mock::given(method("GET"))
            .and(path(page_path))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(page.clone()))
            .mount(&server)
            .await;
    }
    let error = rest
        .full_scan(&session, &ObjectPlan::account(), 2, total_bytes - 1)
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("byte limit"), "{error:#}");
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
    let rows = rest
        .full_scan(&session, &ObjectPlan::account(), 2, total_bytes)
        .await?;
    assert_eq!(
        rows.iter()
            .map(|record| record.id.as_str())
            .collect::<Vec<_>>(),
        ["001000000000001AAA", "001000000000002AAA"]
    );
    let error = rest
        .full_scan(&session, &ObjectPlan::account(), 1, total_bytes)
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("row limit"), "{error:#}");

    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/services/data/v68.0/query"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(pages[0].clone()))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/services/data/v68.0/query/page-2"))
        .respond_with(
            ResponseTemplate::new(401).set_body_json(json!([{"errorCode": "INVALID_SESSION_ID"}])),
        )
        .mount(&server)
        .await;
    let error = rest
        .full_scan(&session, &ObjectPlan::account(), 2, total_bytes)
        .await
        .unwrap_err();
    assert!(crate::rest::is_unauthorized(&error), "{error:#}");
    assert!(!crate::rest::is_retryable(&error));
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
    Ok(())
}

#[tokio::test]
async fn full_scan_accepts_an_empty_complete_result() -> Result<()> {
    let server = MockServer::start().await;
    let config = auth_connection(&server);
    let session = SalesforceSessionToken {
        access_token: "fixture".into(),
        instance_url: server.uri(),
        org_id: config.expected_org_id.clone(),
    };
    let rest = SalesforceRestClient::new(config)?;
    let body = serde_json::to_vec(&json!({"records": [], "done": true}))?;
    let body_len = body.len();
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
        .expect(1)
        .mount(&server)
        .await;
    assert!(rest
        .full_scan(&session, &ObjectPlan::account(), 0, body_len)
        .await?
        .is_empty());
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

#[derive(Clone, Copy)]
enum SubscriptionEnd {
    KeepAlive,
    Close,
    Fail,
}

struct FakePubSub {
    observed: mpsc::UnboundedSender<proto::FetchRequest>,
    schema: String,
    payload: Vec<u8>,
    can_subscribe: bool,
    subscription_end: SubscriptionEnd,
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

        match self.subscription_end {
            SubscriptionEnd::Close => {
                return Ok(Response::new(Box::pin(tokio_stream::once(Ok(response)))));
            }
            SubscriptionEnd::Fail => {
                let failure = requests.take(1).map(|request| {
                    request?;
                    Err(Status::data_loss("fixture stream failure"))
                });
                return Ok(Response::new(Box::pin(
                    tokio_stream::once(Ok(response)).chain(failure),
                )));
            }
            SubscriptionEnd::KeepAlive => {}
        }

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
async fn pubsub_rejects_remote_http_before_connecting() -> Result<()> {
    let result = tokio::time::timeout(
        Duration::from_millis(100),
        SalesforcePubSubClient::connect("http://192.0.2.1:7443"),
    )
    .await
    .expect("unsafe transport must be rejected before attempting a connection");
    let error = result.err().expect("remote HTTP must be rejected");
    assert!(error.to_string().contains("HTTPS"), "{error:#}");
    Ok(())
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
        subscription_end: SubscriptionEnd::KeepAlive,
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
        subscription_end: SubscriptionEnd::KeepAlive,
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
async fn grpc_stream_failures_closure_and_replenish_preserve_typed_errors() -> Result<()> {
    for (ending, expected) in [
        (SubscriptionEnd::Close, tonic::Code::Unavailable),
        (SubscriptionEnd::Fail, tonic::Code::DataLoss),
    ] {
        let (decoder, payload) =
            fixture("CREATE", vec![], vec![], vec![], vec!["001000000000001AAA"]);
        let (observed, _requests) = mpsc::unbounded_channel();
        let service = FakePubSub {
            observed,
            schema: schema_json(&decoder),
            payload,
            can_subscribe: true,
            subscription_end: ending,
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
        let mut feed = client
            .subscribe(
                &local_session(endpoint),
                &ObjectPlan::account().topic(),
                ReplayCursor::Latest,
            )
            .await?;
        assert_eq!(feed.next().await?.events.len(), 1);
        if matches!(ending, SubscriptionEnd::Fail) {
            feed.replenish(0).await?;
        }
        let error = feed.next().await.unwrap_err();
        assert_eq!(error.downcast_ref::<Status>().unwrap().code(), expected);
        let error = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Err(error) = feed.replenish(0).await {
                    break error;
                }
                tokio::task::yield_now().await;
            }
        })
        .await?;
        assert_eq!(
            error.downcast_ref::<Status>().unwrap().code(),
            tonic::Code::Unavailable
        );
        drop(feed);
        shutdown_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), server).await??;
    }
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
