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

//! Error types for MS SQL CDC source

use std::fmt;

/// Errors specific to MS SQL CDC operations
#[derive(Debug)]
pub enum MsSqlError {
    /// Connection-related errors (network issues, authentication failures, etc.)
    Connection(ConnectionError),

    /// LSN-related errors (invalid LSN, out of range, etc.)
    Lsn(LsnError),

    /// Primary key errors (missing key, NULL values, etc.)
    PrimaryKey(PrimaryKeyError),

    /// SQL identifier validation errors
    InvalidIdentifier(String),

    /// Query execution errors
    Query(String),

    /// Configuration errors
    Config(String),

    /// Other errors that don't fit into specific categories
    Other(String),
}

impl fmt::Display for MsSqlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connection(e) => write!(f, "Connection error: {e}"),
            Self::Lsn(e) => write!(f, "LSN error: {e}"),
            Self::PrimaryKey(e) => write!(f, "Primary key error: {e}"),
            Self::InvalidIdentifier(msg) => write!(f, "Invalid SQL identifier: {msg}"),
            Self::Query(msg) => write!(f, "Query error: {msg}"),
            Self::Config(msg) => write!(f, "Configuration error: {msg}"),
            Self::Other(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for MsSqlError {}

/// Connection-related error types
#[derive(Debug)]
pub enum ConnectionError {
    /// Failed to establish initial connection
    Failed(String),

    /// Connection was lost/reset
    Lost(String),

    /// Connection timed out
    Timeout(String),

    /// Authentication failed
    AuthenticationFailed(String),

    /// Network unreachable
    NetworkUnreachable(String),

    /// Connection refused by server
    Refused(String),

    /// Too many consecutive errors indicating unhealthy connection
    Unhealthy {
        consecutive_errors: u32,
        last_error: String,
    },
}

impl fmt::Display for ConnectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Failed(msg) => write!(f, "Failed to connect: {msg}"),
            Self::Lost(msg) => write!(f, "Connection lost: {msg}"),
            Self::Timeout(msg) => write!(f, "Connection timed out: {msg}"),
            Self::AuthenticationFailed(msg) => write!(f, "Authentication failed: {msg}"),
            Self::NetworkUnreachable(msg) => write!(f, "Network unreachable: {msg}"),
            Self::Refused(msg) => write!(f, "Connection refused: {msg}"),
            Self::Unhealthy {
                consecutive_errors,
                last_error,
            } => {
                write!(f, "Connection unhealthy after {consecutive_errors} consecutive errors: {last_error}")
            }
        }
    }
}

/// LSN-related error types
#[derive(Debug)]
pub enum LsnError {
    /// LSN is invalid (malformed or corrupted)
    Invalid(String),

    /// LSN is out of CDC retention range
    OutOfRange(String),

    /// LSN parsing failed
    ParseFailed(String),

    /// No LSN available from CDC
    NotAvailable(String),
}

impl fmt::Display for LsnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(msg) => write!(f, "Invalid LSN: {msg}"),
            Self::OutOfRange(msg) => write!(f, "LSN out of range: {msg}"),
            Self::ParseFailed(msg) => write!(f, "Failed to parse LSN: {msg}"),
            Self::NotAvailable(msg) => write!(f, "LSN not available: {msg}"),
        }
    }
}

/// Primary key related error types
#[derive(Debug)]
pub enum PrimaryKeyError {
    /// No primary key configured for table
    NotConfigured { table: String },

    /// Primary key column not found in row
    ColumnNotFound { table: String, column: String },

    /// All primary key values are NULL
    AllNull { table: String, columns: Vec<String> },
}

impl fmt::Display for PrimaryKeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotConfigured { table } => {
                write!(
                    f,
                    "No primary key configured for table '{table}'. \
                     Add a 'table_keys' configuration entry to specify the primary key columns."
                )
            }
            Self::ColumnNotFound { table, column } => {
                write!(
                    f,
                    "Primary key column '{column}' not found in row for table '{table}'. \
                     Check that the column name in 'table_keys' matches the actual column name."
                )
            }
            Self::AllNull { table, columns } => {
                write!(
                    f,
                    "All primary key values are NULL for table '{table}' (columns: {columns:?}). \
                     Cannot generate a stable element ID."
                )
            }
        }
    }
}

impl MsSqlError {
    /// Check if this error is a connection-related error
    pub fn is_connection_error(&self) -> bool {
        matches!(self, Self::Connection(_))
    }

    /// Check if this error is an LSN-related error that can be recovered by resetting the checkpoint
    pub fn is_recoverable_lsn_error(&self) -> bool {
        matches!(
            self,
            Self::Lsn(LsnError::Invalid(_) | LsnError::OutOfRange(_))
        )
    }

    /// Create a connection error from an underlying error message
    pub fn from_connection_error(error: impl ToString) -> Self {
        let error_str = error.to_string().to_lowercase();

        if error_str.contains("timed out") || error_str.contains("timeout") {
            Self::Connection(ConnectionError::Timeout(error.to_string()))
        } else if error_str.contains("refused") {
            Self::Connection(ConnectionError::Refused(error.to_string()))
        } else if error_str.contains("unreachable") {
            Self::Connection(ConnectionError::NetworkUnreachable(error.to_string()))
        } else if error_str.contains("authentication") || error_str.contains("login") {
            Self::Connection(ConnectionError::AuthenticationFailed(error.to_string()))
        } else if error_str.contains("reset")
            || error_str.contains("broken pipe")
            || error_str.contains("closed")
            || error_str.contains("eof")
        {
            Self::Connection(ConnectionError::Lost(error.to_string()))
        } else {
            Self::Connection(ConnectionError::Failed(error.to_string()))
        }
    }

    /// Attempt to classify an anyhow::Error as an MsSqlError
    ///
    /// This checks if the error is already an MsSqlError (via downcast),
    /// or attempts to classify it based on error message patterns.
    pub fn classify(error: &anyhow::Error) -> Option<MsSqlErrorKind> {
        // Try to downcast to MsSqlError first
        if let Some(mssql_err) = error.downcast_ref::<MsSqlError>() {
            return Some(match mssql_err {
                MsSqlError::Connection(_) => MsSqlErrorKind::Connection,
                MsSqlError::Lsn(LsnError::Invalid(_) | LsnError::OutOfRange(_)) => {
                    MsSqlErrorKind::RecoverableLsn
                }
                MsSqlError::Lsn(_) => MsSqlErrorKind::Other,
                MsSqlError::PrimaryKey(_) => MsSqlErrorKind::Other,
                MsSqlError::InvalidIdentifier(_) => MsSqlErrorKind::Other,
                MsSqlError::Query(_) => MsSqlErrorKind::Other,
                MsSqlError::Config(_) => MsSqlErrorKind::Other,
                MsSqlError::Other(_) => MsSqlErrorKind::Other,
            });
        }

        // Fall back to string-based classification for errors from external libraries
        let error_str = error.to_string().to_lowercase();

        // Check for connection errors
        if error_str.contains("connection")
            || error_str.contains("broken pipe")
            || error_str.contains("reset by peer")
            || error_str.contains("timed out")
            || error_str.contains("network")
            || error_str.contains("socket")
            || error_str.contains("eof")
            || error_str.contains("closed")
            || error_str.contains("refused")
            || error_str.contains("unreachable")
        {
            return Some(MsSqlErrorKind::Connection);
        }

        // Check for LSN errors (from SQL Server error messages)
        if error_str.contains("lsn")
            && (error_str.contains("invalid") || error_str.contains("out of range"))
        {
            return Some(MsSqlErrorKind::RecoverableLsn);
        }

        None
    }
}

/// Simplified error classification for control flow decisions
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsSqlErrorKind {
    /// Connection-related error requiring reconnection
    Connection,

    /// LSN error that can be recovered by resetting the checkpoint
    RecoverableLsn,

    /// Other errors
    Other,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_connection_error_display() {
        let err = MsSqlError::Connection(ConnectionError::Lost("connection reset".to_string()));
        assert!(err.to_string().contains("Connection lost"));
        assert!(err.is_connection_error());
    }

    #[test]
    fn test_lsn_error_display() {
        let err = MsSqlError::Lsn(LsnError::OutOfRange("LSN too old".to_string()));
        assert!(err.to_string().contains("out of range"));
        assert!(err.is_recoverable_lsn_error());
    }

    #[test]
    fn test_primary_key_error_display() {
        let err = MsSqlError::PrimaryKey(PrimaryKeyError::NotConfigured {
            table: "orders".to_string(),
        });
        assert!(err.to_string().contains("No primary key configured"));
        assert!(err.to_string().contains("orders"));
    }

    #[test]
    fn test_classify_connection_error() {
        let err = anyhow::anyhow!("connection reset by peer");
        assert_eq!(MsSqlError::classify(&err), Some(MsSqlErrorKind::Connection));

        let err = anyhow::anyhow!("broken pipe");
        assert_eq!(MsSqlError::classify(&err), Some(MsSqlErrorKind::Connection));

        let err = anyhow::anyhow!("network unreachable");
        assert_eq!(MsSqlError::classify(&err), Some(MsSqlErrorKind::Connection));
    }

    #[test]
    fn test_classify_lsn_error() {
        let err = anyhow::anyhow!("The specified LSN is invalid or out of range");
        assert_eq!(
            MsSqlError::classify(&err),
            Some(MsSqlErrorKind::RecoverableLsn)
        );
    }

    #[test]
    fn test_classify_unknown_error() {
        let err = anyhow::anyhow!("some random error");
        assert_eq!(MsSqlError::classify(&err), None);
    }

    #[test]
    fn test_from_connection_error() {
        let err = MsSqlError::from_connection_error("connection timed out");
        assert!(matches!(
            err,
            MsSqlError::Connection(ConnectionError::Timeout(_))
        ));

        let err = MsSqlError::from_connection_error("connection refused");
        assert!(matches!(
            err,
            MsSqlError::Connection(ConnectionError::Refused(_))
        ));

        let err = MsSqlError::from_connection_error("broken pipe");
        assert!(matches!(
            err,
            MsSqlError::Connection(ConnectionError::Lost(_))
        ));
    }
}
