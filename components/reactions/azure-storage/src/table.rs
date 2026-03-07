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

//! Table service operations for Azure Storage reaction.

use azure_data_tables::clients::TableServiceClientBuilder;
use azure_data_tables::prelude::TableServiceClient;
use azure_storage::{CloudLocation, StorageCredentials};
use serde_json::{Map, Value};

#[derive(Clone)]
pub struct TableService {
    client: TableServiceClient,
}

impl TableService {
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
        let mut builder = TableServiceClientBuilder::new(account_name, credentials);
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

    pub async fn upsert_entity(
        &self,
        table_name: &str,
        partition_key: &str,
        row_key: &str,
        mut properties: Map<String, Value>,
    ) -> anyhow::Result<()> {
        properties.insert(
            "PartitionKey".to_string(),
            Value::String(partition_key.to_string()),
        );
        properties.insert("RowKey".to_string(), Value::String(row_key.to_string()));

        self.client
            .table_client(table_name)
            .partition_key_client(partition_key)
            .entity_client(row_key)
            .insert_or_replace(Value::Object(properties))?
            .await?;
        Ok(())
    }

    pub async fn delete_entity(
        &self,
        table_name: &str,
        partition_key: &str,
        row_key: &str,
    ) -> anyhow::Result<()> {
        self.client
            .table_client(table_name)
            .partition_key_client(partition_key)
            .entity_client(row_key)
            .delete()
            .await?;
        Ok(())
    }
}
