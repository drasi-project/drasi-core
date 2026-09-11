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

use super::SourceChange;

/// A pure per-query transformation applied after source middleware.
pub trait SourceChangeNormalizer: Send + Sync {
    fn normalize(&self, change: SourceChange) -> SourceChange;
}

#[derive(Clone)]
pub struct SourceInput {
    pub(crate) change: SourceChange,
    pub(crate) normalizer: Option<Arc<dyn SourceChangeNormalizer>>,
}

impl SourceInput {
    pub fn with_normalizer(
        change: SourceChange,
        normalizer: Arc<dyn SourceChangeNormalizer>,
    ) -> Self {
        Self {
            change,
            normalizer: Some(normalizer),
        }
    }
}

impl From<SourceChange> for SourceInput {
    fn from(change: SourceChange) -> Self {
        Self {
            change,
            normalizer: None,
        }
    }
}
