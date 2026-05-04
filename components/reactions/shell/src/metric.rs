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

#[derive(Default, Debug, Clone)]
pub struct ShellReactionMetrices {
    timeout_firings: usize,
    non_zero_exits: usize,
    stdout_truncations: usize,
    stderr_truncations: usize,
    stdin_payload_rejections: usize,
}

impl ShellReactionMetrices {
    pub fn new() -> Self {
        Self {
            timeout_firings: 0,
            non_zero_exits: 0,
            stdout_truncations: 0,
            stderr_truncations: 0,
            stdin_payload_rejections: 0,
        }
    }

    pub fn record_timeout(&mut self) {
        self.timeout_firings += 1;
    }

    pub fn record_non_zero_exit(&mut self) {
        self.non_zero_exits += 1;
    }

    pub fn record_stdout_truncation(&mut self) {
        self.stdout_truncations += 1;
    }

    pub fn record_stderr_truncation(&mut self) {
        self.stderr_truncations += 1;
    }

    pub fn record_stdin_rejection(&mut self) {
        self.stdin_payload_rejections += 1;
    }
    
}

#[derive(Default, Debug, Clone)]
pub struct ShellReactionInternalState {
    active_processes: usize,
}

impl ShellReactionInternalState {
    pub fn new() -> Self {
        Self {
            active_processes: 0,
        }
    }

    pub fn increment_active_processes(&mut self) {
        self.active_processes += 1;
    }

    pub fn decrement_active_processes(&mut self) {
        if self.active_processes > 0 {
            self.active_processes -= 1;
        }
    }
}