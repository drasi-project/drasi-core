// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{sync::Arc, time::Duration};

use anyhow::{bail, ensure, Context, Result};
use drasi_lib::identity::{CredentialContext, Credentials, IdentityProvider};
use tonic::{
    metadata::{Ascii, MetadataValue},
    transport::{Channel, ClientTlsConfig, Endpoint},
    Code, Request,
};

use crate::{
    config::{Authentication, DurableTaskReactionConfig},
    proto::{self, task_hub_sidecar_service_client::TaskHubSidecarServiceClient},
};

mod executor;
pub(crate) use executor::DtsClient;

#[async_trait::async_trait]
pub(super) trait DtsApi: Send {
    async fn start(
        &mut self,
        request: proto::CreateInstanceRequest,
    ) -> Result<proto::CreateInstanceResponse>;
    async fn get(&mut self, instance_id: &str) -> Result<Option<proto::OrchestrationState>>;
    async fn raise_event(&mut self, request: proto::RaiseEventRequest) -> Result<()>;
    async fn terminate(&mut self, request: proto::TerminateRequest) -> Result<()>;
}

#[derive(Clone)]
pub(crate) struct GrpcDtsApi {
    inner: TaskHubSidecarServiceClient<Channel>,
    hub: MetadataValue<Ascii>,
    provider: Option<Arc<dyn IdentityProvider>>,
    scope: String,
    timeout: Duration,
}

impl GrpcDtsApi {
    pub(crate) fn new(
        config: &DurableTaskReactionConfig,
        provider: Option<Arc<dyn IdentityProvider>>,
    ) -> Result<Self> {
        let timeout = Duration::from_millis(config.timeout_ms);
        let mut endpoint = Endpoint::from_shared(config.endpoint.clone())?
            .connect_timeout(timeout)
            .timeout(timeout);
        let provider = match config.authentication {
            Authentication::None => None,
            Authentication::Identity => {
                ensure!(
                    provider.is_some(),
                    "identity authentication requires an IdentityProvider"
                );
                endpoint = endpoint.tls_config(ClientTlsConfig::new().with_native_roots())?;
                provider
            }
        };
        Ok(Self {
            inner: TaskHubSidecarServiceClient::new(endpoint.connect_lazy()),
            hub: config.task_hub.parse()?,
            provider,
            scope: config.scope.clone(),
            timeout,
        })
    }

    pub(crate) async fn request<T>(&self, body: T) -> Result<Request<T>> {
        let mut request = Request::new(body);
        request.metadata_mut().insert("taskhub", self.hub.clone());
        request.metadata_mut().insert(
            "x-user-agent",
            MetadataValue::from_static(concat!(
                "drasi-reaction-durable-task/",
                env!("CARGO_PKG_VERSION")
            )),
        );
        if let Some(provider) = &self.provider {
            let context = CredentialContext::new().with_property("scope", &self.scope);
            let credentials =
                tokio::time::timeout(self.timeout, provider.get_credentials(&context))
                    .await
                    .context("DTS credential acquisition timed out")?
                    .context("DTS credential acquisition failed")?;
            let Credentials::Token { token, .. } = credentials else {
                bail!("DTS identity provider must return token credentials");
            };
            ensure!(
                !token.is_empty(),
                "DTS identity provider returned an empty token"
            );
            let mut authorization: MetadataValue<Ascii> = format!("Bearer {token}")
                .parse()
                .context("DTS identity provider returned invalid bearer metadata")?;
            authorization.set_sensitive(true);
            request
                .metadata_mut()
                .insert("authorization", authorization);
        }
        request.set_timeout(self.timeout);
        Ok(request)
    }
}

#[async_trait::async_trait]
impl DtsApi for GrpcDtsApi {
    async fn start(
        &mut self,
        body: proto::CreateInstanceRequest,
    ) -> Result<proto::CreateInstanceResponse> {
        let request = self.request(body).await?;
        Ok(self.inner.start_instance(request).await?.into_inner())
    }

    async fn get(&mut self, instance_id: &str) -> Result<Option<proto::OrchestrationState>> {
        let request = self
            .request(proto::GetInstanceRequest {
                instance_id: instance_id.to_owned(),
                get_inputs_and_outputs: false,
            })
            .await?;
        let response = match self.inner.get_instance(request).await {
            Ok(response) => response.into_inner(),
            Err(error) if error.code() == Code::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if !response.exists {
            return Ok(None);
        }
        Ok(Some(
            response
                .orchestration_state
                .context("DTS returned exists=true without state")?,
        ))
    }

    async fn raise_event(&mut self, body: proto::RaiseEventRequest) -> Result<()> {
        let request = self.request(body).await?;
        self.inner.raise_event(request).await?;
        Ok(())
    }

    async fn terminate(&mut self, body: proto::TerminateRequest) -> Result<()> {
        let request = self.request(body).await?;
        self.inner.terminate_instance(request).await?;
        Ok(())
    }
}
