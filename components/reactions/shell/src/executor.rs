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

use crate::metric::{ShellReactionInternalState, ShellReactionMetrices};
use crate::config::ShellReactionConfig;

#[derive(Debug, Clone)]
pub struct ShellExecutor {
    metrices: ShellReactionMetrices,
    internal_state: ShellReactionInternalState,
    config: ShellReactionConfig,
}

impl ShellExecutor {
    pub fn new(config: ShellReactionConfig) -> Self {
        Self {
            metrices: ShellReactionMetrices::new(),
            internal_state: ShellReactionInternalState::new(),
            config,
        }
    }

    pub fn start_proccessing_loop() {

    }
}