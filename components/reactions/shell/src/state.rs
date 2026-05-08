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

use chrono::{DateTime, Utc};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::AtomicUsize;
use std::sync::Mutex;

#[derive(Default, Debug)]
pub struct ShellReactionMetrics {
    timeout_firings: AtomicUsize,
    non_zero_exits: AtomicUsize,
    stdout_truncations: AtomicUsize,
    stderr_truncations: AtomicUsize,
    stdin_payload_rejections: AtomicUsize,
}

impl ShellReactionMetrics {
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
        self.timeout_firings
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn record_non_zero_exit(&self) {
        self.non_zero_exits
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn record_stdout_truncation(&self) {
        self.stdout_truncations
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn record_stderr_truncation(&self) {
        self.stderr_truncations
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn record_stdin_rejection(&self) {
        self.stdin_payload_rejections
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

#[derive(Default, Debug)]
pub struct ShellReactionProcessState {
    active_processes: AtomicUsize,
}

impl ShellReactionProcessState {
    pub fn new() -> Self {
        Self {
            active_processes: AtomicUsize::new(0),
        }
    }

    pub fn increment_active_processes(&self) {
        self.active_processes
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn decrement_active_processes(&self) {
        self.active_processes
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn get_active_processes(&self) -> usize {
        self.active_processes
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[derive(Debug, Clone)]
pub struct CommandExecutionContext {
    pub stdin_data: String,
    pub env: HashMap<String, String>,
    pub kill_on_drop: bool,
    pub timeout_s: u64,
    pub capture_limits: usize,
}

#[derive(Debug)]
pub struct RecentInvocation {
    pub start_timestamp: DateTime<Utc>,
    pub end_timestamp: Option<DateTime<Utc>>,
    pub exit_status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub executable: String,
}

#[derive(Debug)]
pub struct RecentInvocations {
    pub invocations: VecDeque<RecentInvocation>,
    pub max_recent_invocations: usize,
}

impl RecentInvocations {
    pub fn new(max_recent_invocations: usize) -> Self {
        Self {
            invocations: VecDeque::new(),
            max_recent_invocations,
        }
    }

    pub fn add_invocation(&mut self, invocation: RecentInvocation) {
        self.invocations.push_front(invocation);
        if self.invocations.len() > self.max_recent_invocations {
            self.invocations.pop_back();
        }
    }
}

#[derive(Debug)]
pub struct ShellReactionState {
    pub recent_invocations: Mutex<RecentInvocations>,
    pub metrics: ShellReactionMetrics,
    pub process_state: ShellReactionProcessState,
    pub reaction_id: String,
}

impl ShellReactionState {
    pub fn properties(&self) -> HashMap<String, serde_json::Value> {
        let mut props = HashMap::new();
        props.insert(
            "active_processes".to_string(),
            serde_json::json!(self
                .process_state
                .active_processes
                .load(std::sync::atomic::Ordering::Relaxed)),
        );
        props.insert(
            "timeout_firings".to_string(),
            serde_json::json!(self
                .metrics
                .timeout_firings
                .load(std::sync::atomic::Ordering::Relaxed)),
        );
        props.insert(
            "non_zero_exits".to_string(),
            serde_json::json!(self
                .metrics
                .non_zero_exits
                .load(std::sync::atomic::Ordering::Relaxed)),
        );
        props.insert(
            "stdout_truncations".to_string(),
            serde_json::json!(self
                .metrics
                .stdout_truncations
                .load(std::sync::atomic::Ordering::Relaxed)),
        );
        props.insert(
            "stderr_truncations".to_string(),
            serde_json::json!(self
                .metrics
                .stderr_truncations
                .load(std::sync::atomic::Ordering::Relaxed)),
        );
        props.insert(
            "stdin_payload_rejections".to_string(),
            serde_json::json!(self
                .metrics
                .stdin_payload_rejections
                .load(std::sync::atomic::Ordering::Relaxed)),
        );

        // Include recent invocation details in properties (e.g. count, last exit status, etc.)
        let recent_invocations_properties: Vec<serde_json::Value> = self
            .recent_invocations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .invocations
            .iter()
            .map(|inv| {
                serde_json::json!({
                    "start_timestamp": inv.start_timestamp.to_rfc3339(),
                    "end_timestamp": inv.end_timestamp.map(|t| t.to_rfc3339()),
                    "stdout": inv.stdout,
                    "stderr": inv.stderr,
                    "exit_status": inv.exit_status,
                    "executable": inv.executable,
                })
            })
            .collect();

        props.insert(
            "recent_invocations".to_string(),
            serde_json::json!(recent_invocations_properties),
        );

        props
    }
}
