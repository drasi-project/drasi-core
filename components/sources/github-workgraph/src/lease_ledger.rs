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

//! The lease lifecycle fold.
//!
//! Lease state is **never** derived from observing one comment. Every comment
//! delivery contributes exactly one idempotent statement about *that comment's
//! current contribution* — it acquires a lease, it ends a lease, or it
//! contributes nothing — and the lifecycle of each anchor is then recomputed
//! from the whole surviving set.
//!
//! That is what makes create, edit, pin, redelivery, delete, and rekey all
//! converge on the same current state:
//!
//! * Re-observing an acquisition restates the same set member, so it cannot
//!   resurrect a lease that other artifacts have ended.
//! * Removing an end recomputes from the ends that survive, so a deleted or
//!   edited-away end releases its hold instead of pinning the lease closed.
//! * Duplicate and mixed ends collapse into one deterministic end.
//! * Moving a comment to a different `leaseId` recomputes both the anchor it
//!   left and the anchor it joined.
//!
//! The live Source keeps this ledger in its durable state store; the
//! bootstrapper folds the identical structure over its fetched comment
//! snapshot. Both then project each anchor with [`LeaseLedger::project`], so
//! the same set of current comments yields the same anchors either way.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Which kind of artifact ended a lease.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EndKind {
    Result,
    Expired,
}

impl EndKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EndKind::Result => "result",
            EndKind::Expired => "expired",
        }
    }
}

/// One current trusted acquisition.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Acquisition {
    pub worker_id: String,
    pub slot_id: String,
    pub acquired_at: String,
    pub expires_at: String,
}

/// One current trusted end claim.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EndClaim {
    pub kind: EndKind,
    /// The authoritative instant this end took effect: a Result's comment
    /// timestamp, or an Expiration's `expiredAt`. Both are canonical
    /// fixed-width UTC, so lexicographic order is chronological order.
    pub ended_at: String,
    /// An Expiration names the Lease comment it claims to end; a Result does
    /// not, and carries `None`.
    pub lease_comment_node_id: Option<String>,
}

/// One comment's current contribution to the lease lifecycle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LifecycleIntent {
    /// A trusted acquisition comment currently exists on this anchor.
    Acquire {
        comment_node_id: String,
        anchor: AnchorKey,
        acquisition: Acquisition,
    },
    /// A trusted end comment currently exists on this anchor.
    End {
        comment_node_id: String,
        anchor: AnchorKey,
        end: EndClaim,
    },
    /// This comment contributes nothing: it was deleted, edited into something
    /// else, or is not authored and edited by trusted identities.
    Retract { comment_node_id: String },
}

impl LifecycleIntent {
    pub fn comment_node_id(&self) -> &str {
        match self {
            Self::Acquire {
                comment_node_id, ..
            }
            | Self::End {
                comment_node_id, ..
            }
            | Self::Retract { comment_node_id } => comment_node_id,
        }
    }
}

/// The task-scoped identity a lifecycle artifact contributes to.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AnchorKey {
    pub task_node_id: String,
    pub lease_id: String,
}

impl AnchorKey {
    pub fn new(task_node_id: impl Into<String>, lease_id: impl Into<String>) -> Self {
        Self {
            task_node_id: task_node_id.into(),
            lease_id: lease_id.into(),
        }
    }

    /// The stable element ID of the anchor.
    ///
    /// The task node ID comes first and is GitHub-assigned, and GitHub node IDs
    /// contain no colon, so the first separator always terminates it and no two
    /// distinct tasks can collide even though `leaseId` is free text.
    pub fn element_id(&self) -> String {
        format!("workgraph-lease:{}:{}", self.task_node_id, self.lease_id)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AnchorEntry {
    key: AnchorKey,
    /// Comment node ID → acquisition, for every currently surviving trusted
    /// Lease comment on this anchor.
    acquisitions: BTreeMap<String, Acquisition>,
    /// Comment node ID → end claim, for every currently surviving trusted
    /// Result or Expiration comment on this anchor.
    ends: BTreeMap<String, EndClaim>,
}

impl AnchorEntry {
    fn is_empty(&self) -> bool {
        self.acquisitions.is_empty() && self.ends.is_empty()
    }
}

/// The recomputed lifecycle of one anchor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnchorState {
    pub key: AnchorKey,
    pub is_active: bool,
    pub end_reason: &'static str,
    pub ended_at: Option<String>,
    pub end_comment_node_id: Option<String>,
    /// How many trusted Lease comments currently claim this anchor. More than
    /// one is a conflict.
    pub acquisition_count: usize,
    /// How many trusted end comments currently name this anchor, including any
    /// that cannot take effect.
    pub end_claim_count: usize,
}

/// The reason recorded when several trusted Lease comments claim one anchor.
pub const CONFLICT_REASON: &str = "conflict";
pub const ACTIVE_REASON: &str = "none";

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LeaseLedger {
    anchors: BTreeMap<String, AnchorEntry>,
    /// Comment node ID → the anchor element ID it currently contributes to, so
    /// an edit that moves a comment to another `leaseId` can recompute the
    /// anchor it left as well as the one it joined.
    placement: BTreeMap<String, String>,
    /// Tasks whose current lifecycle artifacts this ledger has actually seen.
    ///
    /// A ledger that has never seen a task cannot distinguish "this lease was
    /// never acquired" from "this lease was acquired before I existed", which
    /// is exactly the situation after a clean bootstrap. Callers reconcile such
    /// a task from GitHub before applying a delivery to it.
    #[serde(default)]
    reconciled_tasks: BTreeSet<String>,
}

impl LeaseLedger {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.anchors.is_empty() && self.placement.is_empty()
    }

    /// True when this ledger has seen the current lifecycle artifacts of `task`.
    pub fn knows_task(&self, task_node_id: &str) -> bool {
        self.reconciled_tasks.contains(task_node_id)
    }

    /// Drop everything known about `task` so it can be rebuilt from GitHub's
    /// current comments, returning every anchor that may have changed.
    pub fn reset_task(&mut self, task_node_id: &str) -> BTreeSet<String> {
        let mut affected = BTreeSet::new();
        let stale: Vec<String> = self
            .anchors
            .iter()
            .filter(|(_, entry)| entry.key.task_node_id == task_node_id)
            .map(|(id, _)| id.clone())
            .collect();
        for anchor in stale {
            if let Some(entry) = self.anchors.remove(&anchor) {
                for comment in entry.acquisitions.keys().chain(entry.ends.keys()) {
                    self.placement.remove(comment);
                }
            }
            affected.insert(anchor);
        }
        self.reconciled_tasks.remove(task_node_id);
        affected
    }

    /// Record that `task`'s current lifecycle artifacts have been applied.
    pub fn mark_reconciled(&mut self, task_node_id: impl Into<String>) {
        self.reconciled_tasks.insert(task_node_id.into());
    }

    /// Apply one comment's current contribution, returning every anchor whose
    /// lifecycle may have changed.
    pub fn apply(&mut self, intent: &LifecycleIntent) -> BTreeSet<String> {
        let mut affected = BTreeSet::new();
        let comment = intent.comment_node_id().to_string();

        // Wherever this comment used to contribute, it no longer does; the
        // caller's intent below is its complete current contribution.
        if let Some(previous) = self.placement.remove(&comment) {
            if let Some(entry) = self.anchors.get_mut(&previous) {
                entry.acquisitions.remove(&comment);
                entry.ends.remove(&comment);
                if entry.is_empty() {
                    self.anchors.remove(&previous);
                }
            }
            affected.insert(previous);
        }

        match intent {
            LifecycleIntent::Retract { .. } => {}
            LifecycleIntent::Acquire {
                anchor,
                acquisition,
                ..
            } => {
                let id = anchor.element_id();
                let entry = self
                    .anchors
                    .entry(id.clone())
                    .or_insert_with(|| AnchorEntry {
                        key: anchor.clone(),
                        ..AnchorEntry::default()
                    });
                entry
                    .acquisitions
                    .insert(comment.clone(), acquisition.clone());
                self.placement.insert(comment, id.clone());
                affected.insert(id);
            }
            LifecycleIntent::End { anchor, end, .. } => {
                let id = anchor.element_id();
                let entry = self
                    .anchors
                    .entry(id.clone())
                    .or_insert_with(|| AnchorEntry {
                        key: anchor.clone(),
                        ..AnchorEntry::default()
                    });
                entry.ends.insert(comment.clone(), end.clone());
                self.placement.insert(comment, id.clone());
                affected.insert(id);
            }
        }
        affected
    }

    /// Every anchor element ID the ledger currently holds, in stable order.
    pub fn anchor_ids(&self) -> Vec<String> {
        self.anchors.keys().cloned().collect()
    }

    /// Recompute one anchor from the artifacts that currently survive.
    ///
    /// Returns `None` when no anchor should exist: either nothing references
    /// the key at all, or only end claims do. An end naming a lease that was
    /// never acquired must not materialize anything a query could bind to.
    pub fn project(&self, anchor_element_id: &str) -> Option<AnchorState> {
        let entry = self.anchors.get(anchor_element_id)?;
        if entry.acquisitions.is_empty() {
            return None;
        }
        let acquisition_count = entry.acquisitions.len();
        let end_claim_count = entry.ends.len();

        // Several trusted Lease comments claiming one identity is ambiguous.
        // Fail closed: the anchor is inactive with an explicit conflict reason,
        // so it can neither be double-booked nor silently rewritten. Deleting
        // or editing back down to one acquisition restores the derived state.
        if acquisition_count > 1 {
            return Some(AnchorState {
                key: entry.key.clone(),
                is_active: false,
                end_reason: CONFLICT_REASON,
                ended_at: None,
                end_comment_node_id: None,
                acquisition_count,
                end_claim_count,
            });
        }

        let lease_comment = entry
            .acquisitions
            .keys()
            .next()
            .expect("exactly one acquisition");

        // An Expiration only counts when it names the Lease comment that
        // actually survives on this anchor. A stale or mismatched reference
        // stays projected as its own artifact but cannot end anything.
        let effective = entry.ends.iter().filter(|(_, end)| match end.kind {
            EndKind::Result => true,
            EndKind::Expired => {
                end.lease_comment_node_id.as_deref() == Some(lease_comment.as_str())
            }
        });

        // Deterministic choice: earliest authoritative end instant, then the
        // stable comment node ID. Both end timestamps are canonical fixed-width
        // UTC, so a lexicographic comparison is a chronological one.
        match effective.min_by(|left, right| {
            left.1
                .ended_at
                .cmp(&right.1.ended_at)
                .then_with(|| left.0.cmp(right.0))
        }) {
            Some((comment, end)) => Some(AnchorState {
                key: entry.key.clone(),
                is_active: false,
                end_reason: end.kind.as_str(),
                ended_at: Some(end.ended_at.clone()),
                end_comment_node_id: Some(comment.clone()),
                acquisition_count,
                end_claim_count,
            }),
            None => Some(AnchorState {
                key: entry.key.clone(),
                is_active: true,
                end_reason: ACTIVE_REASON,
                ended_at: None,
                end_comment_node_id: None,
                acquisition_count,
                end_claim_count,
            }),
        }
    }
}
