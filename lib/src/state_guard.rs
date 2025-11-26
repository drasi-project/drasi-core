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

//! State guard for server initialization checks
//!
//! This module provides a centralized mechanism for verifying that the server
//! is properly initialized before operations are performed.

use crate::error::DrasiError;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Guards server operations to ensure initialization has occurred
///
/// `StateGuard` provides a reusable mechanism to check if the server has been
/// initialized before allowing operations to proceed. This eliminates duplicate
/// initialization checking logic throughout the codebase.
///
/// # Thread Safety
///
/// `StateGuard` is thread-safe and can be cloned across threads. All clones
/// share the same underlying state.
#[derive(Clone)]
pub struct StateGuard {
    initialized: Arc<RwLock<bool>>,
}

impl StateGuard {
    /// Create a new state guard with initial state (not initialized)
    pub fn new() -> Self {
        Self {
            initialized: Arc::new(RwLock::new(false)),
        }
    }

    /// Mark the server as initialized
    ///
    /// This should be called once during server initialization.
    pub async fn mark_initialized(&self) {
        *self.initialized.write().await = true;
    }

    /// Check if the server is initialized
    ///
    /// # Returns
    ///
    /// Returns `true` if the server has been initialized, `false` otherwise.
    pub async fn is_initialized(&self) -> bool {
        *self.initialized.read().await
    }

    /// Require that the server is initialized
    ///
    /// This method checks if the server has been initialized and returns an error
    /// if it has not. Use this at the beginning of operations that require initialization.
    ///
    /// # Errors
    ///
    /// Returns `DrasiError::InvalidState` if the server has not been initialized.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// pub async fn some_operation(&self) -> crate::error::Result<()> {
    ///     self.state_guard.require_initialized().await?;
    ///     // ... perform operation ...
    ///     Ok(())
    /// }
    /// ```
    pub async fn require_initialized(&self) -> crate::error::Result<()> {
        if !*self.initialized.read().await {
            return Err(DrasiError::invalid_state(
                "Server must be initialized before this operation",
            ));
        }
        Ok(())
    }
}

impl Default for StateGuard {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_initial_state_not_initialized() {
        let guard = StateGuard::new();
        assert!(!guard.is_initialized().await);
    }

    #[tokio::test]
    async fn test_mark_initialized() {
        let guard = StateGuard::new();
        guard.mark_initialized().await;
        assert!(guard.is_initialized().await);
    }

    #[tokio::test]
    async fn test_require_initialized_fails_when_not_initialized() {
        let guard = StateGuard::new();
        let result = guard.require_initialized().await;
        assert!(result.is_err());
        match result {
            Err(DrasiError::OperationFailed(message)) => {
                assert!(message.contains("initialized"));
            }
            _ => panic!("Expected OperationFailed error"),
        }
    }

    #[tokio::test]
    async fn test_require_initialized_succeeds_when_initialized() {
        let guard = StateGuard::new();
        guard.mark_initialized().await;
        let result = guard.require_initialized().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_clone_shares_state() {
        let guard1 = StateGuard::new();
        let guard2 = guard1.clone();

        guard1.mark_initialized().await;

        assert!(guard1.is_initialized().await);
        assert!(guard2.is_initialized().await);
    }
}
