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

/// Neo4j CDC enrichment mode expected on the remote database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CdcMode {
    Diff,
    Full,
}

impl Default for CdcMode {
    fn default() -> Self {
        Self::Full
    }
}

/// Startup behavior for CDC cursor initialization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartCursor {
    /// Start reading from the earliest available CDC record.
    Beginning,
    /// Start reading from the current CDC position.
    Now,
    /// Start from a timestamp hint in milliseconds since epoch.
    ///
    /// Neo4j CDC does not provide direct timestamp seeking, so this mode currently
    /// falls back to `Beginning` and applies timestamp filtering in the source loop.
    Timestamp(i64),
}

impl Default for StartCursor {
    fn default() -> Self {
        Self::Now
    }
}

/// Configuration for the Neo4j CDC source.
#[derive(Debug, Clone, PartialEq)]
pub struct Neo4jSourceConfig {
    pub uri: String,
    pub user: String,
    pub password: String,
    pub database: String,
    pub labels: Vec<String>,
    pub rel_types: Vec<String>,
    pub poll_interval_ms: u64,
    pub cdc_mode: CdcMode,
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

    #[test]
    fn test_default_config() {
        let config = Neo4jSourceConfig {
            uri: "bolt://localhost:7687".to_string(),
            user: "neo4j".to_string(),
            password: "secret".to_string(),
            database: "neo4j".to_string(),
            labels: Vec::new(),
            rel_types: Vec::new(),
            poll_interval_ms: 500,
            cdc_mode: CdcMode::default(),
            start_cursor: StartCursor::default(),
        };

        assert_eq!(config.database, "neo4j");
        assert_eq!(config.poll_interval_ms, 500);
        assert_eq!(config.cdc_mode, CdcMode::Full);
        assert_eq!(config.start_cursor, StartCursor::Now);
    }
}
