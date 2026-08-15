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

//! The admission reaction.
//!
//! For one eligible Project Item + Issue row it:
//!
//! 1. re-reads the authoritative issue and derives the body digest and `runId`;
//! 2. consults the durable record for that `runId` — a completed run is a
//!    no-op, and an in-flight one is resumed from its own recorded, immutable
//!    profile pin rather than from wherever the mutable profile path points now;
//! 3. for a genuinely new run, pins the agent profile to an immutable blob SHA
//!    and writes a durable intent record **before** touching GitHub;
//! 4. posts exactly one `ResponsibilityAssigned` comment (adopting one that a
//!    previous attempt may already have written); and
//! 5. sets the Project status to `AwaitingValidation`.
//!
//! Each side effect is reconciled and persisted before the next retry, so a
//! crash at any point resumes without duplicating an external effect.

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
    dedup::{adopt_published_event, ObservedComment},
    event::{
        AssignedResponsibilityType, EventError, ProfileRef, ResponsibilityAssignedPayload, RunId,
        Sha256Digest, WorkGraphEvent, WorkGraphEventPayload,
    },
    ids::{body_digest, run_id},
    summary::{summary_for, SubjectRef},
};
use log::{error, info, warn};

use crate::candidate::AdmissionCandidate;
use crate::config::{WorkgraphAdmissionReactionConfig, ADMITTED_STATUS};
use crate::github::{GithubClient, ProjectItemRef, UpdateStatusOutcome};
use crate::state::{
    compare_and_swap_record, create_record_if_absent, load_record, AdmissionRecord,
    PersistedAdmissionRecord,
};
use crate::WorkgraphAdmissionReactionBuilder;

/// A row that can never succeed, no matter how often it is retried.
///
/// Permanent rejections are logged and skipped; they have no external effect,
/// so unlike transient failures they do not need a durable tombstone to stay
/// consistent across replays.
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

/// The WorkGraph admission reaction.
pub struct WorkgraphAdmissionReaction {
    pub(crate) base: ReactionBase,
    pub(crate) config: WorkgraphAdmissionReactionConfig,
}

impl WorkgraphAdmissionReaction {
    /// Start building a reaction.
    pub fn builder(id: impl Into<String>) -> WorkgraphAdmissionReactionBuilder {
        WorkgraphAdmissionReactionBuilder::new(id)
    }

    pub(crate) fn from_builder(
        id: String,
        queries: Vec<String>,
        config: WorkgraphAdmissionReactionConfig,
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
impl Reaction for WorkgraphAdmissionReaction {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "workgraph-admission"
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
        log_component_start("WorkGraph Admission Reaction", &self.base.id);
        self.config
            .validate(&self.base.queries)
            .context("invalid workgraph-admission config")?;

        self.base
            .set_status(
                ComponentStatus::Starting,
                Some("Starting workgraph-admission reaction".to_string()),
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
                Some("Workgraph admission running".to_string()),
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
    config: WorkgraphAdmissionReactionConfig,
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
                "[{reaction_name}] admission failed for query '{}' sequence {}: {error:#}",
                event.query_id, event.sequence
            );
            base.set_status(
                ComponentStatus::Error,
                Some(format!("Workgraph-admission failed: {error:#}")),
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
                Some(format!("Workgraph-admission checkpoint failure: {error:#}")),
            )
            .await;
            return;
        }
    }
}

async fn process_query_result(
    reaction_name: &str,
    base: &ReactionBase,
    config: &WorkgraphAdmissionReactionConfig,
    github: &GithubClient,
    result: &QueryResult,
) -> anyhow::Result<()> {
    for diff in &result.results {
        match diff {
            ResultDiff::Add { data, .. } => {
                let candidate: AdmissionCandidate = match serde_json::from_value(data.clone()) {
                    Ok(candidate) => candidate,
                    Err(error) => {
                        warn!(
                            "[{reaction_name}] skipping malformed admission row on query '{}': {error}",
                            result.query_id
                        );
                        continue;
                    }
                };
                match admit(reaction_name, base, config, github, &candidate).await {
                    Ok(()) => {}
                    Err(error) if error.downcast_ref::<PermanentCandidateError>().is_some() => {
                        warn!(
                            "[{reaction_name}] rejecting admission row for {}#{}: {error}",
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

/// Admit one candidate, resuming any partially completed prior attempt.
async fn admit(
    reaction_name: &str,
    base: &ReactionBase,
    config: &WorkgraphAdmissionReactionConfig,
    github: &GithubClient,
    candidate: &AdmissionCandidate,
) -> anyhow::Result<()> {
    candidate
        .validate(config)
        .map_err(|error| PermanentCandidateError::new(error.to_string()))?;

    let store = base.state_store().await.ok_or_else(|| {
        anyhow::anyhow!("a durable state store is required for workgraph-admission")
    })?;

    // 1. Authoritative issue read. The body digest — and therefore the run
    //    identity — must come from GitHub, never from the query row.
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
    if !issue.state.eq_ignore_ascii_case("open") {
        return Err(PermanentCandidateError::new(format!(
            "{}#{} is '{}', not open",
            candidate.repository, candidate.subject_number, issue.state
        )));
    }

    let digest = body_digest(issue.body.as_deref());
    let run = run_id(
        &candidate.project_item_node_id,
        &candidate.subject_node_id,
        &digest,
    );

    let item = ProjectItemRef {
        project_node_id: &candidate.project_node_id,
        project_item_node_id: &candidate.project_item_node_id,
        subject_node_id: &candidate.subject_node_id,
        repository: &candidate.repository,
        subject_number: candidate.subject_number,
    };

    // 2. Durable state decides everything that follows. It is consulted before
    //    the mutable profile path is resolved, because for a run that already
    //    exists the record — not the current tip of `profileBaseRef` — is the
    //    authority on which immutable blob this run was assigned. Reading the
    //    live pin first and comparing it against the record would turn ordinary
    //    profile drift into a permanent wedge for every replay of an in-flight
    //    or already-completed run.
    let existing = load_record(store.clone(), &base.id, run.as_str()).await?;
    let mut persisted = match existing {
        Some(existing) => {
            existing
                .record
                .ensure_bound_to(candidate, run.as_str(), digest.as_str())?;
            if existing.record.is_complete() {
                // Both side effects are durable: nothing external is left to do,
                // so the profile is never resolved and no other GitHub read is
                // needed to decide this.
                info!(
                    "[{reaction_name}] run '{}' for {}#{} is already admitted; nothing to do",
                    run, candidate.repository, candidate.subject_number
                );
                return Ok(());
            }
            preflight_project_item(config, github, candidate, item).await?;
            existing
        }
        None => {
            // 3. A genuinely new run. Preflight the Project binding and status
            //    first — this is a read, so the side-effect order is still
            //    comment-then-status, but an ineligible or mis-bound item never
            //    receives an assignment comment — then pin the profile to an
            //    immutable blob so the assignment names an exact file revision
            //    rather than a mutable path.
            preflight_project_item(config, github, candidate, item).await?;
            let blob_sha = github
                .profile_blob_sha(
                    &candidate.repository,
                    &config.profile_path(),
                    &config.profile_base_ref,
                )
                .await
                .context("failed to pin the agent profile blob")?;
            let profile_ref =
                ProfileRef::new(&config.agent_profile, &blob_sha).map_err(|error| {
                    PermanentCandidateError::new(format!("agent profile is not pinnable: {error}"))
                })?;
            let event = assignment_event(&run, candidate, &profile_ref, &digest)
                .map_err(|error| PermanentCandidateError::new(error.to_string()))?;

            // 4. Durable intent before any external effect.
            let intent = AdmissionRecord::new(
                run.as_str(),
                event.event_id.as_str(),
                candidate,
                digest.as_str(),
                profile_ref.as_str(),
            );
            match create_record_if_absent(store.clone(), &base.id, &intent).await? {
                // A concurrent creator won. Its record is the published intent
                // for this run, so resume that pin instead of rejecting on the
                // pin just resolved here.
                Some(winner) => {
                    winner
                        .record
                        .ensure_bound_to(candidate, run.as_str(), digest.as_str())?;
                    if winner.record.is_complete() {
                        info!(
                            "[{reaction_name}] run '{}' for {}#{} is already admitted; nothing to do",
                            run, candidate.repository, candidate.subject_number
                        );
                        return Ok(());
                    }
                    winner
                }
                None => load_record(store.clone(), &base.id, run.as_str())
                    .await?
                    .ok_or_else(|| {
                        anyhow::anyhow!("admission record vanished immediately after create")
                    })?,
            }
        }
    };

    // 5. Rebuild the exact assignment this run intends from the record's own
    //    immutable pin, so a resumed attempt publishes — or adopts — byte for
    //    byte what the intent promised, whatever the profile path points at now.
    let event = recorded_assignment_event(&persisted.record, candidate, &run, &digest)?;

    // 6. Exactly one assignment comment, adopting an earlier write if present.
    if persisted.record.comment_node_id.is_none() {
        let summary = summary_for(
            &event,
            SubjectRef {
                repository: &candidate.repository,
                number: candidate.subject_number,
            },
        );
        let body = render_comment(&event, &summary)
            .map_err(|error| anyhow::anyhow!("failed to render the assignment comment: {error}"))?;
        let adopted = adopt_existing_comment(config, github, candidate, &event).await?;
        let comment_node_id = match adopted {
            Some(node_id) => {
                info!(
                    "[{reaction_name}] adopted existing assignment comment '{node_id}' for run '{run}'"
                );
                node_id
            }
            None => {
                match github
                    .create_issue_comment(&candidate.repository, candidate.subject_number, &body)
                    .await
                {
                    Ok(comment) => comment.node_id,
                    Err(error) => {
                        // The write may or may not have landed; mark the run
                        // ambiguous so the next attempt reconciles instead of
                        // blindly posting again.
                        let mut ambiguous = persisted.record.clone();
                        ambiguous.set_error(format!("{error:#}"), true);
                        persist(store.clone(), &base.id, &mut persisted, ambiguous).await?;
                        return Err(error).context("failed to post the assignment comment");
                    }
                }
            }
        };
        let mut updated = persisted.record.clone();
        updated.set_comment(comment_node_id);
        persist(store.clone(), &base.id, &mut persisted, updated).await?;
    }

    // 6. Admit the item. `AlreadyAtDestination` makes a retry after an
    //    ambiguous mutation safe.
    if !persisted.record.status_applied {
        let outcome = match github
            .update_project_status(item, &config.expected_source_status, ADMITTED_STATUS)
            .await
        {
            Ok(outcome) => outcome,
            Err(error) => {
                let mut ambiguous = persisted.record.clone();
                ambiguous.set_error(format!("{error:#}"), true);
                persist(store.clone(), &base.id, &mut persisted, ambiguous).await?;
                return Err(error).context("failed to admit the project item");
            }
        };
        if outcome == UpdateStatusOutcome::AlreadyAtDestination {
            info!(
                "[{reaction_name}] project item '{}' was already at '{ADMITTED_STATUS}'",
                candidate.project_item_node_id
            );
        }
        let mut updated = persisted.record.clone();
        updated.set_status_applied();
        persist(store.clone(), &base.id, &mut persisted, updated).await?;
    }

    info!(
        "[{reaction_name}] admitted {}#{} as run '{run}'",
        candidate.repository, candidate.subject_number
    );
    Ok(())
}

/// Verify the Project item binding and that its live status still permits work.
///
/// This is a read, so it never has an external effect; the admitted status is
/// tolerated so a resumed run can finish its remaining step.
async fn preflight_project_item(
    config: &WorkgraphAdmissionReactionConfig,
    github: &GithubClient,
    candidate: &AdmissionCandidate,
    item: ProjectItemRef<'_>,
) -> anyhow::Result<()> {
    let snapshot = github
        .project_snapshot(item)
        .await
        .context("failed to verify the project item binding")?;
    if snapshot.current_status != config.expected_source_status
        && snapshot.current_status != ADMITTED_STATUS
    {
        return Err(PermanentCandidateError::new(format!(
            "project item '{}' status is '{}' (expected '{}' or '{ADMITTED_STATUS}')",
            candidate.project_item_node_id, snapshot.current_status, config.expected_source_status
        )));
    }
    Ok(())
}

/// Build the `ResponsibilityAssigned` event for a run at an exact profile pin.
fn assignment_event(
    run: &RunId,
    candidate: &AdmissionCandidate,
    profile_ref: &ProfileRef,
    digest: &Sha256Digest,
) -> Result<WorkGraphEvent, EventError> {
    WorkGraphEvent::new(
        run.clone(),
        candidate.project_item_node_id.clone(),
        candidate.subject_node_id.clone(),
        WorkGraphEventPayload::ResponsibilityAssigned(ResponsibilityAssignedPayload {
            responsibility_type: AssignedResponsibilityType::IssueValidation,
            profile_ref: profile_ref.clone(),
            content_digest: digest.clone(),
        }),
    )
}

/// Reconstruct the exact assignment a durable record committed this run to.
///
/// The record's `profileRef` is immutable for the life of the run, so this
/// reproduces the intended event byte for byte no matter where the mutable
/// profile path points now. An unusable pin, or one that does not derive the
/// recorded `eventId`, means the record is corrupt: fail closed rather than
/// publish something the intent never promised.
fn recorded_assignment_event(
    record: &AdmissionRecord,
    candidate: &AdmissionCandidate,
    run: &RunId,
    digest: &Sha256Digest,
) -> anyhow::Result<WorkGraphEvent> {
    let profile_ref = ProfileRef::try_from(record.profile_ref.clone()).map_err(|error| {
        anyhow::anyhow!(
            "admission record for run '{}' pins an unusable profile '{}': {error}",
            record.run_id,
            record.profile_ref
        )
    })?;
    let event = assignment_event(run, candidate, &profile_ref, digest).map_err(|error| {
        anyhow::anyhow!(
            "admission record for run '{}' cannot be rebuilt into its assignment: {error}",
            record.run_id
        )
    })?;
    if event.event_id.as_str() != record.event_id {
        anyhow::bail!(
            "admission record for run '{}' records event ID '{}', but this run's assignment derives '{}'",
            record.run_id,
            record.event_id,
            event.event_id
        );
    }
    Ok(event)
}

/// Find an assignment comment this reaction already wrote for `event`.
///
/// Only comments authored by the configured trusted author (numeric database
/// ID + actor type), that GitHub reports as never edited, and whose canonical
/// event JSON — envelope *and* payload — is byte-identical to `event` can be
/// adopted. The deterministic `eventId` does **not** cover the payload, so a
/// single pre-existing comment claiming this event ID with different content
/// fails closed instead of being adopted, as do two comments that disagree.
async fn adopt_existing_comment(
    config: &WorkgraphAdmissionReactionConfig,
    github: &GithubClient,
    candidate: &AdmissionCandidate,
    event: &WorkGraphEvent,
) -> anyhow::Result<Option<String>> {
    let comments = github
        .list_issue_comments(&candidate.repository, candidate.subject_number)
        .await
        .context("failed to list issue comments for reconciliation")?;

    let trusted = config.trusted_author();
    let observed: Vec<ObservedComment> = comments
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
        .collect();

    let accepted = adopt_published_event(&observed, event)
        .map_err(|error| anyhow::anyhow!("assignment reconciliation failed: {error}"))?;
    Ok(accepted.map(|observation| observation.comment_node_id.clone()))
}

/// Compare-and-swap `next` into the store, refreshing the in-memory witness.
async fn persist(
    store: Arc<dyn StateStoreProvider>,
    store_id: &str,
    persisted: &mut PersistedAdmissionRecord,
    next: AdmissionRecord,
) -> anyhow::Result<()> {
    let Some(bytes) = compare_and_swap_record(store, store_id, &persisted.bytes, &next).await?
    else {
        anyhow::bail!(
            "admission record for run '{}' changed underneath this writer",
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

    const ITEM: &str = "PVTI_lADOABCDEF4AbcDEzgXYZ123";
    const SUBJECT: &str = "I_kwDOABCDEF6ABCDE";
    const PINNED: &str = "0123456789abcdef0123456789abcdef01234567";
    const MOVED: &str = "89abcdef0123456789abcdef0123456789abcdef";

    fn candidate() -> AdmissionCandidate {
        AdmissionCandidate {
            repository: "drasi-project/drasi-core".to_string(),
            subject_number: 742,
            subject_node_id: SUBJECT.to_string(),
            project_node_id: "PVT_project".to_string(),
            project_item_node_id: ITEM.to_string(),
            project_status: "Triage".to_string(),
        }
    }

    /// The record a run writes before publishing, pinned at `PINNED`.
    fn intent() -> (AdmissionRecord, RunId, Sha256Digest) {
        let candidate = candidate();
        let digest = body_digest(Some("Please validate this issue.\n"));
        let run = run_id(
            &candidate.project_item_node_id,
            &candidate.subject_node_id,
            &digest,
        );
        let profile_ref = ProfileRef::new("issue-validator", PINNED).expect("profile");
        let event = assignment_event(&run, &candidate, &profile_ref, &digest).expect("event");
        let record = AdmissionRecord::new(
            run.as_str(),
            event.event_id.as_str(),
            &candidate,
            digest.as_str(),
            profile_ref.as_str(),
        );
        (record, run, digest)
    }

    #[test]
    fn a_resumed_run_is_rebuilt_from_the_recorded_pin_not_the_live_one() {
        let (record, run, digest) = intent();
        let rebuilt =
            recorded_assignment_event(&record, &candidate(), &run, &digest).expect("rebuild");

        // Byte-identical to what the intent promised, even though the mutable
        // profile path now resolves to a different blob.
        let live = ProfileRef::new("issue-validator", MOVED).expect("profile");
        let drifted = assignment_event(&run, &candidate(), &live, &digest).expect("event");
        assert_ne!(
            drifted.to_canonical_json(),
            rebuilt.to_canonical_json(),
            "the fixture must actually exercise drift"
        );
        assert_eq!(rebuilt.event_id.as_str(), record.event_id);
        match &rebuilt.payload {
            WorkGraphEventPayload::ResponsibilityAssigned(payload) => {
                assert_eq!(payload.profile_ref.blob_sha(), PINNED);
                assert_eq!(payload.content_digest, digest);
            }
            other => panic!("unexpected payload: {other:?}"),
        }
    }

    #[test]
    fn a_corrupt_recorded_pin_or_event_id_fails_closed() {
        let (record, run, digest) = intent();

        let mut unusable = record.clone();
        unusable.profile_ref = "issue-validator@not-a-blob".to_string();
        assert!(
            recorded_assignment_event(&unusable, &candidate(), &run, &digest).is_err(),
            "an unusable pin must fail closed"
        );

        // A pin that parses but was not the one the recorded event ID was
        // derived under is still a corrupt record: the event ID covers the run
        // and event type only, so this check is what catches a rewritten pin.
        let mut wrong_event_id = record.clone();
        wrong_event_id.event_id = crate::state::ADMISSION_RECORD_SCHEMA.to_string();
        assert!(
            recorded_assignment_event(&wrong_event_id, &candidate(), &run, &digest).is_err(),
            "an event ID that this run does not derive must fail closed"
        );

        recorded_assignment_event(&record, &candidate(), &run, &digest)
            .expect("the intact record still rebuilds");
    }
}
