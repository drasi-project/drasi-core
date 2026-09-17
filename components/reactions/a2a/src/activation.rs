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

use serde::{Deserialize, Serialize};

/// Resolved `resultKeyFields` value used to correlate diffs with an A2A task.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ResultKey(pub String);

impl ResultKey {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ResultKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Deterministic A2A `messageId`: length-prefixed identity plus `sequence`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MessageId(pub String);

impl MessageId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for MessageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// What to do when a follow-up arrives after the remote task is terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TerminalUpdatePolicy {
    #[default]
    Replace,
    Ignore,
}

/// Query-result operation mapped onto A2A create / follow-up / cancel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    Add,
    Update,
    Delete,
}

impl Operation {
    pub fn as_str(&self) -> &'static str {
        match self {
            Operation::Add => "ADD",
            Operation::Update => "UPDATE",
            Operation::Delete => "DELETE",
        }
    }
}

/// Persisted per-result-key view of the remote A2A task or one-shot message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Activation {
    OneShot {
        message_id: MessageId,
        sequence: u64,
    },
    ActiveTask {
        task_id: String,
        context_id: String,
        state: String,
        sequence: u64,
    },
    TerminalTask {
        task_id: String,
        context_id: String,
        state: String,
        sequence: u64,
    },
}

impl Activation {
    pub fn sequence(&self) -> u64 {
        match self {
            Activation::OneShot { sequence, .. }
            | Activation::ActiveTask { sequence, .. }
            | Activation::TerminalTask { sequence, .. } => *sequence,
        }
    }

    pub fn with_sequence(self, sequence: u64) -> Self {
        match self {
            Activation::OneShot { message_id, .. } => Activation::OneShot {
                message_id,
                sequence,
            },
            Activation::ActiveTask {
                task_id,
                context_id,
                state,
                ..
            } => Activation::ActiveTask {
                task_id,
                context_id,
                state,
                sequence,
            },
            Activation::TerminalTask {
                task_id,
                context_id,
                state,
                ..
            } => Activation::TerminalTask {
                task_id,
                context_id,
                state,
                sequence,
            },
        }
    }
}

/// Encode identity parts as `len:part` segments joined by `:`.
/// Length prefixes keep keys inspectable and prevent collisions when a
/// part itself contains `:`.
pub(crate) fn length_prefixed(parts: &[&str]) -> String {
    let mut out = String::new();
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            out.push(':');
        }
        out.push_str(&part.len().to_string());
        out.push(':');
        out.push_str(part);
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivationState {
    Absent,
    Present(Activation),
}

/// Next A2A call for a diff given current [`Activation`] state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    SendCreate,
    SendFollowUp { task_id: String },
    Cancel { task_id: String },
    Drop { reason: &'static str },
}

/// Choose create, follow-up, cancel, or drop from the current activation.
pub fn next_action(
    operation: Operation,
    activation: &ActivationState,
    policy: TerminalUpdatePolicy,
    sequence: u64,
) -> Action {
    if let ActivationState::Present(existing) = activation {
        if existing.sequence() >= sequence {
            return Action::Drop {
                reason: "diff sequence already applied",
            };
        }
    }

    match (operation, activation) {
        (Operation::Add, ActivationState::Absent) => Action::SendCreate,
        (Operation::Add, ActivationState::Present(Activation::ActiveTask { .. })) => Action::Drop {
            reason: "ADD ignored for active task",
        },
        (Operation::Add, ActivationState::Present(Activation::OneShot { .. })) => Action::Drop {
            reason: "ADD ignored for one-shot activation",
        },
        (Operation::Add, ActivationState::Present(Activation::TerminalTask { .. })) => match policy
        {
            TerminalUpdatePolicy::Replace => Action::SendCreate,
            TerminalUpdatePolicy::Ignore => Action::Drop {
                reason: "ADD ignored for terminal activation",
            },
        },
        (Operation::Update, ActivationState::Absent) => Action::SendCreate,
        (Operation::Update, ActivationState::Present(Activation::ActiveTask { task_id, .. })) => {
            Action::SendFollowUp {
                task_id: task_id.clone(),
            }
        }
        (Operation::Update, ActivationState::Present(Activation::OneShot { .. })) => Action::Drop {
            reason: "UPDATE ignored for one-shot activation",
        },
        (Operation::Update, ActivationState::Present(Activation::TerminalTask { .. })) => {
            match policy {
                TerminalUpdatePolicy::Replace => Action::SendCreate,
                TerminalUpdatePolicy::Ignore => Action::Drop {
                    reason: "UPDATE ignored for terminal activation",
                },
            }
        }
        (Operation::Delete, ActivationState::Present(Activation::ActiveTask { task_id, .. })) => {
            Action::Cancel {
                task_id: task_id.clone(),
            }
        }
        (Operation::Delete, _) => Action::Drop {
            reason: "DELETE ignored without an active task",
        },
    }
}
