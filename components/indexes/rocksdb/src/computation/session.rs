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

use std::sync::Arc;

use async_trait::async_trait;
use drasi_core::{
    computation::ComputationResourceCleanup,
    interface::{IndexError, SessionControl},
};

use crate::RocksDbSessionState;

use super::blocking::{BlockingFailures, BlockingScope};

pub(super) struct ComputationSession {
    pub(super) state: Arc<RocksDbSessionState>,
    pub(super) work: Arc<BlockingScope>,
}

#[async_trait]
impl SessionControl for ComputationSession {
    async fn begin(&self) -> Result<(), IndexError> {
        let state = self.state.clone();
        self.work.run(move || state.begin()).await
    }

    async fn commit(&self) -> Result<(), IndexError> {
        let state = self.state.clone();
        self.work.run(move || state.commit()).await
    }

    fn rollback(&self) -> Result<(), IndexError> {
        self.state.rollback()
    }
}

#[async_trait]
impl ComputationResourceCleanup for ComputationSession {
    fn cancel(&self) {
        self.work.cancel();
    }

    async fn shutdown(&self) -> Result<(), IndexError> {
        let mut failures = Vec::new();
        if let Err(error) = self.work.shutdown().await {
            failures.push(error);
        }
        if let Err(error) = self.state.rollback() {
            failures.push(error);
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(IndexError::other(BlockingFailures(failures)))
        }
    }
}
