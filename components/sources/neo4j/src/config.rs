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

/// Startup behavior for CDC cursor initialization.
#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub enum StartCursor {
    /// Start reading from the earliest available CDC record.
    Beginning,
    /// Start reading from the current CDC position.
    #[default]
    Now,
    /// Start from a timestamp hint in milliseconds since epoch.
    ///
    /// Neo4j CDC does not provide direct timestamp seeking, so this mode currently
    /// falls back to `Beginning` and applies timestamp filtering in the source loop.
    Timestamp(i64),
}

/// Configuration for the Neo4j CDC source.
#[derive(Debug, Clone, PartialEq)]
pub struct Neo4jSourceConfig {
    /// Bolt URI for the Neo4j instance (e.g. `bolt://host:7687` or `bolt+s://host:7687`).
    pub uri: String,
    /// Username for Neo4j authentication.
    pub user: String,
    /// Password for Neo4j authentication.
    pub password: String,
    /// Neo4j database name to connect to.
    pub database: String,
    /// Node labels to subscribe to for CDC events. Empty means all labels.
    pub labels: Vec<String>,
    /// Relationship types to subscribe to for CDC events. Empty means all types.
    pub rel_types: Vec<String>,
    /// Polling interval in milliseconds for CDC queries.
    pub poll_interval_ms: u64,
    /// Where to start reading CDC events from on first connection.
    pub start_cursor: StartCursor,
}

impl Neo4jSourceConfig {
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
        if self.poll_interval_ms == 0 {
            return Err(anyhow::anyhow!(
                "Validation error: poll_interval_ms cannot be 0. \
                 Please specify a polling interval in milliseconds"
            ));
        }
        if let StartCursor::Timestamp(ts) = &self.start_cursor {
            if *ts < 0 {
                return Err(anyhow::anyhow!(
                    "Validation error: start_cursor timestamp cannot be negative. \
                     Please specify a non-negative millisecond epoch timestamp"
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_config() -> Neo4jSourceConfig {
        Neo4jSourceConfig {
            uri: "bolt://localhost:7687".to_string(),
            user: "neo4j".to_string(),
            password: "secret".to_string(),
            database: "neo4j".to_string(),
            labels: Vec::new(),
            rel_types: Vec::new(),
            poll_interval_ms: 500,
            start_cursor: StartCursor::default(),
        }
    }

    #[test]
    fn test_valid_config_passes_validation() {
        assert!(valid_config().validate().is_ok());
    }

    #[test]
    fn test_validate_empty_uri() {
        let mut config = valid_config();
        config.uri = "  ".to_string();
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("uri cannot be empty"));
    }

    #[test]
    fn test_validate_empty_user() {
        let mut config = valid_config();
        config.user = "".to_string();
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("user cannot be empty"));
    }

    #[test]
    fn test_validate_empty_database() {
        let mut config = valid_config();
        config.database = " ".to_string();
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("database cannot be empty"));
    }

    #[test]
    fn test_validate_zero_poll_interval() {
        let mut config = valid_config();
        config.poll_interval_ms = 0;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("poll_interval_ms cannot be 0"));
    }

    #[test]
    fn test_validate_negative_timestamp() {
        let mut config = valid_config();
        config.start_cursor = StartCursor::Timestamp(-1);
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("timestamp cannot be negative"));
    }

    #[test]
    fn test_validate_valid_timestamp() {
        let mut config = valid_config();
        config.start_cursor = StartCursor::Timestamp(1700000000000);
        assert!(config.validate().is_ok());
    }
}
