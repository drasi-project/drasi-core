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

use reqwest::Client;

use crate::models::{HttpSourceChange, ProjectItemStatusNode};

#[derive(Debug, thiserror::Error)]
pub enum DestinationPublishError {
    #[error("transport error while publishing to destination source: {0}")]
    Transport(String),
    #[error("destination source rejected payload with HTTP {status}: {body}")]
    HttpStatus { status: u16, body: String },
}

impl DestinationPublishError {
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Transport(_) => true,
            Self::HttpStatus { status, .. } => *status >= 500 || *status == 429,
        }
    }

    pub fn is_ambiguous(&self) -> bool {
        matches!(self, Self::Transport(_))
    }
}

#[derive(Clone)]
pub struct DestinationSourceClient {
    client: Client,
    destination_event_url: String,
    bearer_secret: Option<String>,
}

impl DestinationSourceClient {
    pub fn new(
        client: Client,
        destination_event_url: impl Into<String>,
        bearer_secret: Option<String>,
    ) -> Self {
        Self {
            client,
            destination_event_url: destination_event_url.into(),
            bearer_secret,
        }
    }

    pub async fn publish_project_item_status(
        &self,
        node: &ProjectItemStatusNode,
    ) -> Result<(), DestinationPublishError> {
        let change = HttpSourceChange::update_project_item_status(node);
        let mut request = self.client.post(&self.destination_event_url).json(&change);

        if let Some(secret) = &self.bearer_secret {
            request = request.bearer_auth(secret);
        }

        let response = request
            .send()
            .await
            .map_err(|e| DestinationPublishError::Transport(e.to_string()))?;

        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|e| DestinationPublishError::Transport(e.to_string()))?;

        if !status.is_success() {
            return Err(DestinationPublishError::HttpStatus {
                status: status.as_u16(),
                body: truncate_for_error(&body),
            });
        }

        Ok(())
    }
}

fn truncate_for_error(raw: &str) -> String {
    const MAX: usize = 512;
    if raw.chars().count() <= MAX {
        return raw.to_string();
    }
    let mut truncated = raw.chars().take(MAX).collect::<String>();
    truncated.push('…');
    truncated
}
