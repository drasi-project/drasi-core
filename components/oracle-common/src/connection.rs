// Copyright 2025 The Drasi Authors.
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

//! Oracle connection management.

use crate::config::OracleSourceConfig;
use anyhow::Result;
use log::{debug, info};
use oracle::Connection;

pub struct OracleConnection {
    connection: Connection,
}

impl OracleConnection {
    pub fn connect(config: &OracleSourceConfig) -> Result<Self> {
        info!(
            "Connecting to Oracle at {}:{} service '{}'",
            config.host, config.port, config.database
        );

        let connection =
            Connection::connect(&config.user, &config.password, config.connect_string())?;
        debug!("Successfully connected to Oracle");

        Ok(Self { connection })
    }

    pub fn inner(&self) -> &Connection {
        &self.connection
    }

    pub fn test_connection(&self) -> Result<()> {
        let row = self.connection.query_row("SELECT 1 FROM DUAL", &[])?;
        let value: i64 = row.get(0)?;
        if value != 1 {
            anyhow::bail!("Oracle connection test returned unexpected value: {value}");
        }
        Ok(())
    }
}
