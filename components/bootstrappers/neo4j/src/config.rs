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

//! Configuration types for the Neo4j bootstrap provider.

/// Configuration for the Neo4j bootstrap provider.
#[derive(Debug, Clone, PartialEq)]
pub struct Neo4jBootstrapConfig {
    pub uri: String,
    pub user: String,
    pub password: String,
    pub database: String,
    pub labels: Vec<String>,
    pub rel_types: Vec<String>,
}

impl Neo4jBootstrapConfig {
    /// Validate the configuration and return an error if invalid.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.uri.trim().is_empty() {
            return Err(anyhow::anyhow!(
                "Validation error: uri cannot be empty. \
                 Please specify a Bolt URI (for example, bolt://localhost:7687)"
            ));
        }
        if self.user.trim().is_empty() {
            return Err(anyhow::anyhow!(
                "Validation error: user cannot be empty. \
                 Please specify the Neo4j user"
            ));
        }
        if self.database.trim().is_empty() {
            return Err(anyhow::anyhow!(
                "Validation error: database cannot be empty. \
                 Please specify a Neo4j database name"
            ));
        }
        Ok(())
    }
}
