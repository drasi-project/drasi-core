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

//! Per-result-key correlation state and the diff → AHP action decision.

use std::cmp::Ordering;

use serde::{Deserialize, Serialize};

use crate::config::AhpMode;

/// Resolved `resultKeyFields` value that correlates diffs with AHP work.
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

/// Query-result operation.
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

/// Persisted AHP work correlated with one result key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Activation {
    /// A session (and the chat that receives turns) created in session mode.
    Session {
        session: String,
        chat: String,
        sequence: u64,
        diff_index: u32,
    },
    /// An automation run started in automation mode.
    Automation {
        run: String,
        sequence: u64,
        diff_index: u32,
    },
}

impl Activation {
    fn position(&self) -> (u64, u32) {
        match self {
            Activation::Session {
                sequence,
                diff_index,
                ..
            }
            | Activation::Automation {
                sequence,
                diff_index,
                ..
            } => (*sequence, *diff_index),
        }
    }

    /// True when the diff at (`sequence`, `diff_index`) was already applied.
    pub fn already_applied(&self, sequence: u64, diff_index: u32) -> bool {
        let (applied_seq, applied_idx) = self.position();
        match applied_seq.cmp(&sequence) {
            Ordering::Greater => true,
            Ordering::Equal => applied_idx >= diff_index,
            Ordering::Less => false,
        }
    }

    /// Same activation with its applied position advanced.
    pub fn with_applied(self, sequence: u64, diff_index: u32) -> Self {
        match self {
            Activation::Session { session, chat, .. } => Activation::Session {
                session,
                chat,
                sequence,
                diff_index,
            },
            Activation::Automation { run, .. } => Activation::Automation {
                run,
                sequence,
                diff_index,
            },
        }
    }
}

/// The next AHP operation for a diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Create a session and start its first turn.
    StartSession,
    /// Start a run of the configured automation.
    RunAutomation,
    /// Send a follow-up pending message to an existing chat.
    FollowUp { session: String, chat: String },
    /// Apply `onDelete` to the correlated work and forget it.
    Close(Activation),
    /// Do nothing.
    Drop { reason: &'static str },
}

/// Choose the AHP operation for `operation` given the current activation.
pub fn next_action(
    mode: AhpMode,
    operation: Operation,
    activation: Option<&Activation>,
    sequence: u64,
    diff_index: u32,
) -> Action {
    if let Some(existing) = activation {
        if existing.already_applied(sequence, diff_index) {
            return Action::Drop {
                reason: "diff already applied",
            };
        }
    }
    match (operation, activation) {
        (Operation::Add | Operation::Update, None) => match mode {
            AhpMode::Session => Action::StartSession,
            AhpMode::Automation => Action::RunAutomation,
        },
        (Operation::Add, Some(_)) => Action::Drop {
            reason: "ADD ignored for an existing activation",
        },
        (Operation::Update, Some(Activation::Session { session, chat, .. })) => Action::FollowUp {
            session: session.clone(),
            chat: chat.clone(),
        },
        (Operation::Update, Some(Activation::Automation { .. })) => Action::Drop {
            reason: "automation runs do not accept follow-up input",
        },
        (Operation::Delete, Some(existing)) => Action::Close(existing.clone()),
        (Operation::Delete, None) => Action::Drop {
            reason: "DELETE ignored without an activation",
        },
    }
}

/// Encode identity parts as `len:part` segments joined by `:` so parts
/// containing `:` cannot collide.
pub(crate) fn length_prefixed(parts: &[&str]) -> String {
    parts
        .iter()
        .map(|p| format!("{}:{p}", p.len()))
        .collect::<Vec<_>>()
        .join(":")
}
