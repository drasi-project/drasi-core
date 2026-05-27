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

//! Blob service operations for Azure Storage reaction.

use azure_storage::{CloudLocation, StorageCredentials};
use azure_storage_blobs::prelude::{BlobServiceClient, ClientBuilder};

use crate::util::normalize_custom_uri;

/// Thin wrapper around the Azure Blob Storage client.
///
/// Provides `put_blob` and `delete_blob` operations used by the reaction handler.
#[derive(Clone)]
pub(crate) struct BlobService {
    client: BlobServiceClient,
}

impl BlobService {
    pub fn new(
        account_name: &str,
        credentials: StorageCredentials,
        endpoint: Option<&str>,
    ) -> Self {
        let mut builder = ClientBuilder::new(account_name, credentials);
        if let Some(uri) = endpoint {
            builder = builder.cloud_location(CloudLocation::Custom {
                account: account_name.to_string(),
                uri: normalize_custom_uri(account_name, uri),
            });
        }
        Self {
            client: builder.blob_service_client(),
        }
    }

    /// Uploads `body` as a block blob at `blob_path` in `container_name`.
    ///
    /// # Errors
    /// Returns an error if the Azure Storage call fails (e.g. container not found, auth error).
    pub async fn put_blob(
        &self,
        container_name: &str,
        blob_path: &str,
        body: &str,
        content_type: &str,
    ) -> anyhow::Result<()> {
        self.client
            .container_client(container_name)
            .blob_client(blob_path)
            .put_block_blob(body.to_string())
            .content_type(content_type.to_string())
            .await?;
        Ok(())
    }

    pub async fn delete_blob(&self, container_name: &str, blob_path: &str) -> anyhow::Result<()> {
        self.client
            .container_client(container_name)
            .blob_client(blob_path)
            .delete()
            .await?;
        Ok(())
    }
}
