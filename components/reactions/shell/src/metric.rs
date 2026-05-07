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

use std::sync::atomic::AtomicUsize;

#[derive(Default, Debug)]
pub struct ShellReactionMetrices {
    timeout_firings: AtomicUsize,
    non_zero_exits: AtomicUsize,
    stdout_truncations: AtomicUsize,
    stderr_truncations: AtomicUsize,
    stdin_payload_rejections: AtomicUsize,
}

impl ShellReactionMetrices {
    pub fn new() -> Self {
        Self {
            timeout_firings: AtomicUsize::new(0),
            non_zero_exits: AtomicUsize::new(0),
            stdout_truncations: AtomicUsize::new(0),
            stderr_truncations: AtomicUsize::new(0),
            stdin_payload_rejections: AtomicUsize::new(0),
        }
    }

    pub fn record_timeout(&self) {
        self.timeout_firings.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn record_non_zero_exit(&self) {
        self.non_zero_exits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn record_stdout_truncation(&self) {
        self.stdout_truncations.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn record_stderr_truncation(&self) {
        self.stderr_truncations.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn record_stdin_rejection(&self) {
        self.stdin_payload_rejections.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    
}

#[derive(Default, Debug)]
pub struct ShellReactionInternalState {
    active_processes: AtomicUsize,
}

impl ShellReactionInternalState {
    pub fn new() -> Self {
        Self {
            active_processes: AtomicUsize::new(0),
        }
    }

    pub fn increment_active_processes(&self) {
        self.active_processes.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn decrement_active_processes(&self) {
        self.active_processes.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}