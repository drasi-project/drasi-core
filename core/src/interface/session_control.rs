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

use std::sync::Arc;

use async_trait::async_trait;

use super::IndexError;

/// Backend-implemented lifecycle control for session-scoped transactions.
///
/// Implementations manage the begin/commit/rollback lifecycle for atomic
/// writes across all indexes during a single source-change processing.
/// Session state lives inside backend implementations, shared via `Arc`.
#[async_trait]
pub trait SessionControl: Send + Sync {
    /// Begin a new session-scoped transaction.
    async fn begin(&self) -> Result<(), IndexError>;

    /// Commit the current session-scoped transaction.
    async fn commit(&self) -> Result<(), IndexError>;

    /// Roll back the current session-scoped transaction.
    ///
    /// This is synchronous to be safe for use in `Drop` implementations.
    fn rollback(&self);
}

/// No-op implementation of `SessionControl`.
///
/// All methods are no-ops that return `Ok(())`. Used for in-memory backends
/// and as the default when no session control is configured.
pub struct NoOpSessionControl;

#[async_trait]
impl SessionControl for NoOpSessionControl {
    async fn begin(&self) -> Result<(), IndexError> {
        Ok(())
    }

    async fn commit(&self) -> Result<(), IndexError> {
        Ok(())
    }

    fn rollback(&self) {}
}

/// RAII guard for session-scoped transactions.
///
/// Created via `SessionGuard::begin()`, which calls `control.begin()`.
/// On drop, automatically calls `control.rollback()` unless `commit()`
/// was called first.
pub struct SessionGuard {
    control: Arc<dyn SessionControl>,
    committed: bool,
}

impl SessionGuard {
    /// Begin a new session, returning an RAII guard.
    ///
    /// Calls `control.begin()` and returns a guard that will automatically
    /// roll back on drop unless `commit()` is called.
    pub async fn begin(control: Arc<dyn SessionControl>) -> Result<Self, IndexError> {
        control.begin().await?;
        Ok(Self {
            control,
            committed: false,
        })
    }

    /// Commit the session-scoped transaction.
    ///
    /// Marks the guard as committed so that `Drop` will not roll back.
    pub async fn commit(mut self) -> Result<(), IndexError> {
        self.committed = true;
        self.control.commit().await
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        if !self.committed {
            self.control.rollback();
        }
    }
}
