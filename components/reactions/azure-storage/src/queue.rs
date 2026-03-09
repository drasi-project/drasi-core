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

//! Queue service operations for Azure Storage reaction.

use azure_storage::{CloudLocation, StorageCredentials};
use azure_storage_queues::prelude::QueueServiceClient;
use azure_storage_queues::QueueServiceClientBuilder;

#[derive(Clone)]
pub struct QueueService {
    client: QueueServiceClient,
}

impl QueueService {
    fn normalize_custom_uri(account_name: &str, uri: &str) -> String {
        let trimmed = uri.trim_end_matches('/');
        if trimmed.ends_with(account_name) {
            trimmed.to_string()
        } else {
            format!("{trimmed}/{account_name}")
        }
    }

    pub fn new(
        account_name: &str,
        credentials: StorageCredentials,
        endpoint: Option<&str>,
    ) -> Self {
        let mut builder = QueueServiceClientBuilder::new(account_name, credentials);
        if let Some(uri) = endpoint {
            builder = builder.cloud_location(CloudLocation::Custom {
                account: account_name.to_string(),
                uri: Self::normalize_custom_uri(account_name, uri),
            });
        }
        Self {
            client: builder.build(),
        }
    }

    pub async fn send_message(&self, queue_name: &str, payload: &str) -> anyhow::Result<()> {
        self.client
            .queue_client(queue_name)
            .put_message(payload.to_string())
            .await?;
        Ok(())
    }
}
