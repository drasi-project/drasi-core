// Copyright 2026 The Drasi Authors.
// SPDX-License-Identifier: Apache-2.0

use anyhow::{bail, ensure, Context, Result};
use drasi_identity_azure::AzureIdentityProvider;
use drasi_lib::identity::{CredentialContext, Credentials, IdentityProvider};
use serde_json::{json, Value};

#[derive(Debug)]
pub struct SubmissionFailure {
    pub uncertain: bool,
    pub attempts: u8,
    pub message: String,
}

impl std::fmt::Display for SubmissionFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}
impl std::error::Error for SubmissionFailure {}

pub struct Foundry {
    pub offline: bool,
    pub project: Option<reqwest::Url>,
    client: reqwest::Client,
    identity: std::sync::Arc<dyn IdentityProvider>,
}

impl Foundry {
    pub fn new(offline: bool) -> Result<Self> {
        let endpoint = std::env::var("FOUNDRY_PROJECT_ENDPOINT").ok();
        let project = Self::parse_project(endpoint.as_deref())?;
        Ok(Self {
            offline,
            project,
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            identity: std::sync::Arc::new(
                AzureIdentityProvider::with_default_credentials("local-developer")?
                    .with_scope("https://ai.azure.com/.default"),
            ),
        })
    }

    fn parse_project(endpoint: Option<&str>) -> Result<Option<reqwest::Url>> {
        let project = endpoint
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(reqwest::Url::parse)
            .transpose()
            .context("FOUNDRY_PROJECT_ENDPOINT must be a Foundry project API endpoint")?;
        if let Some(url) = &project {
            ensure!(
                url.scheme() == "https"
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none()
                    && url
                        .host_str()
                        .is_some_and(|host| host.ends_with(".services.ai.azure.com"))
                    && url.path().strip_prefix("/api/projects/").is_some_and(|project| !project.trim_matches('/').is_empty() && !project.trim_matches('/').contains('/')),
                "FOUNDRY_PROJECT_ENDPOINT must be https://<account>.services.ai.azure.com/api/projects/<project>, without credentials, query or fragment"
            );
        }
        Ok(project)
    }

    fn endpoint(&self, segments: &[&str]) -> Result<reqwest::Url> {
        let mut url = self
            .project
            .clone()
            .context("Set FOUNDRY_PROJECT_ENDPOINT in examples/lib/foundry-routines/.env and restart. Authentication uses your existing Azure developer sign-in; do not supply a token or API key.")?;
        url.path_segments_mut()
            .map_err(|_| anyhow::anyhow!("Invalid project endpoint"))?
            .pop_if_empty()
            .extend(segments);
        url.query_pairs_mut().append_pair("api-version", "v1");
        Ok(url)
    }

    async fn token(&self) -> Result<String> {
        match self
            .identity
            .get_credentials(&CredentialContext::new())
            .await?
        {
            Credentials::Token { token, .. } => Ok(token),
            _ => bail!("Azure identity did not return a bearer token"),
        }
    }

    pub async fn ready(&self) -> Result<()> {
        if !self.offline {
            self.endpoint(&["agents"])?;
            self.token().await?;
        }
        Ok(())
    }

    async fn body(mut response: reqwest::Response) -> Result<Value> {
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            ensure!(
                bytes.len() + chunk.len() <= 1024 * 1024,
                "Foundry response exceeds 1 MiB"
            );
            bytes.extend_from_slice(&chunk);
        }
        Ok(serde_json::from_slice(&bytes)?)
    }

    async fn error_message(response: reqwest::Response, operation: &str) -> String {
        let status = response.status();
        let prefix = format!("{operation} returned HTTP {status}");
        match Self::body(response).await {
            Ok(body) => {
                let error = body.get("error").unwrap_or(&body);
                let code = error
                    .get("code")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown_error");
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("Foundry did not return an error message");
                let detail: String = message.chars().take(4096).collect();
                format!("{prefix} [{code}]: {detail}")
            }
            Err(error) => format!("{prefix}; could not read JSON error details: {error}"),
        }
    }

    async fn create_conversation(&self, agent: &str, token: &str) -> Result<String> {
        let response = self
            .client
            .post(self.endpoint(&[
                "agents",
                agent,
                "endpoint",
                "protocols",
                "openai",
                "conversations",
            ])?)
            .bearer_auth(token)
            .json(&json!({}))
            .send()
            .await
            .context("Foundry conversation setup failed; the agent was not invoked")?;
        if !response.status().is_success() {
            bail!(
                "{}",
                Self::error_message(response, "Foundry conversation creation").await
            );
        }
        let body = Self::body(response).await?;
        body.get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(String::from)
            .context("Foundry conversation creation returned no identifier")
    }

    pub async fn agents(&self) -> Result<Value> {
        if self.offline {
            return Ok(json!([{"name":"offline-test-agent","kind":"prompt","offline":true}]));
        }
        let response = self
            .client
            .get(self.endpoint(&["agents"])?)
            .bearer_auth(self.token().await?)
            .send()
            .await?;
        if !response.status().is_success() {
            bail!(
                "{}",
                Self::error_message(response, "Foundry agent listing").await
            );
        }
        let body = Self::body(response).await?;
        let agents = body
            .get("data")
            .or_else(|| body.get("value"))
            .and_then(Value::as_array)
            .context("Unexpected Foundry agent-list response")?;
        Ok(Value::Array(
            agents
                .iter()
                .filter_map(|a| {
                    let kind = a
                        .pointer("/definition/kind")
                        .or_else(|| a.pointer("/versions/latest/definition/kind"))
                        .and_then(Value::as_str)?;
                    if kind != "prompt" {
                        return None;
                    }
                    a.get("name").map(|name| json!({"name":name,"kind":kind}))
                })
                .collect(),
        ))
    }

    pub async fn invoke(
        &self,
        agent: &str,
        input: &Value,
    ) -> Result<(Value, u8), SubmissionFailure> {
        if self.offline {
            return Ok((
                json!({"id":format!("offline-{}",uuid::Uuid::new_v4()),"status":"completed","output":[{"content":[{"type":"output_text","text":format!("OFFLINE TEST TRANSPORT - received structured event: {}", input["event"])}]}]}),
                1,
            ));
        }
        let url = self
            .endpoint(&[
                "agents",
                agent,
                "endpoint",
                "protocols",
                "openai",
                "responses",
            ])
            .map_err(|e| SubmissionFailure {
                uncertain: false,
                attempts: 0,
                message: e.to_string(),
            })?;
        let text = serde_json::to_string(input).map_err(|e| SubmissionFailure {
            uncertain: false,
            attempts: 0,
            message: e.to_string(),
        })?;
        let mut token = self.token().await.map_err(|e| SubmissionFailure {
            uncertain: false,
            attempts: 0,
            message: e.to_string(),
        })?;
        let conversation =
            self.create_conversation(agent, &token)
                .await
                .map_err(|e| SubmissionFailure {
                    uncertain: false,
                    attempts: 0,
                    message: format!("{e:#}"),
                })?;
        let mut refreshed = false;
        for attempt in 1..=5u8 {
            let response = self
                .client
                .post(url.clone())
                .bearer_auth(&token)
                .json(&json!({"conversation":conversation,"input":[{"role":"user","content":text}],"background":true}))
                .send()
                .await;
            let response = match response {
                Ok(response) => response,
                Err(error) if error.is_connect() && attempt < 5 => {
                    log::warn!("Pre-acceptance connection failure; retrying attempt {attempt}");
                    tokio::time::sleep(std::time::Duration::from_secs(1u64 << (attempt - 1))).await;
                    continue;
                }
                Err(error) => {
                    return Err(SubmissionFailure {
                        uncertain: !error.is_connect(),
                        attempts: attempt,
                        message: error.to_string(),
                    })
                }
            };
            if response.status() == reqwest::StatusCode::UNAUTHORIZED && !refreshed && attempt < 5 {
                refreshed = true;
                let identity = AzureIdentityProvider::with_default_credentials("local-developer")
                    .map_err(|e| SubmissionFailure {
                        uncertain: false,
                        attempts: attempt,
                        message: e.to_string(),
                    })?
                    .with_scope("https://ai.azure.com/.default");
                let credentials = identity
                    .get_credentials(&CredentialContext::new())
                    .await
                    .map_err(|e| SubmissionFailure {
                        uncertain: false,
                        attempts: attempt,
                        message: e.to_string(),
                    })?;
                let Credentials::Token { token: renewed, .. } = credentials else {
                    return Err(SubmissionFailure {
                        uncertain: false,
                        attempts: attempt,
                        message: "Azure identity did not return a token".into(),
                    });
                };
                token = renewed;
                continue;
            }
            if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS && attempt < 5 {
                let delay = response
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(1u64 << (attempt - 1))
                    .min(30);
                log::warn!("Foundry throttled; retrying attempt {attempt} in {delay}s");
                tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
                continue;
            }
            if !response.status().is_success() {
                let uncertain = response.status().is_server_error();
                return Err(SubmissionFailure {
                    uncertain,
                    attempts: attempt,
                    message: Self::error_message(response, "Foundry submission").await,
                });
            }
            let body = Self::body(response).await.map_err(|e| SubmissionFailure {
                uncertain: true,
                attempts: attempt,
                message: format!("Unreadable response after submission: {e}"),
            })?;
            if body["id"].as_str().is_none_or(|id| id.is_empty()) {
                return Err(SubmissionFailure {
                    uncertain: true,
                    attempts: attempt,
                    message: "Accepted response has no identifier".into(),
                });
            }
            return Ok((body, attempt));
        }
        unreachable!("The final attempt always returns a response or an error")
    }

    pub async fn response(&self, agent: &str, id: &str) -> Result<Value> {
        let response = self
            .client
            .get(self.endpoint(&[
                "agents",
                agent,
                "endpoint",
                "protocols",
                "openai",
                "responses",
                id,
            ])?)
            .bearer_auth(self.token().await?)
            .send()
            .await?;
        if !response.status().is_success() {
            bail!(
                "{}",
                Self::error_message(response, "Foundry response lookup").await
            );
        }
        Self::body(response).await
    }

    pub fn status(&self) -> Value {
        json!({"mode":if self.offline {"offline"} else {"live"},"project":self.project.as_ref().map(|p|p.as_str()),"configured":self.offline || self.project.is_some(),"identity":"Local backend developer identity","nativeRoutine":false})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        extract::State,
        http::{HeaderMap, StatusCode},
        response::IntoResponse,
        routing::post,
        Json, Router,
    };
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    #[test]
    fn blank_config_is_unconfigured_and_only_project_api_endpoints_are_valid() -> Result<()> {
        assert!(Foundry::parse_project(Some("  "))?.is_none());
        assert!(Foundry::parse_project(None)?.is_none());
        let endpoint = "https://example.services.ai.azure.com/api/projects/demo";
        assert_eq!(
            Foundry::parse_project(Some(&format!(" {endpoint} ")))?
                .as_ref()
                .map(reqwest::Url::as_str),
            Some(endpoint)
        );
        assert!(Foundry::parse_project(Some(
            "https://example.services.ai.azure.com/api/projects/"
        ))
        .is_err());
        assert!(Foundry::parse_project(Some("https://ai.azure.com/nextgen/project")).is_err());
        Ok(())
    }

    #[derive(Clone)]
    struct TestIdentity;
    #[async_trait::async_trait]
    impl IdentityProvider for TestIdentity {
        async fn get_credentials(&self, _: &CredentialContext) -> Result<Credentials> {
            Ok(Credentials::Token {
                username: "test".into(),
                token: "non-secret-test-token".into(),
            })
        }
        fn clone_box(&self) -> Box<dyn IdentityProvider> {
            Box::new(self.clone())
        }
    }
    #[derive(Clone, Copy)]
    enum Behavior {
        Success,
        Throttle,
        Malformed,
        RejectConversation,
        RejectSubmission,
    }

    #[derive(Clone)]
    struct Reply {
        calls: Arc<AtomicUsize>,
        conversations: Arc<AtomicUsize>,
        behavior: Behavior,
    }
    async fn conversation(State(state): State<Reply>) -> impl IntoResponse {
        let count = state.conversations.fetch_add(1, Ordering::SeqCst) + 1;
        if matches!(state.behavior, Behavior::RejectConversation) {
            return (
                StatusCode::FORBIDDEN,
                Json(
                    json!({"error":{"code":"permission_denied","message":"Conversation creation denied"}}),
                ),
            );
        }
        (StatusCode::OK, Json(json!({"id":format!("conv-{count}")})))
    }
    async fn reply(State(state): State<Reply>, Json(body): Json<Value>) -> impl IntoResponse {
        let count = state.calls.fetch_add(1, Ordering::SeqCst);
        assert!(body["input"][0]["content"].as_str().is_some());
        assert_eq!(
            body["conversation"],
            format!("conv-{}", state.conversations.load(Ordering::SeqCst))
        );
        if matches!(state.behavior, Behavior::Throttle) && count == 0 {
            let mut headers = HeaderMap::new();
            headers.insert("retry-after", "0".parse().unwrap());
            return (
                StatusCode::TOO_MANY_REQUESTS,
                headers,
                Json(json!({"error":"throttled"})),
            );
        }
        if matches!(state.behavior, Behavior::RejectSubmission) {
            return (
                StatusCode::BAD_REQUEST,
                HeaderMap::new(),
                Json(
                    json!({"error":{"code":"bad_request","message":"The conversation parameter is required"}}),
                ),
            );
        }
        (
            StatusCode::OK,
            HeaderMap::new(),
            Json(if matches!(state.behavior, Behavior::Malformed) {
                json!({"status":"completed"})
            } else {
                json!({"id":"response-id","status":"completed"})
            }),
        )
    }

    struct Exercise {
        outcomes: Vec<Result<(Value, u8), SubmissionFailure>>,
        submissions: usize,
        conversations: usize,
    }

    async fn exercise(behavior: Behavior, invocations: usize) -> Result<Exercise> {
        let calls = Arc::new(AtomicUsize::new(0));
        let conversations = Arc::new(AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("http://{}/api/projects/test", listener.local_addr()?);
        let router = Router::new()
            .route(
                "/api/projects/test/agents/{agent}/endpoint/protocols/openai/conversations",
                post(conversation),
            )
            .route(
                "/api/projects/test/agents/{agent}/endpoint/protocols/openai/responses",
                post(reply),
            )
            .with_state(Reply {
                calls: calls.clone(),
                conversations: conversations.clone(),
                behavior,
            });
        let (shutdown, receive) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async {
                    let _ = receive.await;
                })
                .await
        });
        let foundry = Foundry {
            offline: false,
            project: Some(reqwest::Url::parse(&endpoint)?),
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(2))
                .build()?,
            identity: Arc::new(TestIdentity),
        };
        let mut outcomes = Vec::new();
        for _ in 0..invocations {
            outcomes.push(
                foundry
                    .invoke("agent with spaces", &json!({"event":{"operation":"added"}}))
                    .await,
            );
        }
        let _ = shutdown.send(());
        server.await??;
        Ok(Exercise {
            outcomes,
            submissions: calls.load(Ordering::SeqCst),
            conversations: conversations.load(Ordering::SeqCst),
        })
    }
    #[tokio::test]
    async fn explicit_throttling_retries_but_ambiguous_acceptance_does_not() -> Result<()> {
        let mut result = exercise(Behavior::Throttle, 1).await?;
        let (response, attempts) = result.outcomes.remove(0)?;
        ensure!(
            response["id"] == "response-id"
                && attempts == 2
                && result.submissions == 2
                && result.conversations == 1
        );
        let mut result = exercise(Behavior::Malformed, 1).await?;
        ensure!(
            result.outcomes.remove(0).err().is_some_and(|e| e.uncertain) && result.submissions == 1
        );
        Ok(())
    }

    #[tokio::test]
    async fn each_invocation_gets_a_fresh_agent_scoped_conversation() -> Result<()> {
        let result = exercise(Behavior::Success, 2).await?;
        ensure!(result.outcomes.iter().all(Result::is_ok));
        ensure!(result.submissions == 2 && result.conversations == 2);
        Ok(())
    }

    #[tokio::test]
    async fn setup_and_submission_failures_preserve_service_details_without_retry() -> Result<()> {
        let mut result = exercise(Behavior::RejectConversation, 1).await?;
        let error = result
            .outcomes
            .remove(0)
            .expect_err("Conversation setup must fail");
        ensure!(!error.uncertain && error.attempts == 0 && result.submissions == 0);
        ensure!(
            error.message.contains("permission_denied")
                && error.message.contains("Conversation creation denied")
        );
        let mut result = exercise(Behavior::RejectSubmission, 1).await?;
        let error = result.outcomes.remove(0).expect_err("Submission must fail");
        ensure!(!error.uncertain && error.attempts == 1 && result.submissions == 1);
        ensure!(
            error.message.contains("bad_request")
                && error.message.contains("conversation parameter is required")
        );
        Ok(())
    }
}
