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

//! Error types for Oracle source and bootstrap handling.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum OracleError {
    #[error("Connection error: {0}")]
    Connection(String),
    #[error("Recoverable SCN error: {0}")]
    RecoverableScn(String),
    #[error("Primary key error: {0}")]
    PrimaryKey(String),
    #[error("Invalid SQL identifier: {0}")]
    InvalidIdentifier(String),
    #[error("Query error: {0}")]
    Query(String),
    #[error("Configuration error: {0}")]
    Config(String),
    #[error("{0}")]
    Other(String),
}

impl OracleError {
    pub fn classify(error: &anyhow::Error) -> Option<OracleErrorKind> {
        if let Some(oracle_error) = error.downcast_ref::<OracleError>() {
            return Some(match oracle_error {
                OracleError::Connection(_) => OracleErrorKind::Connection,
                OracleError::RecoverableScn(_) => OracleErrorKind::RecoverableScn,
                OracleError::PrimaryKey(_)
                | OracleError::InvalidIdentifier(_)
                | OracleError::Query(_)
                | OracleError::Config(_)
                | OracleError::Other(_) => OracleErrorKind::Other,
            });
        }

        let error_text = error.to_string().to_lowercase();
        if error_text.contains("ora-12170")
            || error_text.contains("ora-12541")
            || error_text.contains("ora-12514")
            || error_text.contains("ora-12543")
            || error_text.contains("ora-12545")
            || error_text.contains("ora-01017")
            || error_text.contains("dpi-1047")
            || error_text.contains("connection")
            || error_text.contains("network")
            || error_text.contains("timed out")
        {
            return Some(OracleErrorKind::Connection);
        }

        if error_text.contains("ora-01291")
            || error_text.contains("ora-01327")
            || error_text.contains("missing logfile")
            || error_text.contains("scn") && error_text.contains("invalid")
        {
            return Some(OracleErrorKind::RecoverableScn);
        }

        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OracleErrorKind {
    Connection,
    RecoverableScn,
    Other,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_connection_error() {
        let err = anyhow::anyhow!("ORA-12541: TNS:no listener");
        assert_eq!(
            OracleError::classify(&err),
            Some(OracleErrorKind::Connection)
        );
    }

    #[test]
    fn test_classify_recoverable_scn_error() {
        let err = anyhow::anyhow!("ORA-01291: missing logfile");
        assert_eq!(
            OracleError::classify(&err),
            Some(OracleErrorKind::RecoverableScn)
        );
    }
}
