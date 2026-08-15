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

//! The routing reaction.
//!
//! Each added row **is** one authoritative `CompletedIssueValidation` comment as
//! the GitHub Source projected it (see [`crate::candidate`]). For one such row
//! the reaction:
//!
//! 1. accepts the row's completion event — unedited (`isEdited == false`),
//!    authored by `trustedAuthorDatabaseId` + `trustedAuthorType`, strictly
//!    parsed, and bound to the row's item, subject, and `bodyDigest`;
//! 2. re-reads the authoritative issue and requires its **current** body digest
//!    to still equal the row's `bodyDigest`, then verifies the Project item
//!    binding and that the item is still at `AwaitingValidation`;
//! 3. requires the rest of the chain — a trusted `ResponsibilityAssigned` (with
//!    the expected profile and the same body digest) and a trusted
//!    `ExecutionStarted` whose execution the completion agrees with — to be
//!    active on the issue, and requires exactly **one** accepted completion,
//!    carrying exactly the event the row delivered;
//! 4. writes a durable intent record — pinning the accepted completion comment,
//!    its body hash, and the canonical JSON of the decision it will publish —
//!    **before** touching GitHub;
//! 5. posts exactly one `RoutingDecided` comment (adopting one a previous
//!    attempt may already have written, but only when that comment is
//!    byte-identical to the intended decision); and
//! 6. sets the Project status **directly** to the final destination.
//!
//! There is no intermediate `AwaitingRouting` status, no fifth assignment
//! event, and no separate routing reservation: the next responsibility travels
//! inside the `RoutingDecided` payload, and the deterministic `eventId` is the
//! reservation.
//!
//! # Before and after publication
//!
//! Steps 1–3 are the *pre-publication* guard: nothing is written until the row's
//! completion is trusted, the current issue body still derives this run, the
//! chain is coherent, and the completion the decision came from is unchanged.
//!
//! Once the decision comment is durably recorded as published (or adopted), or
//! merely as *attempted*, that guard is over. The remaining work is finished
//! **from the persisted record** — see [`resume_attempted_decision`] — because
//! a decision that may already be visible in the issue thread must not be
//! stranded merely because the issue body or the completion input changed
//! afterwards. What is still reconciled is the decision comment itself: it must
//! exist (or be created from the pinned event when an authoritative listing
//! shows the write never landed), be trusted, be unedited, stay bound to the
//! recorded run/item/subject, and carry exactly the canonical event JSON the
//! record pinned. Any deviation is a hard halt with zero further side effects —
//! never a skippable rejection.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context;
use async_trait::async_trait;
use drasi_lib::channels::{ComponentStatus, QueryResult, ResultDiff};
use drasi_lib::managers::log_component_start;
use drasi_lib::reactions::common::base::{ReactionBase, ReactionBaseParams};
use drasi_lib::reactions::common::CheckpointState;
use drasi_lib::reactions::ManagerCheckpointOwnership;
use drasi_lib::recovery::ReactionRecoveryPolicy;
use drasi_lib::state_store::StateStoreProvider;
use drasi_lib::Reaction;
use drasi_workgraph_common::{
    comment::{parse_comment, render_comment},
    dedup::{adopt_published_event, coalesce, ObservedComment},
    event::{
        CompletedIssueValidationPayload, EventId, ExecutionId, ResponsibilityAssignedPayload,
        RoutingDecidedPayload, WorkGraphEvent, WorkGraphEventPayload, WorkGraphEventType,
    },
    ids::{body_digest, event_id},
    row::AcceptedEventRow,
    summary::summary_for,
};
use log::{error, info, warn};

use crate::candidate::RoutingCandidate;
use crate::config::{WorkgraphRouterReactionConfig, ROUTABLE_STATUS};
use crate::github_client::{GithubClient, IssueComment, ProjectItemRef, UpdateStatusOutcome};
use crate::state::{
    comment_body_hash, compare_and_swap_record, create_record_if_absent, load_open_run,
    load_record, set_open_run, AcceptedCompletion, PersistedRoutingRecord, RoutingRecord,
};
use crate::WorkgraphRouterReactionBuilder;

/// A row that can never succeed, no matter how often it is retried.
///
/// Permanent rejections are logged and skipped; they have no external effect,
/// so unlike transient failures they do not need a durable tombstone to stay
/// consistent across replays. A run whose validation has simply not completed
/// yet is also "permanent" for *this* row: the next result diff re-nominates it.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
struct PermanentCandidateError {
    message: String,
}

impl PermanentCandidateError {
    #[allow(clippy::new_ret_no_self)]
    fn new(message: impl Into<String>) -> anyhow::Error {
        anyhow::Error::new(Self {
            message: message.into(),
        })
    }
}

/// The WorkGraph router reaction.
pub struct WorkgraphRouterReaction {
    pub(crate) base: ReactionBase,
    pub(crate) config: WorkgraphRouterReactionConfig,
}

impl WorkgraphRouterReaction {
    /// Start building a reaction.
    pub fn builder(id: impl Into<String>) -> WorkgraphRouterReactionBuilder {
        WorkgraphRouterReactionBuilder::new(id)
    }

    pub(crate) fn from_builder(
        id: String,
        queries: Vec<String>,
        config: WorkgraphRouterReactionConfig,
        priority_queue_capacity: Option<usize>,
        auto_start: bool,
    ) -> Self {
        let mut params = ReactionBaseParams::new(id, queries).with_auto_start(auto_start);
        if let Some(capacity) = priority_queue_capacity {
            params = params.with_priority_queue_capacity(capacity);
        }
        Self {
            base: ReactionBase::new(params),
            config,
        }
    }
}

#[async_trait]
impl Reaction for WorkgraphRouterReaction {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "workgraph-router"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        self.base.properties_or_serialize(&self.config)
    }

    fn query_ids(&self) -> Vec<String> {
        self.base.queries.clone()
    }

    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
    }

    async fn initialize(&self, context: drasi_lib::context::ReactionRuntimeContext) {
        self.base.initialize(context).await;
    }

    async fn start(&self) -> anyhow::Result<()> {
        log_component_start("WorkGraph Router Reaction", &self.base.id);
        self.config
            .validate(&self.base.queries)
            .context("invalid workgraph-router config")?;

        self.base
            .set_status(
                ComponentStatus::Starting,
                Some("Starting workgraph-router reaction".to_string()),
            )
            .await;

        let github = GithubClient::from_config(&self.config)?;
        let shutdown_rx = self.base.create_shutdown_channel().await;
        let base = self.base.clone_shared();
        let config = self.config.clone();
        let reaction_name = self.base.id.clone();

        let handle = tokio::spawn(async move {
            let mut checkpoint_state = CheckpointState::load(&base).await;
            run_processing_loop(
                &reaction_name,
                base,
                config,
                github,
                &mut checkpoint_state,
                shutdown_rx,
            )
            .await;
        });

        self.base.set_processing_task(handle).await;
        self.base
            .set_status(
                ComponentStatus::Running,
                Some("Workgraph router running".to_string()),
            )
            .await;
        Ok(())
    }

    async fn stop(&self) -> anyhow::Result<()> {
        self.base.stop_common().await
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn enqueue_query_result(&self, result: QueryResult) -> anyhow::Result<()> {
        self.base.enqueue_query_result(result).await
    }

    fn is_durable(&self) -> bool {
        true
    }

    fn needs_snapshot_on_fresh_start(&self) -> bool {
        false
    }

    fn default_recovery_policy(&self) -> ReactionRecoveryPolicy {
        ReactionRecoveryPolicy::Strict
    }

    fn checkpoint_ownership(&self) -> ManagerCheckpointOwnership {
        ManagerCheckpointOwnership::Reaction
    }
}

async fn run_processing_loop(
    reaction_name: &str,
    base: ReactionBase,
    config: WorkgraphRouterReactionConfig,
    github: GithubClient,
    checkpoint_state: &mut CheckpointState,
    mut shutdown_rx: tokio::sync::oneshot::Receiver<()>,
) {
    loop {
        let event = tokio::select! {
            biased;
            _ = &mut shutdown_rx => break,
            event = base.priority_queue.dequeue() => event,
        };

        if let Err(error) =
            process_query_result(reaction_name, &base, &config, &github, &event).await
        {
            error!(
                "[{reaction_name}] routing failed for query '{}' sequence {}: {error:#}",
                event.query_id, event.sequence
            );
            base.set_status(
                ComponentStatus::Error,
                Some(format!("Workgraph-router failed: {error:#}")),
            )
            .await;
            // Strict recovery: stop without advancing the checkpoint so the
            // batch replays from the outbox after operator intervention.
            return;
        }

        if let Err(error) = checkpoint_state
            .advance(&base, &event.query_id, event.sequence)
            .await
        {
            error!(
                "[{reaction_name}] checkpoint update failed at sequence {}: {error:#}",
                event.sequence
            );
            base.set_status(
                ComponentStatus::Error,
                Some(format!("Workgraph-router checkpoint failure: {error:#}")),
            )
            .await;
            return;
        }
    }
}

async fn process_query_result(
    reaction_name: &str,
    base: &ReactionBase,
    config: &WorkgraphRouterReactionConfig,
    github: &GithubClient,
    result: &QueryResult,
) -> anyhow::Result<()> {
    for diff in &result.results {
        match diff {
            ResultDiff::Add { data, .. } => {
                let candidate: RoutingCandidate = match serde_json::from_value(data.clone()) {
                    Ok(candidate) => candidate,
                    Err(error) => {
                        warn!(
                            "[{reaction_name}] skipping malformed routing row on query '{}': {error}",
                            result.query_id
                        );
                        continue;
                    }
                };
                match route(reaction_name, base, config, github, &candidate).await {
                    Ok(()) => {}
                    Err(error) if error.downcast_ref::<PermanentCandidateError>().is_some() => {
                        warn!(
                            "[{reaction_name}] not routing {}#{}: {error}",
                            candidate.repository, candidate.subject_number
                        );
                    }
                    Err(error) => return Err(error),
                }
            }
            ResultDiff::Update { .. } | ResultDiff::Delete { .. } => {
                info!(
                    "[{reaction_name}] ignoring non-added diff for query '{}'",
                    result.query_id
                );
            }
            ResultDiff::Aggregation { .. } | ResultDiff::Noop => {}
        }
    }
    Ok(())
}

/// The trusted event chain for one run, read from the issue thread.
///
/// The assignment is validated inside [`trusted_chain`] (profile and body
/// digest) but is not carried here: nothing downstream needs it, and an unused
/// field would invite a later reader to route on it.
#[derive(Debug)]
struct TrustedChain {
    /// The execution both the start and the completion agree on.
    execution_id: ExecutionId,
    /// The accepted completion payload the decision derives from.
    completion: CompletedIssueValidationPayload,
    /// The physical comment that carried [`Self::completion`].
    accepted_completion: AcceptedCompletion,
}

/// Route one candidate, resuming any partially completed prior attempt.
async fn route(
    reaction_name: &str,
    base: &ReactionBase,
    config: &WorkgraphRouterReactionConfig,
    github: &GithubClient,
    candidate: &RoutingCandidate,
) -> anyhow::Result<()> {
    candidate
        .validate(config)
        .map_err(|error| PermanentCandidateError::new(error.to_string()))?;

    let store = base
        .state_store()
        .await
        .ok_or_else(|| anyhow::anyhow!("a durable state store is required for workgraph-router"))?;

    // 0. A decision this item may already show owns the item. Finish it from
    //    durable state before looking at anything live: re-deriving the chain
    //    (or a new run from a changed issue body) would strand a decision the
    //    issue thread already shows — or, after an unobserved write, one it may
    //    show without this process ever having seen its comment ID.
    if let Some(persisted) = attempted_but_unapplied_run(store.clone(), &base.id, candidate).await?
    {
        return resume_attempted_decision(
            reaction_name,
            base,
            config,
            github,
            candidate,
            store,
            persisted,
        )
        .await;
    }

    // 1. Accept the row's completion event: unedited, authored by the trusted
    //    identity, strictly parsed, and bound to this row's item, subject, and
    //    `bodyDigest`. The run comes from that binding, never from the event
    //    JSON alone.
    let accepted_row = candidate
        .accept_completion(config)
        .map_err(|error| PermanentCandidateError::new(error.to_string()))?;
    let run = accepted_row.run_id.clone();
    let digest = accepted_row.body_digest.clone();

    // 2. Authoritative issue read: the row's `bodyDigest` must still be the
    //    issue's current digest, or the run this row names no longer exists.
    let issue = github
        .issue_snapshot(&candidate.repository, candidate.subject_number)
        .await
        .context("failed to read the authoritative issue")?;
    if issue.node_id != candidate.subject_node_id {
        return Err(PermanentCandidateError::new(format!(
            "{}#{} resolves to node '{}', not the row's '{}'",
            candidate.repository,
            candidate.subject_number,
            issue.node_id,
            candidate.subject_node_id
        )));
    }
    let current_digest = body_digest(issue.body.as_deref());
    if current_digest != digest {
        return Err(PermanentCandidateError::new(format!(
            "issue body changed since the completion: row bodyDigest '{}' but the current body is '{}'",
            digest.as_str(),
            current_digest.as_str()
        )));
    }

    // 3. Verify the Project binding and that the item is still routable. The
    //    decided destinations are tolerated so a resumed run can finish.
    let item = ProjectItemRef {
        project_node_id: &candidate.project_node_id,
        project_item_node_id: &candidate.project_item_node_id,
        subject_node_id: &candidate.subject_node_id,
        repository: &candidate.repository,
        subject_number: candidate.subject_number,
    };
    let snapshot = github
        .project_snapshot(item)
        .await
        .context("failed to verify the project item binding")?;
    // Only the routable status may start (or re-derive) a decision. An item
    // already at a decided status is finished exclusively by
    // [`resume_published_decision`], from durable state — never by re-deriving.
    if snapshot.current_status != ROUTABLE_STATUS {
        return Err(PermanentCandidateError::new(format!(
            "project item '{}' status is '{}' (expected '{ROUTABLE_STATUS}')",
            candidate.project_item_node_id, snapshot.current_status
        )));
    }

    // 4. Confirm the comment the row names, then require the rest of the chain
    //    to be active and the row's completion to be the one accepted completion.
    let comments = github
        .list_issue_comments(&candidate.repository, candidate.subject_number)
        .await
        .context("failed to list issue comments")?;
    verify_named_completion_comment(config, candidate, &accepted_row.event, &comments)?;
    let chain = trusted_chain(config, candidate, &accepted_row, &run, &digest, &comments)?;

    let decision = RoutingDecidedPayload::for_outcome(chain.completion.outcome);
    let event = WorkGraphEvent::new(
        run.clone(),
        candidate.project_item_node_id.clone(),
        candidate.subject_node_id.clone(),
        WorkGraphEventPayload::RoutingDecided(decision.clone()),
    )
    .map_err(|error| PermanentCandidateError::new(error.to_string()))?;
    let summary = summary_for(&event);
    let body = render_comment(&event, &summary)
        .map_err(|error| anyhow::anyhow!("failed to render the routing comment: {error}"))?;

    // 5. Durable intent before any external effect.
    let intent = RoutingRecord::new(
        run.as_str(),
        event.event_id.as_str(),
        candidate,
        digest.as_str(),
        chain.accepted_completion.clone(),
        chain.completion.outcome.as_str(),
        decision.to_status.as_str(),
        &event.to_canonical_json(),
    );
    let mut persisted = match create_record_if_absent(store.clone(), &base.id, &intent).await? {
        Some(existing) => {
            existing.record.ensure_matches(candidate)?;
            // A run is decided exactly once: refuse to continue if the
            // completion the decision was derived from has changed.
            existing.record.ensure_decision_inputs_unchanged(
                &chain.accepted_completion,
                chain.completion.outcome.as_str(),
                decision.to_status.as_str(),
            )?;
            existing
        }
        None => load_record(store.clone(), &base.id, run.as_str())
            .await?
            .ok_or_else(|| anyhow::anyhow!("routing record vanished immediately after create"))?,
    };

    if persisted.record.is_complete() {
        info!(
            "[{reaction_name}] run '{run}' for {}#{} is already routed to '{}'; nothing to do",
            candidate.repository, candidate.subject_number, persisted.record.to_status
        );
        return Ok(());
    }

    // 5b. Point the item at this run *before* the first external effect, so a
    //     later attempt can find this decision even if the issue body (and with
    //     it a freshly derived `runId`) changes after publication.
    claim_open_run(store.clone(), &base.id, candidate, run.as_str()).await?;

    // 6. Exactly one decision comment, adopting an earlier write if present.
    //    `published_in_this_pass` carries the physical comment this pass saw, so
    //    step 7 can verify it without another round trip.
    let mut published_in_this_pass: Option<IssueComment> = None;
    if persisted.record.decision_comment_node_id.is_none() {
        published_in_this_pass = publish_decision_comment(
            base,
            config,
            github,
            store.clone(),
            &mut persisted,
            &event,
            &body,
        )
        .await?;
    }

    // 7. Move directly to the final status — through the *same* verified finish
    //    a resumed run uses, so a status can never move for a decision comment
    //    that was not checked in this pass.
    finish_published_decision(
        reaction_name,
        base,
        github,
        config,
        store,
        &mut persisted,
        published_in_this_pass,
    )
    .await?;

    info!(
        "[{reaction_name}] routed {}#{} to '{}' (next responsibility '{}') as run '{run}' from execution '{}'",
        candidate.repository,
        candidate.subject_number,
        decision.to_status.as_str(),
        decision.next_responsibility_type.as_str(),
        chain.execution_id
    );
    Ok(())
}

/// Whether a status is one of the two destinations a routing decision may set.
fn is_decided_status(status: &str) -> bool {
    status == drasi_workgraph_common::status::AWAITING_ISSUE_RISK_PROFILING
        || status == drasi_workgraph_common::status::NEEDS_MORE_INFORMATION
}

/// The run that owns `candidate`'s Project item when publishing its decision
/// has been attempted but its final status move has not been applied yet.
///
/// "Attempted" deliberately includes a run whose create-comment outcome was
/// never observed: its decision may already be visible in the issue thread, so
/// it must be reconciled from durable state rather than skipped by a fresh
/// derivation (which a later issue-body edit would otherwise silently do).
///
/// Returns `None` when the item has no open run, when that run has not yet
/// reached the publication attempt (so the normal derivation path applies), or
/// when it is already complete.
async fn attempted_but_unapplied_run(
    store: Arc<dyn StateStoreProvider>,
    store_id: &str,
    candidate: &RoutingCandidate,
) -> anyhow::Result<Option<PersistedRoutingRecord>> {
    let Some(run_id) = load_open_run(store.clone(), store_id, &candidate.project_item_node_id)
        .await
        .context("failed to read the open run for the project item")?
    else {
        return Ok(None);
    };
    let Some(persisted) = load_record(store, store_id, &run_id)
        .await
        .context("failed to load the open run's routing record")?
    else {
        return Ok(None);
    };
    Ok(persisted
        .record
        .is_publish_attempted_but_unapplied()
        .then_some(persisted))
}

/// Finish a run whose decision publication has already been attempted.
///
/// Nothing is re-derived from live state: the intended decision event, the
/// destination status, and the subject all come from the persisted record, so a
/// run cannot be stranded by an issue body or completion comment that changed
/// after the attempt. Two shapes exist:
///
/// * the decision comment node ID is durable — the published comment is
///   re-verified and the status applied; or
/// * the write outcome was never observed — the comments are listed and the
///   pinned event is reconciled against them with the same strict adoption rule
///   a first attempt uses: an exact match is adopted, a divergent comment
///   claiming the same event ID fails closed, and only an authoritative listing
///   without the event may publish the pinned comment.
///
/// Every failure here is a **hard error**, never a [`PermanentCandidateError`]:
/// the reaction stops with zero further side effects rather than skipping a row
/// whose decision may already be visible in the issue thread.
async fn resume_attempted_decision(
    reaction_name: &str,
    base: &ReactionBase,
    config: &WorkgraphRouterReactionConfig,
    github: &GithubClient,
    candidate: &RoutingCandidate,
    store: Arc<dyn StateStoreProvider>,
    mut persisted: PersistedRoutingRecord,
) -> anyhow::Result<()> {
    persisted
        .record
        .ensure_matches(candidate)
        .context("the attempted decision does not belong to this row")?;
    let run_id = persisted.record.run_id.clone();
    let to_status = persisted.record.to_status.clone();

    // An unobserved write is reconciled first, from the pinned event only.
    let mut observed: Option<IssueComment> = None;
    if persisted.record.decision_comment_node_id.is_none() {
        let intended = pinned_decision_event(&persisted.record)?;
        let body = render_pinned_decision(&persisted.record, &intended)?;
        warn!(
            "[{reaction_name}] run '{run_id}' attempted a decision comment whose outcome was never \
             observed; reconciling {}#{} against the persisted decision",
            persisted.record.repository, persisted.record.subject_number
        );
        observed = publish_decision_comment(
            base,
            config,
            github,
            store.clone(),
            &mut persisted,
            &intended,
            &body,
        )
        .await?;
    }

    // When nothing was observed in this pass the published comment is re-read.
    finish_published_decision(
        reaction_name,
        base,
        github,
        config,
        store,
        &mut persisted,
        observed,
    )
    .await?;

    info!(
        "[{reaction_name}] completed the published decision for {}#{}: run '{run_id}' -> '{to_status}'",
        persisted.record.repository, persisted.record.subject_number
    );
    Ok(())
}

/// The `RoutingDecided` event a record pinned before its first GitHub write.
///
/// Re-parsed under the strict grammar and required to still bind the record's
/// run, event, item, and subject: a record whose pinned event says anything
/// else is corrupt and must never produce a write.
fn pinned_decision_event(record: &RoutingRecord) -> anyhow::Result<WorkGraphEvent> {
    let event = WorkGraphEvent::from_json(&record.decision_event_json).map_err(|error| {
        anyhow::anyhow!(
            "the decision event pinned by run '{}' no longer parses: {error}",
            record.run_id
        )
    })?;
    if event.run_id.as_str() != record.run_id
        || event.event_id.as_str() != record.event_id
        || event.project_item_node_id != record.project_item_node_id
        || event.subject_node_id != record.subject_node_id
    {
        anyhow::bail!(
            "the decision event pinned by run '{}' does not bind run '{}', item '{}', and subject '{}'",
            record.run_id,
            record.run_id,
            record.project_item_node_id,
            record.subject_node_id
        );
    }
    let pinned_status = match &event.payload {
        WorkGraphEventPayload::RoutingDecided(decision) => decision.to_status.as_str().to_string(),
        other => anyhow::bail!(
            "the event pinned by run '{}' is a {} event, not a routing decision",
            record.run_id,
            other.event_type()
        ),
    };
    // The destination is read from the record, so a record that disagrees with
    // the decision it pinned is corrupt: publishing it would announce one
    // destination and move the item to another.
    if pinned_status != record.to_status {
        anyhow::bail!(
            "the decision pinned by run '{}' routes to '{pinned_status}' but the record names '{}'",
            record.run_id,
            record.to_status
        );
    }
    Ok(event)
}

/// Render the exact comment body a record's pinned decision must be published
/// as, from durable state alone.
fn render_pinned_decision(
    record: &RoutingRecord,
    intended: &WorkGraphEvent,
) -> anyhow::Result<String> {
    let summary = summary_for(intended);
    render_comment(intended, &summary)
        .map_err(|error| anyhow::anyhow!("failed to render the routing comment: {error}"))
}

/// Publish exactly one decision comment for a run, adopting an earlier write.
///
/// The comments are listed immediately before writing, so a decision comment
/// that landed since the last read is adopted rather than duplicated. Adoption
/// requires canonical event JSON byte-identical to `intended`; a divergent
/// comment claiming the same event ID fails closed.
///
/// When the authoritative listing does not carry the event, the write is
/// preceded by a durable "publication attempted" marker, so an outcome this
/// process never observes still leaves the run resumable from its pinned
/// decision instead of re-derivable from live state.
///
/// Returns the physical comment this pass observed (adopted or created), if
/// GitHub reported one, so the caller can verify it without another round trip.
async fn publish_decision_comment(
    base: &ReactionBase,
    config: &WorkgraphRouterReactionConfig,
    github: &GithubClient,
    store: Arc<dyn StateStoreProvider>,
    persisted: &mut PersistedRoutingRecord,
    intended: &WorkGraphEvent,
    body: &str,
) -> anyhow::Result<Option<IssueComment>> {
    let repository = persisted.record.repository.clone();
    let subject_number = persisted.record.subject_number;
    let run_id = persisted.record.run_id.clone();

    let latest = github
        .list_issue_comments(&repository, subject_number)
        .await
        .context("failed to list issue comments before posting the decision")?;
    let adopted = adopt_own_published_comment(config, &latest, intended)
        .context("routing-decision reconciliation failed")?;
    let (comment_node_id, published) = match adopted {
        Some(observation) => {
            info!(
                "[{}] adopted existing routing comment '{}' for run '{run_id}'",
                base.id, observation.comment_node_id
            );
            let physical = latest
                .iter()
                .find(|comment| comment.node_id == observation.comment_node_id)
                .cloned();
            (observation.comment_node_id, physical)
        }
        None => {
            // Durable intent to write, before the write: an unobserved outcome
            // must still be recognisable as "the decision may be published".
            if !persisted.record.decision_publish_attempted {
                let mut attempted = persisted.record.clone();
                attempted.mark_decision_publish_attempted();
                persist(store.clone(), &base.id, persisted, attempted).await?;
            }
            match github
                .create_issue_comment(&repository, subject_number, body)
                .await
            {
                Ok(comment) => (comment.node_id.clone(), Some(comment)),
                Err(error) => {
                    // The write may or may not have landed; mark the run
                    // ambiguous so the next attempt reconciles instead of
                    // blindly posting again.
                    let mut ambiguous = persisted.record.clone();
                    ambiguous.set_error(format!("{error:#}"), true);
                    persist(store.clone(), &base.id, persisted, ambiguous).await?;
                    return Err(error).context("failed to post the routing comment");
                }
            }
        }
    };
    let mut updated = persisted.record.clone();
    updated.set_decision_comment(comment_node_id);
    persist(store, &base.id, persisted, updated).await?;
    Ok(published)
}

/// Apply the final status move for a run whose decision comment is durable.
///
/// This is the **only** path that moves a Project item, so a status can never
/// move for a decision comment that was not verified first. The destination is
/// the persisted `to_status` — never a freshly derived one.
///
/// `observed` is the physical decision comment this pass already saw (adopted
/// or just created); when it is `None`, or does not match the recorded comment,
/// the comments are re-read from GitHub. Verification failures are hard errors.
async fn finish_published_decision(
    reaction_name: &str,
    base: &ReactionBase,
    github: &GithubClient,
    config: &WorkgraphRouterReactionConfig,
    store: Arc<dyn StateStoreProvider>,
    persisted: &mut PersistedRoutingRecord,
    observed: Option<IssueComment>,
) -> anyhow::Result<()> {
    if persisted.record.status_applied {
        return Ok(());
    }
    let comment_node_id = persisted
        .record
        .decision_comment_node_id
        .clone()
        .ok_or_else(|| anyhow::anyhow!("a published run must name its decision comment"))?;

    let comments = match observed {
        Some(comment) if comment.node_id == comment_node_id => vec![comment],
        _ => github
            .list_issue_comments(
                &persisted.record.repository,
                persisted.record.subject_number,
            )
            .await
            .context("failed to list issue comments to reconcile the published decision")?,
    };
    verify_published_decision(config, &persisted.record, &comment_node_id, &comments)?;
    // The routing table is fixed by the event contract: a record that names any
    // other destination is corrupt and must never move an item.
    if !is_decided_status(&persisted.record.to_status) {
        anyhow::bail!(
            "routing record for run '{}' names destination '{}', which is not a routing decision",
            persisted.record.run_id,
            persisted.record.to_status
        );
    }

    let item = ProjectItemRef {
        project_node_id: &persisted.record.project_node_id,
        project_item_node_id: &persisted.record.project_item_node_id,
        subject_node_id: &persisted.record.subject_node_id,
        repository: &persisted.record.repository,
        subject_number: persisted.record.subject_number,
    };
    let outcome = match github
        .update_project_status(item, ROUTABLE_STATUS, &persisted.record.to_status)
        .await
    {
        Ok(outcome) => outcome,
        Err(error) => {
            let mut ambiguous = persisted.record.clone();
            ambiguous.set_error(format!("{error:#}"), true);
            persist(store.clone(), &base.id, persisted, ambiguous).await?;
            return Err(error).context("failed to apply the routing decision");
        }
    };
    if outcome == UpdateStatusOutcome::AlreadyAtDestination {
        info!(
            "[{reaction_name}] project item '{}' was already at '{}'",
            persisted.record.project_item_node_id, persisted.record.to_status
        );
    }
    let mut updated = persisted.record.clone();
    updated.set_status_applied();
    persist(store, &base.id, persisted, updated).await
}

/// Point the Project item at `run_id` before this run's first external effect.
///
/// Refuses to take the item from a *different* run that has already attempted
/// its decision without applying the status: that run's decision may be visible
/// in the issue thread and must be finished, not abandoned. In the sequential
/// processing loop this cannot normally happen (such a run is resumed at step
/// 0), so it is a fail-closed guard against a concurrent writer, not a routine
/// branch.
async fn claim_open_run(
    store: Arc<dyn StateStoreProvider>,
    store_id: &str,
    candidate: &RoutingCandidate,
    run_id: &str,
) -> anyhow::Result<()> {
    if let Some(incumbent) = attempted_but_unapplied_run(store.clone(), store_id, candidate).await?
    {
        if incumbent.record.run_id != run_id {
            anyhow::bail!(
                "project item '{}' still owes the decision of run '{}'; refusing to start run '{run_id}'",
                candidate.project_item_node_id,
                incumbent.record.run_id
            );
        }
    }
    set_open_run(store, store_id, &candidate.project_item_node_id, run_id)
        .await
        .context("failed to record the open run for the project item")
}

/// Require the published decision comment to still be exactly what was decided.
fn verify_published_decision(
    config: &WorkgraphRouterReactionConfig,
    record: &RoutingRecord,
    comment_node_id: &str,
    comments: &[IssueComment],
) -> anyhow::Result<()> {
    let comment = comments
        .iter()
        .find(|comment| comment.node_id == comment_node_id)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "published decision comment '{comment_node_id}' for run '{}' no longer exists; \
                 refusing to complete the status move",
                record.run_id
            )
        })?;
    if !comment.is_authored_by(&config.trusted_author()) {
        anyhow::bail!(
            "published decision comment '{comment_node_id}' for run '{}' is no longer authored by the trusted author",
            record.run_id
        );
    }
    if !comment.is_unedited() {
        anyhow::bail!(
            "published decision comment '{comment_node_id}' for run '{}' was edited; refusing to complete the status move",
            record.run_id
        );
    }
    let parsed = parse_comment(&comment.body).map_err(|error| {
        anyhow::anyhow!(
            "published decision comment '{comment_node_id}' for run '{}' no longer parses: {error}",
            record.run_id
        )
    })?;
    if parsed.event.run_id.as_str() != record.run_id
        || parsed.event.event_id.as_str() != record.event_id
        || parsed.event.project_item_node_id != record.project_item_node_id
        || parsed.event.subject_node_id != record.subject_node_id
    {
        anyhow::bail!(
            "published decision comment '{comment_node_id}' no longer binds run '{}', item '{}', and subject '{}'",
            record.run_id,
            record.project_item_node_id,
            record.subject_node_id
        );
    }
    if parsed.event.to_canonical_json() != record.decision_event_json {
        anyhow::bail!(
            "published decision comment '{comment_node_id}' for run '{}' no longer carries the decided event; refusing to complete the status move",
            record.run_id
        );
    }
    Ok(())
}

/// Require the comment the row names to still be exactly what the row
/// delivered.
///
/// The comment is located by the row's `eventCommentNodeId` — never by scanning
/// for something completion-shaped — and must still be authored by the trusted
/// identity, be unedited, parse under the strict grammar, and carry canonical
/// event JSON byte-identical to the row's. A row can therefore never name one
/// comment while carrying another comment's event.
///
/// This is deliberately *not* the same check as accepting a completion: the
/// accepted completion is the earliest physical comment carrying that event
/// (see [`trusted_chain`]), which may be an earlier byte-identical duplicate of
/// the one the row named.
fn verify_named_completion_comment(
    config: &WorkgraphRouterReactionConfig,
    candidate: &RoutingCandidate,
    accepted: &WorkGraphEvent,
    comments: &[IssueComment],
) -> anyhow::Result<()> {
    let comment = comments
        .iter()
        .find(|comment| comment.node_id == candidate.event_comment_node_id)
        .ok_or_else(|| {
            PermanentCandidateError::new(format!(
                "completion comment '{}' no longer exists on {}#{}",
                candidate.event_comment_node_id, candidate.repository, candidate.subject_number
            ))
        })?;
    if !comment.is_authored_by(&config.trusted_author()) {
        return Err(PermanentCandidateError::new(format!(
            "completion comment '{}' is not authored by the trusted identity",
            candidate.event_comment_node_id
        )));
    }
    if !comment.is_unedited() {
        return Err(PermanentCandidateError::new(format!(
            "completion comment '{}' was edited",
            candidate.event_comment_node_id
        )));
    }
    let parsed = parse_comment(&comment.body).map_err(|error| {
        PermanentCandidateError::new(format!(
            "completion comment '{}' no longer parses: {error}",
            candidate.event_comment_node_id
        ))
    })?;
    if parsed.event.to_canonical_json() != accepted.to_canonical_json() {
        return Err(PermanentCandidateError::new(format!(
            "completion comment '{}' no longer carries the event the row delivered",
            candidate.event_comment_node_id
        )));
    }
    Ok(())
}

/// Require the rest of a complete, coherent, trusted event chain for one run,
/// and require the row's completion to be the one accepted completion.
///
/// Every step is verified against the *current* authoritative state:
///
/// * only comments authored by the configured trusted author (numeric database
///   ID + actor type) and reported unedited
///   are considered at all;
/// * the assignment must name the expected profile and must bind to the current
///   issue-body digest;
/// * an `ExecutionStarted` must exist, and the completion must carry the same
///   `executionId`;
/// * exactly one completion is accepted — byte-identical duplicates coalesce
///   and contradictory ones fail closed; and
/// * that accepted completion must carry **exactly** the event the row
///   delivered, so the router can never decide from an event the row did not
///   name.
fn trusted_chain(
    config: &WorkgraphRouterReactionConfig,
    candidate: &RoutingCandidate,
    accepted_row: &AcceptedEventRow,
    run: &drasi_workgraph_common::event::RunId,
    digest: &drasi_workgraph_common::event::Sha256Digest,
    comments: &[IssueComment],
) -> anyhow::Result<TrustedChain> {
    let assignment_id = event_id(run, WorkGraphEventType::ResponsibilityAssigned);
    let started_id = event_id(run, WorkGraphEventType::ExecutionStarted);
    let completed_id = event_id(run, WorkGraphEventType::CompletedIssueValidation);

    let assignment = accept_trusted_comment(config, comments, &assignment_id)
        .context("assignment reconciliation failed")?
        .ok_or_else(|| {
            PermanentCandidateError::new(
                "no trusted, unedited ResponsibilityAssigned comment exists for this run",
            )
        })?;
    let assignment_payload = match &assignment.comment.event.payload {
        WorkGraphEventPayload::ResponsibilityAssigned(payload) => payload.clone(),
        other => {
            return Err(PermanentCandidateError::new(format!(
                "assignment comment carries a {} payload",
                other.event_type()
            )))
        }
    };
    ensure_binding(candidate, &assignment.comment.event)?;
    if assignment_payload.profile_ref.profile() != config.expected_profile {
        return Err(PermanentCandidateError::new(format!(
            "assignment profile '{}' is not '{}'",
            assignment_payload.profile_ref.profile(),
            config.expected_profile
        )));
    }
    if &assignment_payload.content_digest != digest {
        return Err(PermanentCandidateError::new(
            "assignment content digest does not match the current issue body",
        ));
    }

    let started = accept_trusted_comment(config, comments, &started_id)
        .context("execution-start reconciliation failed")?
        .ok_or_else(|| {
            PermanentCandidateError::new(
                "no trusted, unedited ExecutionStarted comment exists for this run",
            )
        })?;
    let started_payload = match &started.comment.event.payload {
        WorkGraphEventPayload::ExecutionStarted(payload) => payload.clone(),
        other => {
            return Err(PermanentCandidateError::new(format!(
                "start comment carries a {} payload",
                other.event_type()
            )))
        }
    };
    ensure_binding(candidate, &started.comment.event)?;

    let completion = accept_trusted_comment(config, comments, &completed_id)
        .context("completion reconciliation failed")?
        .ok_or_else(|| {
            PermanentCandidateError::new(
                "no trusted, unedited CompletedIssueValidation comment exists for this run yet",
            )
        })?;
    // The row is authoritative about *which* completion this decision comes
    // from: the accepted comment must carry exactly that event, or the
    // completion has since been changed and nothing may be decided from it.
    if completion.comment.event.to_canonical_json() != accepted_row.event.to_canonical_json() {
        return Err(PermanentCandidateError::new(format!(
            "the accepted completion comment '{}' does not carry the completion event the row delivered",
            completion.comment_node_id
        )));
    }
    let completion_payload = match &completion.comment.event.payload {
        WorkGraphEventPayload::CompletedIssueValidation(payload) => payload.clone(),
        other => {
            return Err(PermanentCandidateError::new(format!(
                "completion comment carries a {} payload",
                other.event_type()
            )))
        }
    };
    ensure_binding(candidate, &completion.comment.event)?;
    if completion_payload.execution_id != started_payload.execution_id {
        return Err(PermanentCandidateError::new(format!(
            "completion reports execution '{}' but the started execution is '{}'",
            completion_payload.execution_id, started_payload.execution_id
        )));
    }

    // Hash the *exact* body GitHub reports for the accepted comment, not a
    // re-render of it: the record must be able to detect any later change to
    // that physical comment, including one GitHub does not flag as an edit.
    let accepted_body = comments
        .iter()
        .find(|comment| comment.node_id == completion.comment_node_id)
        .map(|comment| comment.body.as_str())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "accepted completion comment '{}' vanished from the listing",
                completion.comment_node_id
            )
        })?;

    Ok(TrustedChain {
        execution_id: started_payload.execution_id,
        accepted_completion: AcceptedCompletion {
            comment_node_id: completion.comment_node_id.clone(),
            body_hash: comment_body_hash(accepted_body),
        },
        completion: completion_payload,
    })
}

/// Require an event to name the same Project Item and subject as the row.
///
/// The `runId` already binds both (and the body digest), so a mismatch cannot
/// normally reach here; checking anyway keeps the binding explicit rather than
/// implied by a hash.
fn ensure_binding(candidate: &RoutingCandidate, event: &WorkGraphEvent) -> anyhow::Result<()> {
    if event.project_item_node_id != candidate.project_item_node_id {
        return Err(PermanentCandidateError::new(format!(
            "event names project item '{}', not the row's '{}'",
            event.project_item_node_id, candidate.project_item_node_id
        )));
    }
    if event.subject_node_id != candidate.subject_node_id {
        return Err(PermanentCandidateError::new(format!(
            "event names subject '{}', not the row's '{}'",
            event.subject_node_id, candidate.subject_node_id
        )));
    }
    Ok(())
}

/// Coalesce the trusted, unedited comments carrying `wanted_event_id`.
///
/// This is how the router reads events **other components** wrote:
/// byte-identical duplicates collapse to the earliest physical comment, and
/// conflicting content for one event ID fails closed.
fn accept_trusted_comment(
    config: &WorkgraphRouterReactionConfig,
    comments: &[IssueComment],
    wanted_event_id: &EventId,
) -> anyhow::Result<Option<ObservedComment>> {
    let observed = observed_comments(config, comments);
    let accepted = coalesce(&observed, wanted_event_id)
        .map_err(|error| anyhow::anyhow!("comment reconciliation failed: {error}"))?;
    Ok(accepted.cloned())
}

/// Adopt a decision comment this reaction already published.
///
/// Unlike [`accept_trusted_comment`], adoption requires canonical event JSON —
/// envelope *and* payload — byte-identical to `intended`, because the
/// deterministic `eventId` covers only the run and the event type. A single
/// divergent comment claiming that event ID, or two that disagree, fails closed
/// rather than being adopted as this reaction's own write.
fn adopt_own_published_comment(
    config: &WorkgraphRouterReactionConfig,
    comments: &[IssueComment],
    intended: &WorkGraphEvent,
) -> anyhow::Result<Option<ObservedComment>> {
    let observed = observed_comments(config, comments);
    let accepted = adopt_published_event(&observed, intended)
        .map_err(|error| anyhow::anyhow!("comment reconciliation failed: {error}"))?;
    Ok(accepted.cloned())
}

/// The trusted, unedited, parseable WorkGraph comments on an issue.
fn observed_comments(
    config: &WorkgraphRouterReactionConfig,
    comments: &[IssueComment],
) -> Vec<ObservedComment> {
    let trusted = config.trusted_author();
    comments
        .iter()
        .filter(|comment| comment.is_authored_by(&trusted))
        .filter(|comment| comment.is_unedited())
        .filter_map(|comment| {
            parse_comment(&comment.body)
                .ok()
                .map(|parsed| ObservedComment {
                    comment_node_id: comment.node_id.clone(),
                    comment: parsed,
                })
        })
        .collect()
}

/// Compare-and-swap `next` into the store, refreshing the in-memory witness.
async fn persist(
    store: Arc<dyn StateStoreProvider>,
    store_id: &str,
    persisted: &mut PersistedRoutingRecord,
    next: RoutingRecord,
) -> anyhow::Result<()> {
    let Some(bytes) = compare_and_swap_record(store, store_id, &persisted.bytes, &next).await?
    else {
        anyhow::bail!(
            "routing record for run '{}' changed underneath this writer",
            next.run_id
        );
    };
    persisted.record = next;
    persisted.bytes = bytes;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_workgraph_common::event::{
        AssignedResponsibilityType, ExecutionStartedPayload, ProfileRef, RoutingToStatus,
        ValidationOutcome, ValidationReasonCode,
    };
    use drasi_workgraph_common::trust::{ActorType, AuthorIdentity};

    const ITEM: &str = "PVTI_item";
    const SUBJECT: &str = "I_kwDOABCDEF6ABCDE";
    const BLOB: &str = "0123456789abcdef0123456789abcdef01234567";
    const BODY: &str = "Please validate. workgraph:validate";

    fn config() -> WorkgraphRouterReactionConfig {
        WorkgraphRouterReactionConfig {
            allowed_repositories: vec!["drasi-project/drasi-core".to_string()],
            allowed_projects: vec!["PVT_project".to_string()],
            expected_project_status_field_node_id: "PVTSSF_status".to_string(),
            trusted_author_database_id: 4021243,
            trusted_author_type: ActorType::Bot,
            ..WorkgraphRouterReactionConfig::default()
        }
    }

    /// A row carrying the exact completion comment named by `execution`.
    fn candidate_for(outcome: ValidationOutcome, execution: &str) -> RoutingCandidate {
        RoutingCandidate {
            repository: "drasi-project/drasi-core".to_string(),
            subject_number: 742,
            subject_node_id: SUBJECT.to_string(),
            project_node_id: "PVT_project".to_string(),
            project_item_node_id: ITEM.to_string(),
            project_status: ROUTABLE_STATUS.to_string(),
            body_digest: body_digest(Some(BODY)).as_str().to_string(),
            event_comment_node_id: "IC_complete".to_string(),
            event_body: event_body(&completion_event(outcome, execution)),
            author_database_id: 4021243,
            author_type: "Bot".to_string(),
            is_edited: false,
        }
    }

    fn candidate() -> RoutingCandidate {
        candidate_for(ValidationOutcome::Passed, "exec-1")
    }

    /// The canonical comment body for an event.
    fn event_body(event: &WorkGraphEvent) -> String {
        let summary = summary_for(event);
        render_comment(event, &summary).expect("render")
    }

    fn trusted_identity() -> AuthorIdentity {
        AuthorIdentity::new(4021243, ActorType::Bot)
            .with_author_id("U_trusted")
            .with_login("workgraph-bot")
    }

    fn comment(node_id: &str, event: &WorkGraphEvent, identity: AuthorIdentity) -> IssueComment {
        let summary = summary_for(event);
        IssueComment {
            node_id: node_id.to_string(),
            body: render_comment(event, &summary).expect("render"),
            author: Some(identity),
            created_at: Some("2026-08-14T00:00:00Z".to_string()),
            updated_at: Some("2026-08-14T00:00:00Z".to_string()),
        }
    }

    fn run() -> drasi_workgraph_common::event::RunId {
        drasi_workgraph_common::ids::run_id(ITEM, &body_digest(Some(BODY)))
    }

    fn assignment_event() -> WorkGraphEvent {
        WorkGraphEvent::new(
            run(),
            ITEM,
            SUBJECT,
            WorkGraphEventPayload::ResponsibilityAssigned(ResponsibilityAssignedPayload {
                responsibility_type: AssignedResponsibilityType::IssueValidation,
                profile_ref: ProfileRef::new("issue-validator", BLOB).expect("profile"),
                content_digest: body_digest(Some(BODY)),
            }),
        )
        .expect("event")
    }

    fn started_event() -> WorkGraphEvent {
        WorkGraphEvent::new(
            run(),
            ITEM,
            SUBJECT,
            WorkGraphEventPayload::ExecutionStarted(ExecutionStartedPayload {
                execution_id: ExecutionId::from_run_id(&run()),
                task_id: "task-1".to_string(),
            }),
        )
        .expect("event")
    }

    fn completion_event(outcome: ValidationOutcome, execution: &str) -> WorkGraphEvent {
        let reason = match outcome {
            ValidationOutcome::Passed => ValidationReasonCode::RequiredMarkerPresent,
            ValidationOutcome::Failed => ValidationReasonCode::RequiredMarkerMissing,
        };
        WorkGraphEvent::new(
            run(),
            ITEM,
            SUBJECT,
            WorkGraphEventPayload::CompletedIssueValidation(CompletedIssueValidationPayload {
                execution_id: if execution == "exec-1" {
                    ExecutionId::from_run_id(&run())
                } else {
                    let other_run =
                        drasi_workgraph_common::ids::run_id("PVTI_other", &body_digest(Some(BODY)));
                    ExecutionId::from_run_id(&other_run)
                },
                outcome,
                reason_code: reason,
            }),
        )
        .expect("event")
    }

    fn full_chain(outcome: ValidationOutcome) -> Vec<IssueComment> {
        vec![
            comment("IC_assign", &assignment_event(), trusted_identity()),
            comment("IC_start", &started_event(), trusted_identity()),
            comment(
                "IC_complete",
                &completion_event(outcome, "exec-1"),
                trusted_identity(),
            ),
        ]
    }

    /// Run the chain check for `candidate` against `comments`.
    fn chain_with(
        config: &WorkgraphRouterReactionConfig,
        candidate: &RoutingCandidate,
        digest: &drasi_workgraph_common::event::Sha256Digest,
        comments: &[IssueComment],
    ) -> anyhow::Result<TrustedChain> {
        let accepted = candidate
            .accept_completion(config)
            .map_err(|error| PermanentCandidateError::new(error.to_string()))?;
        trusted_chain(config, candidate, &accepted, &run(), digest, comments)
    }

    fn chain_for(comments: &[IssueComment]) -> anyhow::Result<TrustedChain> {
        chain_with(&config(), &candidate(), &body_digest(Some(BODY)), comments)
    }

    /// The chain check for a row whose completion names `outcome`.
    fn chain_for_outcome(
        outcome: ValidationOutcome,
        comments: &[IssueComment],
    ) -> anyhow::Result<TrustedChain> {
        chain_with(
            &config(),
            &candidate_for(outcome, "exec-1"),
            &body_digest(Some(BODY)),
            comments,
        )
    }

    #[test]
    fn a_complete_trusted_chain_yields_the_completion() {
        let chain = chain_for(&full_chain(ValidationOutcome::Passed)).expect("chain");
        assert_eq!(chain.completion.outcome, ValidationOutcome::Passed);
        assert_eq!(chain.execution_id, ExecutionId::from_run_id(&run()),);
        assert_eq!(chain.accepted_completion.comment_node_id, "IC_complete");
        assert!(chain.accepted_completion.body_hash.starts_with("sha256:"));
    }

    #[test]
    fn routing_maps_outcomes_to_the_two_final_statuses() {
        let passed = chain_for(&full_chain(ValidationOutcome::Passed)).expect("chain");
        let decision = RoutingDecidedPayload::for_outcome(passed.completion.outcome);
        assert_eq!(
            decision.to_status,
            RoutingToStatus::AwaitingIssueRiskProfiling
        );

        let failed = chain_for_outcome(
            ValidationOutcome::Failed,
            &full_chain(ValidationOutcome::Failed),
        )
        .expect("chain");
        let decision = RoutingDecidedPayload::for_outcome(failed.completion.outcome);
        assert_eq!(decision.to_status, RoutingToStatus::NeedsMoreInformation);
    }

    #[test]
    fn each_link_of_the_chain_is_required() {
        for missing in ["IC_assign", "IC_start", "IC_complete"] {
            let comments: Vec<IssueComment> = full_chain(ValidationOutcome::Passed)
                .into_iter()
                .filter(|comment| comment.node_id != missing)
                .collect();
            let error = chain_for(&comments).expect_err("incomplete chain");
            assert!(
                error.downcast_ref::<PermanentCandidateError>().is_some(),
                "missing '{missing}' must be a permanent rejection: {error}"
            );
        }
    }

    #[test]
    fn untrusted_and_edited_comments_are_invisible() {
        let untrusted = AuthorIdentity::new(66, ActorType::User).with_author_id("U_mallory");
        let mut comments = full_chain(ValidationOutcome::Passed);
        comments[2] = comment(
            "IC_complete",
            &completion_event(ValidationOutcome::Passed, "exec-1"),
            untrusted,
        );
        assert!(
            chain_for(&comments).is_err(),
            "untrusted completion ignored"
        );

        // The trusted numeric database ID under the wrong actor type is not
        // the trusted author.
        let wrong_type = AuthorIdentity::new(4021243, ActorType::User).with_author_id("U_trusted");
        let mut comments = full_chain(ValidationOutcome::Passed);
        comments[2] = comment(
            "IC_complete",
            &completion_event(ValidationOutcome::Passed, "exec-1"),
            wrong_type,
        );
        assert!(chain_for(&comments).is_err(), "wrong actor type ignored");

        // An edited completion is ignored entirely.
        let mut comments = full_chain(ValidationOutcome::Passed);
        comments[2].updated_at = Some("2026-08-14T01:00:00Z".to_string());
        assert!(chain_for(&comments).is_err(), "edited completion ignored");
    }

    #[test]
    fn a_renamed_login_and_a_missing_node_id_do_not_break_the_chain() {
        let renamed = AuthorIdentity::new(4021243, ActorType::Bot)
            .with_author_id("U_trusted")
            .with_login("renamed-since-then");
        let mut comments = full_chain(ValidationOutcome::Passed);
        comments[2] = comment(
            "IC_complete",
            &completion_event(ValidationOutcome::Passed, "exec-1"),
            renamed,
        );
        chain_for(&comments).expect("logins are display-only");

        // The node ID is audit data: an author reported without one is still
        // the trusted author.
        let no_node_id = AuthorIdentity::new(4021243, ActorType::Bot);
        let mut comments = full_chain(ValidationOutcome::Passed);
        comments[2] = comment(
            "IC_complete",
            &completion_event(ValidationOutcome::Passed, "exec-1"),
            no_node_id,
        );
        chain_for(&comments).expect("node IDs are audit data");
    }

    #[test]
    fn a_completion_from_another_execution_is_rejected() {
        let run = run();
        let other_run = drasi_workgraph_common::ids::run_id("PVTI_other", &body_digest(Some(BODY)));
        let error = WorkGraphEvent::new(
            run,
            ITEM,
            SUBJECT,
            WorkGraphEventPayload::CompletedIssueValidation(CompletedIssueValidationPayload {
                execution_id: ExecutionId::from_run_id(&other_run),
                outcome: ValidationOutcome::Passed,
                reason_code: ValidationReasonCode::RequiredMarkerPresent,
            }),
        )
        .expect_err("execution mismatch must fail event validation");
        assert!(error.to_string().contains("executionId"), "{error}");
    }

    #[test]
    fn a_completion_comment_that_diverges_from_the_row_is_rejected() {
        // The issue thread's accepted completion says 'failed' while the row
        // delivered a 'passed' completion for the same event ID: the router must
        // never decide from an event the row did not name.
        let mut comments = full_chain(ValidationOutcome::Passed);
        comments[2] = comment(
            "IC_complete",
            &completion_event(ValidationOutcome::Failed, "exec-1"),
            trusted_identity(),
        );
        let error = chain_for(&comments).expect_err("divergent completion");
        assert!(
            error
                .to_string()
                .contains("does not carry the completion event the row delivered"),
            "{error}"
        );
    }

    #[test]
    fn a_stale_body_digest_breaks_the_run_binding() {
        // The assignment for a *different* body digest has a different runId
        // and therefore a different eventId: the chain simply does not exist.
        let other_digest = body_digest(Some("a different body"));
        let error = chain_with(
            &config(),
            &candidate(),
            &other_digest,
            &full_chain(ValidationOutcome::Passed),
        )
        .expect_err("digest mismatch");
        assert!(error.to_string().contains("content digest"), "{error}");
    }

    #[test]
    fn a_foreign_profile_is_not_routed() {
        let mut config = config();
        config.expected_profile = "issue-risk-profiler".to_string();
        let error = chain_with(
            &config,
            &candidate(),
            &body_digest(Some(BODY)),
            &full_chain(ValidationOutcome::Passed),
        )
        .expect_err("foreign profile");
        assert!(error.to_string().contains("profile"), "{error}");
    }

    #[test]
    fn identical_duplicate_completions_coalesce_and_conflicts_fail_closed() {
        let mut comments = full_chain(ValidationOutcome::Passed);
        comments.push(comment(
            "IC_complete_dup",
            &completion_event(ValidationOutcome::Passed, "exec-1"),
            trusted_identity(),
        ));
        let chain = chain_for(&comments).expect("duplicates coalesce");
        assert_eq!(
            chain.accepted_completion.comment_node_id, "IC_complete",
            "the earliest physical comment is accepted"
        );

        let mut comments = full_chain(ValidationOutcome::Passed);
        comments.push(comment(
            "IC_complete_conflict",
            &completion_event(ValidationOutcome::Failed, "exec-1"),
            trusted_identity(),
        ));
        let error = chain_for(&comments).expect_err("conflict must fail closed");
        assert!(
            error.downcast_ref::<PermanentCandidateError>().is_none(),
            "a contradiction must halt the reaction, not skip the row: {error:#}"
        );
        assert!(format!("{error:#}").contains("conflicting"), "{error:#}");
    }

    #[test]
    fn a_misbound_event_is_rejected() {
        let mut row = candidate();
        row.project_item_node_id = "PVTI_other".to_string();
        // The row's item no longer matches the event it carries, so the row is
        // never accepted in the first place.
        let error = chain_with(
            &config(),
            &row,
            &body_digest(Some(BODY)),
            &full_chain(ValidationOutcome::Passed),
        )
        .expect_err("misbound row");
        assert!(error.to_string().contains("project item"), "{error}");
    }

    #[test]
    fn a_row_must_name_the_comment_that_carries_its_event() {
        let comments = full_chain(ValidationOutcome::Passed);
        let row = candidate();
        let accepted = row.accept_completion(&config()).expect("accepted");

        verify_named_completion_comment(&config(), &row, &accepted.event, &comments)
            .expect("the named completion comment carries the row's event");

        // Naming the assignment comment while carrying a completion event.
        let mut misnamed = candidate();
        misnamed.event_comment_node_id = "IC_assign".to_string();
        let error =
            verify_named_completion_comment(&config(), &misnamed, &accepted.event, &comments)
                .expect_err("a row may not name a comment carrying another event");
        assert!(
            error
                .to_string()
                .contains("no longer carries the event the row delivered"),
            "{error}"
        );
        assert!(
            error.downcast_ref::<PermanentCandidateError>().is_some(),
            "a mis-named row is skippable, not a halt: {error}"
        );

        // Naming a comment that is not on the issue at all.
        let mut missing = candidate();
        missing.event_comment_node_id = "IC_deleted".to_string();
        assert!(
            verify_named_completion_comment(&config(), &missing, &accepted.event, &comments)
                .expect_err("a deleted completion comment is rejected")
                .to_string()
                .contains("no longer exists")
        );

        // The named comment is untrusted or edited.
        let untrusted = comment(
            "IC_complete",
            &completion_event(ValidationOutcome::Passed, "exec-1"),
            AuthorIdentity::new(66, ActorType::User),
        );
        assert!(
            verify_named_completion_comment(&config(), &row, &accepted.event, &[untrusted])
                .expect_err("an untrusted completion comment is rejected")
                .to_string()
                .contains("trusted identity")
        );

        let mut edited = comments[2].clone();
        edited.updated_at = Some("2026-08-14T03:00:00Z".to_string());
        assert!(
            verify_named_completion_comment(&config(), &row, &accepted.event, &[edited])
                .expect_err("an edited completion comment is rejected")
                .to_string()
                .contains("was edited")
        );
    }

    #[test]
    fn decided_statuses_are_the_only_destinations_this_reaction_may_set() {
        assert!(is_decided_status("AwaitingIssueRiskProfiling"));
        assert!(is_decided_status("NeedsMoreInformation"));
        assert!(!is_decided_status("AwaitingRouting"));
        assert!(!is_decided_status(ROUTABLE_STATUS));
        assert!(!is_decided_status("Done"));
    }

    fn decision_event(outcome: ValidationOutcome) -> WorkGraphEvent {
        WorkGraphEvent::new(
            run(),
            ITEM,
            SUBJECT,
            WorkGraphEventPayload::RoutingDecided(RoutingDecidedPayload::for_outcome(outcome)),
        )
        .expect("event")
    }

    fn published_record(event: &WorkGraphEvent, comment_node_id: &str) -> RoutingRecord {
        let mut record = intent_record(event);
        record.set_decision_comment(comment_node_id);
        record
    }

    #[test]
    fn a_published_decision_is_verified_against_the_persisted_decision() {
        let event = decision_event(ValidationOutcome::Passed);
        let record = published_record(&event, "IC_decision");
        let published = comment("IC_decision", &event, trusted_identity());
        verify_published_decision(
            &config(),
            &record,
            "IC_decision",
            std::slice::from_ref(&published),
        )
        .expect("an unchanged published decision verifies");

        // Deleted.
        let error = verify_published_decision(&config(), &record, "IC_decision", &[])
            .expect_err("a deleted decision comment must halt");
        assert!(error.to_string().contains("no longer exists"), "{error}");

        // Edited.
        let mut edited = published.clone();
        edited.updated_at = Some("2026-08-14T02:00:00Z".to_string());
        let error = verify_published_decision(&config(), &record, "IC_decision", &[edited])
            .expect_err("an edited decision comment must halt");
        assert!(error.to_string().contains("was edited"), "{error}");

        // Authored by somebody else.
        let untrusted = comment(
            "IC_decision",
            &event,
            AuthorIdentity::new(66, ActorType::User).with_author_id("U_mallory"),
        );
        let error = verify_published_decision(&config(), &record, "IC_decision", &[untrusted])
            .expect_err("an untrusted decision comment must halt");
        assert!(error.to_string().contains("trusted author"), "{error}");

        // Same event ID, different payload: the eventId does not cover it.
        let divergent = decision_event(ValidationOutcome::Failed);
        assert_eq!(divergent.event_id, event.event_id);
        let swapped = comment("IC_decision", &divergent, trusted_identity());
        let error = verify_published_decision(&config(), &record, "IC_decision", &[swapped])
            .expect_err("a divergent decision comment must halt");
        assert!(
            error
                .to_string()
                .contains("no longer carries the decided event"),
            "{error}"
        );

        // A comment that no longer parses at all.
        let mut unparseable = published.clone();
        unparseable.body = "WorkGraphEvent/v1\n\nstill official\n\nnot json".to_string();
        let error = verify_published_decision(&config(), &record, "IC_decision", &[unparseable])
            .expect_err("an unparseable decision comment must halt");
        assert!(error.to_string().contains("no longer parses"), "{error}");

        // A comment carrying a different run entirely.
        let other_run = WorkGraphEvent::new(
            drasi_workgraph_common::ids::run_id(ITEM, &body_digest(Some("another body"))),
            ITEM,
            SUBJECT,
            WorkGraphEventPayload::RoutingDecided(RoutingDecidedPayload::for_outcome(
                ValidationOutcome::Passed,
            )),
        )
        .expect("event");
        let rebound = comment("IC_decision", &other_run, trusted_identity());
        let error = verify_published_decision(&config(), &record, "IC_decision", &[rebound])
            .expect_err("a re-bound decision comment must halt");
        assert!(error.to_string().contains("no longer binds run"), "{error}");
    }

    #[test]
    fn adoption_of_a_divergent_decision_fails_closed() {
        let intended = decision_event(ValidationOutcome::Passed);
        let divergent = decision_event(ValidationOutcome::Failed);
        let comments = vec![comment("IC_divergent", &divergent, trusted_identity())];

        // Reading by event ID alone would hand back the divergent comment...
        assert_eq!(
            accept_trusted_comment(&config(), &comments, &intended.event_id)
                .expect("no conflict among one comment")
                .expect("one observation")
                .comment_node_id,
            "IC_divergent"
        );
        // ...but adopting it as our own published decision must fail closed.
        let error = adopt_own_published_comment(&config(), &comments, &intended)
            .expect_err("divergent adoption must fail");
        assert!(
            format!("{error:#}").contains("differs from the event"),
            "{error:#}"
        );

        // The intended decision itself is adoptable.
        let ours = vec![comment("IC_ours", &intended, trusted_identity())];
        assert_eq!(
            adopt_own_published_comment(&config(), &ours, &intended)
                .expect("adoptable")
                .expect("one observation")
                .comment_node_id,
            "IC_ours"
        );
    }

    /// Seed a run's record and the item's open-run pointer.
    async fn seed_open_run(
        store: &Arc<dyn StateStoreProvider>,
        record: &RoutingRecord,
    ) -> anyhow::Result<()> {
        create_record_if_absent(store.clone(), "router", record).await?;
        set_open_run(
            store.clone(),
            "router",
            &record.project_item_node_id,
            &record.run_id,
        )
        .await
    }

    fn intent_record(event: &WorkGraphEvent) -> RoutingRecord {
        RoutingRecord::new(
            run().as_str(),
            event.event_id.as_str(),
            &candidate(),
            body_digest(Some(BODY)).as_str(),
            AcceptedCompletion {
                comment_node_id: "IC_complete".to_string(),
                body_hash: comment_body_hash("body"),
            },
            "passed",
            RoutingToStatus::AwaitingIssueRiskProfiling.as_str(),
            &event.to_canonical_json(),
        )
    }

    #[tokio::test]
    async fn an_attempted_but_unobserved_decision_still_owns_the_item() {
        use drasi_lib::state_store::MemoryStateStoreProvider;

        let event = decision_event(ValidationOutcome::Passed);

        // No pointer at all: the normal derivation path applies.
        let store: Arc<dyn StateStoreProvider> = Arc::new(MemoryStateStoreProvider::new());
        create_record_if_absent(store.clone(), "router", &intent_record(&event))
            .await
            .expect("seed record");
        assert!(
            attempted_but_unapplied_run(store, "router", &candidate())
                .await
                .expect("select")
                .is_none(),
            "a run without an open-run pointer does not own the item"
        );

        // Pointer, but the run has not reached its write yet.
        let store: Arc<dyn StateStoreProvider> = Arc::new(MemoryStateStoreProvider::new());
        seed_open_run(&store, &intent_record(&event))
            .await
            .expect("seed");
        assert!(
            attempted_but_unapplied_run(store, "router", &candidate())
                .await
                .expect("select")
                .is_none(),
            "a pre-publication run is re-derived, not resumed"
        );

        // Publication was attempted and its outcome never observed: this is the
        // state a fresh derivation must never skip.
        let store: Arc<dyn StateStoreProvider> = Arc::new(MemoryStateStoreProvider::new());
        let mut attempted = intent_record(&event);
        attempted.mark_decision_publish_attempted();
        attempted.set_error("create comment request failed: operation timed out", true);
        seed_open_run(&store, &attempted).await.expect("seed");
        let selected = attempted_but_unapplied_run(store, "router", &candidate())
            .await
            .expect("select")
            .expect("an attempted decision owns the item");
        assert!(selected.record.decision_comment_node_id.is_none());
        assert!(selected.record.decision_publish_attempted);

        // A published decision still owns the item.
        let store: Arc<dyn StateStoreProvider> = Arc::new(MemoryStateStoreProvider::new());
        let mut published = intent_record(&event);
        published.set_decision_comment("IC_decision");
        seed_open_run(&store, &published).await.expect("seed");
        assert!(attempted_but_unapplied_run(store, "router", &candidate())
            .await
            .expect("select")
            .is_some());

        // A finished run releases it.
        let store: Arc<dyn StateStoreProvider> = Arc::new(MemoryStateStoreProvider::new());
        let mut complete = intent_record(&event);
        complete.set_decision_comment("IC_decision");
        complete.set_status_applied();
        seed_open_run(&store, &complete).await.expect("seed");
        assert!(
            attempted_but_unapplied_run(store, "router", &candidate())
                .await
                .expect("select")
                .is_none(),
            "a completed run must not wedge the item"
        );
    }

    #[tokio::test]
    async fn a_new_run_may_not_take_an_item_that_owes_an_attempted_decision() {
        use drasi_lib::state_store::MemoryStateStoreProvider;

        let store: Arc<dyn StateStoreProvider> = Arc::new(MemoryStateStoreProvider::new());
        let mut attempted = intent_record(&decision_event(ValidationOutcome::Passed));
        attempted.mark_decision_publish_attempted();
        seed_open_run(&store, &attempted).await.expect("seed");

        let error = claim_open_run(store.clone(), "router", &candidate(), "run:another")
            .await
            .expect_err("a different run must not take the item");
        assert!(
            format!("{error:#}").contains("still owes the decision of run"),
            "{error:#}"
        );

        // The owning run itself re-claims freely.
        claim_open_run(store, "router", &candidate(), &attempted.run_id)
            .await
            .expect("the owning run keeps the item");
    }

    #[test]
    fn a_resumed_run_republishes_exactly_the_pinned_decision() {
        let event = decision_event(ValidationOutcome::Passed);
        let record = intent_record(&event);

        let pinned = pinned_decision_event(&record).expect("the pinned event parses");
        assert_eq!(pinned.to_canonical_json(), event.to_canonical_json());
        assert_eq!(
            render_pinned_decision(&record, &pinned).expect("render"),
            event_body(&event),
            "a resumed run must publish byte-identically to a first attempt"
        );

        // A record whose pinned event is corrupt, mis-bound, or not a decision
        // must never produce a write.
        let mut unparseable = record.clone();
        unparseable.decision_event_json = "{".to_string();
        assert!(pinned_decision_event(&unparseable)
            .expect_err("corrupt JSON")
            .to_string()
            .contains("no longer parses"));

        let mut rebound = record.clone();
        rebound.event_id = "event:something-else".to_string();
        assert!(pinned_decision_event(&rebound)
            .expect_err("mis-bound event")
            .to_string()
            .contains("does not bind run"));

        let mut wrong_type = record.clone();
        wrong_type.decision_event_json = started_event().to_canonical_json();
        wrong_type.run_id = started_event().run_id.as_str().to_string();
        wrong_type.event_id = started_event().event_id.as_str().to_string();
        assert!(pinned_decision_event(&wrong_type)
            .expect_err("not a routing decision")
            .to_string()
            .contains("not a routing decision"));

        // The record's destination must agree with the decision it pinned:
        // publishing one destination while moving the item to another is a
        // corruption the resume path must never act on.
        let mut mismatched = record.clone();
        mismatched.to_status = RoutingToStatus::NeedsMoreInformation.as_str().to_string();
        assert!(pinned_decision_event(&mismatched)
            .expect_err("destination mismatch")
            .to_string()
            .contains("but the record names"));
    }
}
