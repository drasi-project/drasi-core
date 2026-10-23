use anyhow::{ensure, Context, Result};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};
use tonic::Request;

use crate::auth::SalesforceSessionToken;
use crate::projection::ObjectPlan;
use crate::proto::{self, pub_sub_client::PubSubClient, FetchRequest, FetchResponse, ReplayPreset};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayCursor {
    Latest,
    Earliest,
    Custom(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopicUnavailable {
    pub topic_name: String,
}

impl std::fmt::Display for TopicUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Salesforce topic '{}' is unavailable for Subscribe",
            self.topic_name
        )
    }
}

impl std::error::Error for TopicUnavailable {}

#[derive(Clone)]
pub struct SalesforcePubSubClient {
    endpoint: String,
    grpc: PubSubClient<Channel>,
}

pub struct Feed {
    requests: mpsc::Sender<FetchRequest>,
    responses: tonic::Streaming<FetchResponse>,
}

impl Feed {
    pub async fn next(&mut self) -> Result<FetchResponse> {
        self.responses.message().await?.ok_or_else(|| {
            tonic::Status::unavailable("Salesforce closed the Pub/Sub subscription stream").into()
        })
    }

    pub async fn replenish(&self, pending: i32) -> Result<()> {
        if pending == 0 {
            self.requests
                .send(FetchRequest {
                    num_requested: 100,
                    ..Default::default()
                })
                .await
                .map_err(|_| {
                    tonic::Status::unavailable("Salesforce Pub/Sub request channel closed")
                })?;
        }
        Ok(())
    }
}

impl SalesforcePubSubClient {
    pub async fn connect(endpoint: impl Into<String>) -> Result<Self> {
        let endpoint = endpoint.into();
        let mut builder = Endpoint::from_shared(endpoint.clone())
            .context("invalid Salesforce Pub/Sub endpoint URL")?
            .connect_timeout(std::time::Duration::from_secs(20));

        if endpoint.starts_with("https://") {
            builder = builder
                .tls_config(ClientTlsConfig::new().with_native_roots())
                .context("failed to configure TLS for Salesforce Pub/Sub client")?;
        }

        let channel = builder
            .connect()
            .await
            .context("failed to connect to Salesforce Pub/Sub endpoint")?;

        Ok(Self {
            endpoint,
            grpc: PubSubClient::new(channel),
        })
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub async fn get_topic(
        &self,
        session: &SalesforceSessionToken,
        plan: &ObjectPlan,
    ) -> Result<proto::TopicInfo> {
        let mut grpc = self.grpc.clone();
        let mut request = self.request(
            session,
            proto::TopicRequest {
                topic_name: plan.topic(),
            },
        )?;
        request.set_timeout(std::time::Duration::from_secs(20));
        let topic = grpc
            .get_topic(request)
            .await
            .map_err(|status| rpc_error("GetTopic", status))?
            .into_inner();
        if !topic.can_subscribe {
            return Err(anyhow::Error::new(TopicUnavailable {
                topic_name: plan.topic(),
            }));
        }
        Ok(topic)
    }

    pub async fn get_schema(
        &self,
        session: &SalesforceSessionToken,
        schema_id: &str,
    ) -> Result<String> {
        let mut grpc = self.grpc.clone();
        let mut request = self.request(
            session,
            proto::SchemaRequest {
                schema_id: schema_id.to_owned(),
            },
        )?;
        request.set_timeout(std::time::Duration::from_secs(20));
        let response = grpc
            .get_schema(request)
            .await
            .map_err(|status| rpc_error("GetSchema", status))?
            .into_inner();
        ensure!(
            response.schema_id == schema_id,
            "Salesforce returned a different schema ID than requested"
        );
        Ok(response.schema_json)
    }

    pub async fn subscribe(
        &self,
        session: &SalesforceSessionToken,
        topic_name: &str,
        replay: ReplayCursor,
    ) -> Result<Feed> {
        let mut grpc = self.grpc.clone();
        let (requests_tx, requests_rx) = mpsc::channel(4);

        requests_tx
            .send(fetch_request(topic_name, replay))
            .await
            .context("failed to enqueue initial Salesforce fetch request")?;

        let stream = ReceiverStream::new(requests_rx);
        let response = grpc
            .subscribe(self.request(session, stream)?)
            .await
            .map_err(|status| rpc_error("Subscribe", status))?
            .into_inner();

        Ok(Feed {
            requests: requests_tx,
            responses: response,
        })
    }

    fn request<T>(&self, session: &SalesforceSessionToken, body: T) -> Result<Request<T>> {
        let mut request = Request::new(body);
        request.metadata_mut().insert(
            "accesstoken",
            session
                .access_token
                .parse()
                .context("invalid OAuth access token metadata value")?,
        );
        request.metadata_mut().insert(
            "instanceurl",
            session
                .instance_url
                .parse()
                .context("invalid Salesforce instance URL metadata value")?,
        );
        request.metadata_mut().insert(
            "tenantid",
            session
                .org_id
                .parse()
                .context("invalid Salesforce tenant/org metadata value")?,
        );
        Ok(request)
    }
}

fn fetch_request(topic: &str, replay: ReplayCursor) -> FetchRequest {
    match replay {
        ReplayCursor::Latest => FetchRequest {
            topic_name: topic.to_owned(),
            replay_preset: ReplayPreset::Latest as i32,
            num_requested: 100,
            ..Default::default()
        },
        ReplayCursor::Earliest => FetchRequest {
            topic_name: topic.to_owned(),
            replay_preset: ReplayPreset::Earliest as i32,
            num_requested: 100,
            ..Default::default()
        },
        ReplayCursor::Custom(replay_id) => FetchRequest {
            topic_name: topic.to_owned(),
            replay_preset: ReplayPreset::Custom as i32,
            replay_id,
            num_requested: 100,
            ..Default::default()
        },
    }
}

fn rpc_error(operation: &str, status: tonic::Status) -> anyhow::Error {
    let detail = match status.metadata().get("error-code") {
        Some(code) => format!("Salesforce Pub/Sub {operation} failed ({code:?})"),
        None => format!("Salesforce Pub/Sub {operation} failed"),
    };
    anyhow::Error::new(status).context(detail)
}
