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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TerminalUpdatePolicy {
    #[default]
    Replace,
    Ignore,
}

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivationState {
    Absent,
    Present(Activation),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    SendCreate,
    SendFollowUp { task_id: String },
    Cancel { task_id: String },
    Drop { reason: &'static str },
}

pub fn next_action(
    operation: Operation,
    activation: &ActivationState,
    policy: TerminalUpdatePolicy,
) -> Action {
    match (operation, activation) {
        (Operation::Add, ActivationState::Absent) => Action::SendCreate,
        (Operation::Add, ActivationState::Present(Activation::ActiveTask { task_id, .. })) => {
            Action::SendFollowUp {
                task_id: task_id.clone(),
            }
        }
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
