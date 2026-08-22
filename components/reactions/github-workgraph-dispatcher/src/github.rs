// Copyright 2026 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::config::GitHubWorkGraphDispatcherConfig;
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use reqwest::header::{
    HeaderMap, HeaderName, HeaderValue, ACCEPT, AUTHORIZATION, RETRY_AFTER, USER_AGENT,
};
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use serde_json::json;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_COMMENT_PAGES: u32 = 100;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemoteComment {
    pub database_id: u64,
    pub node_id: String,
    pub body: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PostDisposition {
    Accepted(RemoteComment),
    Ambiguous {
        reason: String,
        retry_after: Option<Duration>,
    },
    Rejected(String),
}

#[async_trait]
pub(crate) trait GitHubApi: Send + Sync {
    async fn post_comment(
        &self,
        owner: &str,
        repository: &str,
        issue_number: u64,
        body: &str,
    ) -> PostDisposition;

    async fn list_comments(
        &self,
        owner: &str,
        repository: &str,
        issue_number: u64,
    ) -> Result<Vec<RemoteComment>>;
}

#[derive(Clone)]
pub(crate) struct RestGitHubApi {
    client: Client,
    api_url: String,
}

impl RestGitHubApi {
    pub fn new(config: &GitHubWorkGraphDispatcherConfig) -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", config.token))
                .context("token cannot be represented as an Authorization header")?,
        );
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/vnd.github+json"),
        );
        headers.insert(
            USER_AGENT,
            HeaderValue::from_str(&config.user_agent).context("invalid userAgent")?,
        );
        headers.insert(
            HeaderName::from_static("x-github-api-version"),
            HeaderValue::from_str(&config.api_version).context("invalid apiVersion")?,
        );
        for (name, value) in &config.headers {
            headers.insert(
                HeaderName::from_bytes(name.as_bytes())
                    .context("invalid configured header name")?,
                HeaderValue::from_str(value).context("invalid configured header value")?,
            );
        }
        let client = Client::builder()
            .default_headers(headers)
            .timeout(Duration::from_millis(config.request_timeout_ms))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("failed to build GitHub dispatcher HTTP client")?;
        Ok(Self {
            client,
            api_url: config.normalized_api_url(),
        })
    }

    fn comments_url(&self, owner: &str, repository: &str, issue_number: u64) -> String {
        format!(
            "{}/repos/{owner}/{repository}/issues/{issue_number}/comments",
            self.api_url
        )
    }
}

#[derive(Deserialize)]
struct CommentResponse {
    id: u64,
    node_id: String,
    body: String,
}

#[derive(Deserialize)]
struct ErrorResponse {
    message: Option<String>,
}

fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    if let Some(seconds) = headers
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
    {
        return Some(Duration::from_secs(seconds));
    }
    let reset = headers
        .get("x-ratelimit-reset")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some(Duration::from_secs(reset.saturating_sub(now)))
}

impl TryFrom<CommentResponse> for RemoteComment {
    type Error = anyhow::Error;

    fn try_from(value: CommentResponse) -> Result<Self> {
        if value.id == 0
            || value.node_id.is_empty()
            || value.node_id.chars().any(char::is_whitespace)
        {
            bail!("GitHub returned a comment without a valid identity");
        }
        Ok(Self {
            database_id: value.id,
            node_id: value.node_id,
            body: value.body,
        })
    }
}

#[async_trait]
impl GitHubApi for RestGitHubApi {
    async fn post_comment(
        &self,
        owner: &str,
        repository: &str,
        issue_number: u64,
        body: &str,
    ) -> PostDisposition {
        let response = match self
            .client
            .post(self.comments_url(owner, repository, issue_number))
            .json(&json!({ "body": body }))
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => {
                return PostDisposition::Ambiguous {
                    reason: format!("GitHub comment request transport failure: {error}"),
                    retry_after: None,
                }
            }
        };
        let status = response.status();
        let retry_after = retry_after(response.headers());
        if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
            return PostDisposition::Ambiguous {
                reason: format!("GitHub comment request returned retryable status {status}"),
                retry_after,
            };
        }
        if status == StatusCode::FORBIDDEN {
            let exhausted = response
                .headers()
                .get("x-ratelimit-remaining")
                .is_some_and(|value| value.as_bytes() == b"0");
            if exhausted || retry_after.is_some() {
                return PostDisposition::Ambiguous {
                    reason: "GitHub comment request was rate limited with status 403".to_string(),
                    retry_after,
                };
            }
            let secondary_limit = response
                .json::<ErrorResponse>()
                .await
                .ok()
                .and_then(|error| error.message)
                .is_some_and(|message| {
                    let message = message.to_ascii_lowercase();
                    message.contains("rate limit") || message.contains("abuse detection")
                });
            if secondary_limit {
                return PostDisposition::Ambiguous {
                    reason: "GitHub comment request was secondary-rate-limited with status 403"
                        .to_string(),
                    retry_after: Some(Duration::from_secs(60)),
                };
            }
            return PostDisposition::Rejected(
                "GitHub comment request returned status 403".to_string(),
            );
        }
        if !status.is_success() {
            return PostDisposition::Rejected(format!(
                "GitHub comment request returned status {status}"
            ));
        }
        let response: CommentResponse = match response.json().await {
            Ok(response) => response,
            Err(error) => {
                return PostDisposition::Ambiguous {
                    reason: format!(
                        "GitHub accepted the comment but returned invalid JSON: {error}"
                    ),
                    retry_after: None,
                }
            }
        };
        let comment = match RemoteComment::try_from(response) {
            Ok(comment) => comment,
            Err(error) => {
                return PostDisposition::Ambiguous {
                    reason: format!(
                        "GitHub accepted the comment but returned an invalid identity: {error}"
                    ),
                    retry_after: None,
                }
            }
        };
        if comment.body != body {
            return PostDisposition::Ambiguous {
                reason: "GitHub accepted the comment but returned a different body".to_string(),
                retry_after: None,
            };
        }
        PostDisposition::Accepted(comment)
    }

    async fn list_comments(
        &self,
        owner: &str,
        repository: &str,
        issue_number: u64,
    ) -> Result<Vec<RemoteComment>> {
        let url = self.comments_url(owner, repository, issue_number);
        let mut comments = Vec::new();
        for page in 1..=MAX_COMMENT_PAGES {
            let response = self
                .client
                .get(&url)
                .query(&[("per_page", 100u32), ("page", page)])
                .send()
                .await
                .context("GitHub comment reconciliation request failed")?;
            let status = response.status();
            if !status.is_success() {
                bail!("GitHub comment reconciliation returned status {status}");
            }
            let page_comments: Vec<CommentResponse> = response
                .json()
                .await
                .context("GitHub comment reconciliation returned invalid JSON")?;
            let count = page_comments.len();
            for comment in page_comments {
                comments.push(RemoteComment::try_from(comment)?);
            }
            if count < 100 {
                return Ok(comments);
            }
        }
        bail!("GitHub comment reconciliation exceeded {MAX_COMMENT_PAGES} pages")
    }
}
