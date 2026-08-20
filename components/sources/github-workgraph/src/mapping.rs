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

use crate::config::{LeaseTrust, RepositoryFilter, TaskIssueType};
use crate::lease_ledger::{
    Acquisition, AnchorKey, AnchorState, EndClaim, EndKind, LeaseLedger, LifecycleIntent,
};
use crate::workers::{WorkerFile, WorkerFileContent, WorkerFileLocation, MAX_WORKER_SLOTS};
use crate::workgraph::{
    classify_comment, classify_task_body, comment_error_element_id, lease_anchor_element_id,
    slot_id, status_error_element_id, task_error_element_id, worker_config_error_element_id,
    worker_element_id, worker_slot_element_id, CommentClassification, TaskClassification,
    TaskLease, WorkGraphError,
};
use chrono::{DateTime, SecondsFormat};
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::Arc;
use Op::{Insert, Update};

macro_rules! labels {
    ($array:ident, $($name:ident = $value:literal),+ $(,)?) => {
        $(pub const $name: &str = $value;)+
        pub const $array: [&str; [$($value),+].len()] = [$($value),+];
    };
}

labels!(
    NODE_LABELS,
    NODE_ORGANIZATION = "GitHubOrganization",
    NODE_REPOSITORY = "GitHubRepository",
    NODE_ISSUE = "GitHubIssue",
    NODE_PULL_REQUEST = "GitHubPullRequest",
    NODE_ISSUE_COMMENT = "GitHubIssueComment",
    NODE_PULL_REQUEST_COMMENT = "GitHubPullRequestComment",
    NODE_PULL_REQUEST_REVIEW = "GitHubPullRequestReview",
    NODE_WORKGRAPH_TASK = "WorkGraphTask",
    NODE_WORKGRAPH_TASK_ASSIGNMENT = "WorkGraphTaskAssignment",
    NODE_WORKGRAPH_TASK_RESULT = "WorkGraphTaskResult",
    NODE_WORKGRAPH_TASK_RESULT_ACCEPTANCE = "WorkGraphTaskResultAcceptance",
    NODE_WORKGRAPH_TASK_LEASE = "WorkGraphTaskLease",
    NODE_WORKGRAPH_TASK_LEASE_ANCHOR = "WorkGraphTaskLeaseAnchor",
    NODE_WORKGRAPH_TASK_LEASE_EXPIRATION = "WorkGraphTaskLeaseExpiration",
    NODE_WORKGRAPH_WORKER = "WorkGraphWorker",
    NODE_WORKGRAPH_WORKER_SLOT = "WorkGraphWorkerSlot",
    NODE_WORKGRAPH_ERROR = "WorkGraphError",
);

labels!(
    RELATION_LABELS,
    REL_IN_ORGANIZATION = "IN_ORGANIZATION",
    REL_IN_REPOSITORY = "IN_REPOSITORY",
    REL_COMMENT_ON = "COMMENT_ON",
    REL_REVIEW_OF = "REVIEW_OF",
    REL_ASSIGNMENT_FOR = "ASSIGNMENT_FOR",
    REL_ASSIGNED_TO = "ASSIGNED_TO",
    REL_RESULT_FOR = "RESULT_FOR",
    REL_RESULT_FOR_LEASE = "RESULT_FOR_LEASE",
    REL_ACCEPTS_RESULT = "ACCEPTS_RESULT",
    REL_TASK_FOR = "TASK_FOR",
    REL_HAS_SLOT = "HAS_SLOT",
    REL_LEASE_FOR = "LEASE_FOR",
    REL_LEASES_SLOT = "LEASES_SLOT",
    REL_LEASE_ANCHOR = "LEASE_ANCHOR",
    REL_EXPIRES_LEASE = "EXPIRES_LEASE",
    REL_ERROR_ON = "ERROR_ON",
);

pub const STATUS_PREFIX: &str = "status:";

const SUPPORTED_EVENTS: &str =
    "repository issues issue_comment pull_request pull_request_review sub_issues";

const REPOSITORY_ACTIONS: &str = "created edited renamed archived unarchived privatized \
     publicized deleted transferred";
const ISSUE_ACTIONS: &str = "opened deleted transferred assigned closed demilestoned edited \
     field_added field_removed labeled locked milestoned reopened typed unassigned unlabeled \
     unlocked untyped";
const ISSUE_COMMENT_ACTIONS: &str = "created edited deleted pinned unpinned";
const PULL_REQUEST_ACTIONS: &str = "opened assigned auto_merge_disabled auto_merge_enabled \
     closed converted_to_draft demilestoned dequeued edited enqueued labeled locked milestoned \
     ready_for_review reopened review_request_removed review_requested stacked synchronize \
     unassigned unlabeled unlocked";
const REVIEW_ACTIONS: &str = "submitted edited dismissed";
const SUB_ISSUE_ACTIONS: &str =
    "parent_issue_added parent_issue_removed sub_issue_added sub_issue_removed";

const ORGANIZATION_PROPS: &str = "nodeId=node_id databaseId=id login=login url=url \
     avatarUrl=avatar_url description=description";
const REPOSITORY_PROPS: &str = "nodeId=node_id databaseId=id name=name nameWithOwner=full_name \
     ownerLogin=owner/login description=description url=html_url isPrivate=private \
     isArchived=archived isFork=fork visibility=visibility defaultBranch=default_branch \
     topics=topics";
const WORK_ITEM_PROPS: &str = "nodeId=node_id databaseId=id number=number title=title body=body \
     state=state isLocked=locked createdAt=created_at updatedAt=updated_at closedAt=closed_at \
     url=html_url";
const ISSUE_ONLY_PROPS: &str = "stateReason=state_reason";
const PULL_REQUEST_ONLY_PROPS: &str = "isDraft=draft isMerged=merged mergedAt=merged_at \
     headRefName=head/ref headSha=head/sha baseRefName=base/ref baseSha=base/sha";
const COMMENT_PROPS: &str = "nodeId=node_id databaseId=id body=body createdAt=created_at \
     updatedAt=updated_at url=html_url";
const PROVENANCE_PROPS: &str = "sourceCommentNodeId=node_id sourceCommentDatabaseId=id \
     createdAt=created_at updatedAt=updated_at url=html_url";
const REVIEW_PROPS: &str = "nodeId=node_id databaseId=id state=state body=body \
     submittedAt=submitted_at commitId=commit_id url=html_url";
const AUTHOR_PROPS: &str = "authorLogin=user/login authorId=user/node_id \
     authorDatabaseId=user/id authorType=user/type authorAssociation=author_association";

fn in_table(table: &str, value: &str) -> bool {
    table.split_whitespace().any(|entry| entry == value)
}

fn is_known_action(event_type: &str, action: &str) -> bool {
    let table = match event_type {
        "repository" => REPOSITORY_ACTIONS,
        "issues" => ISSUE_ACTIONS,
        "issue_comment" => ISSUE_COMMENT_ACTIONS,
        "pull_request" => PULL_REQUEST_ACTIONS,
        "pull_request_review" => REVIEW_ACTIONS,
        _ => SUB_ISSUE_ACTIONS,
    };
    in_table(table, action)
}

/// One delivery's graph changes plus the lease-lifecycle contributions it
/// states. Anchor state is never written here; the caller folds the
/// contributions into its [`LeaseLedger`] and projects the affected anchors.
pub struct Conversion {
    pub changes: Vec<SourceChange>,
    pub lifecycle: Vec<LifecycleIntent>,
    /// Set when the delivery concerns a lease lifecycle artifact, whatever its
    /// trust. The caller reconciles this task's current comments from GitHub
    /// before applying the delivery if its ledger has never seen the task.
    pub lifecycle_scope: Option<LifecycleScope>,
    /// Anchor element IDs this delivery names, by its current body or the one
    /// it is moving away from. The caller re-projects each from its ledger.
    pub lifecycle_anchors: Vec<String>,
}

/// Everything needed to re-read one task's current comments from GitHub.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LifecycleScope {
    pub task_node_id: String,
    pub repository_owner: String,
    pub repository_name: String,
    pub issue_number: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ConvertError {
    OrganizationMismatch(String),
    InvalidPayload(String),
}

fn invalid(message: impl Into<String>) -> ConvertError {
    ConvertError::InvalidPayload(message.into())
}

struct Delivery<'a> {
    action: &'a str,
    payload: &'a Value,
    org: &'a str,
}

type Mapped = Result<(), ConvertError>;

pub struct Converter<'a> {
    source_id: &'a str,
    organization: &'a str,
    task_issue_type: &'a TaskIssueType,
    effective_from: u64,
    repository_filter: Option<&'a RepositoryFilter>,
    lease_trust: Option<&'a LeaseTrust>,
}

impl<'a> Converter<'a> {
    pub fn new(
        source_id: &'a str,
        organization: &'a str,
        task_issue_type: &'a TaskIssueType,
        effective_from: u64,
    ) -> Self {
        Self {
            source_id,
            organization,
            task_issue_type,
            effective_from,
            repository_filter: None,
            lease_trust: None,
        }
    }

    /// Configure which identities may author lease lifecycle artifacts.
    ///
    /// Without it nothing is trusted: Lease, Result/v2, and Lease Expiration
    /// comments are still projected with their provenance, but none of them
    /// binds or moves the lease lifecycle.
    pub fn with_lease_trust(mut self, lease_trust: &'a LeaseTrust) -> Self {
        self.lease_trust = Some(lease_trust);
        self
    }

    pub fn with_repository_filter(mut self, repository_filter: &'a RepositoryFilter) -> Self {
        self.repository_filter = Some(repository_filter);
        self
    }

    pub fn convert(
        &self,
        event_type: &str,
        payload: &Value,
    ) -> Result<Option<Conversion>, ConvertError> {
        if !in_table(SUPPORTED_EVENTS, event_type) {
            return Ok(None);
        }
        let org_node_id = self.validate_organization(payload)?;
        let action = payload
            .get("action")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("missing 'action'"))?;
        if !is_known_action(event_type, action) {
            return Ok(None);
        }
        if !self.delivery_in_scope(event_type, action, payload)? {
            return Ok(None);
        }
        if !self.delivery_projects_open(event_type, action, payload)? {
            return Ok(None);
        }
        let d = &Delivery {
            action,
            payload,
            org: &org_node_id,
        };
        let mut cs = Changes::new(self.source_id, self.effective_from);
        match event_type {
            "repository" => self.repository_event(&mut cs, d)?,
            "issues" => self.issue_event(&mut cs, d)?,
            "pull_request" => self.work_item_event(&mut cs, d, NODE_PULL_REQUEST)?,
            "issue_comment" => self.comment_event(&mut cs, d)?,
            "pull_request_review" => self.review_event(&mut cs, d)?,
            _ => self.sub_issue_event(&mut cs, d)?,
        }
        Ok(Some(Conversion {
            changes: cs.changes,
            lifecycle: cs.lifecycle,
            lifecycle_scope: cs.lifecycle_scope,
            lifecycle_anchors: cs.lifecycle_anchors,
        }))
    }

    fn delivery_in_scope(
        &self,
        event_type: &str,
        action: &str,
        payload: &Value,
    ) -> Result<bool, ConvertError> {
        let Some(filter) = self.repository_filter else {
            return Ok(true);
        };
        if event_type == "issues" {
            let (current_task, previous_task) = self.issue_task_states(action, payload)?;
            if action == "deleted" || (!current_task && (action == "closed" || previous_task)) {
                return Ok(true);
            }
        }
        if (event_type == "pull_request" && action == "closed")
            || (event_type == "sub_issues"
                && in_table("parent_issue_removed sub_issue_removed", action))
        {
            return Ok(true);
        }
        if event_type == "sub_issues" {
            if in_table("parent_issue_added parent_issue_removed", action) {
                return self.repository_in_scope(payload.get("repository"));
            }
            if let Some(repository) = payload.get("sub_issue_repo") {
                return self.repository_in_scope(Some(repository));
            }
            if let Some(name) = payload.get("sub_issue").and_then(issue_repository_name) {
                return Ok(filter.includes_name(name));
            }
            return self.repository_in_scope(payload.get("repository"));
        }
        let current = self.repository_in_scope(payload.get("repository"))?;
        if event_type == "issues" && action == "transferred" {
            if current {
                return Ok(true);
            }
            return match payload.pointer("/changes/new_repository") {
                Some(repository) => self.repository_in_scope(Some(repository)),
                None => Ok(false),
            };
        }
        if event_type == "repository" && action == "renamed" {
            let previous = payload
                .pointer("/changes/repository/name/from")
                .and_then(Value::as_str)
                .is_some_and(|name| filter.includes_name(name));
            return Ok(current || previous);
        }
        Ok(current)
    }

    fn delivery_projects_open(
        &self,
        event_type: &str,
        action: &str,
        payload: &Value,
    ) -> Result<bool, ConvertError> {
        match event_type {
            "repository" | "sub_issues" => Ok(true),
            "issues" if in_table("closed deleted transferred", action) => Ok(true),
            "pull_request" if action == "closed" => Ok(true),
            "issues" => {
                let issue = payload
                    .get("issue")
                    .ok_or_else(|| invalid("missing 'issue'"))?;
                let (current_task, previous_task) = self.issue_task_states(action, payload)?;
                if current_task || previous_task {
                    Ok(true)
                } else {
                    work_item_action_projects_open(action, issue, "issue")
                }
            }
            "pull_request" => work_item_action_projects_open(
                action,
                payload
                    .get("pull_request")
                    .ok_or_else(|| invalid("missing 'pull_request'"))?,
                "pull_request",
            ),
            "issue_comment" => {
                let issue = payload
                    .get("issue")
                    .ok_or_else(|| invalid("missing 'issue'"))?;
                if self.is_task_issue(issue) {
                    Ok(true)
                } else {
                    item_is_open(issue, "issue")
                }
            }
            _ => item_is_open(
                payload
                    .get("pull_request")
                    .ok_or_else(|| invalid("missing 'pull_request'"))?,
                "pull_request",
            ),
        }
    }

    fn repository_in_scope(&self, repository: Option<&Value>) -> Result<bool, ConvertError> {
        let Some(filter) = self.repository_filter else {
            return Ok(true);
        };
        let repository = repository.ok_or_else(|| invalid("missing 'repository'"))?;
        filter
            .includes_repository(repository)
            .map_err(|error| invalid(error.to_string()))
    }

    fn is_task_issue(&self, issue: &Value) -> bool {
        self.task_issue_type.matches(issue.get("type"))
    }

    fn issue_task_states(
        &self,
        action: &str,
        payload: &Value,
    ) -> Result<(bool, bool), ConvertError> {
        let issue = payload
            .get("issue")
            .ok_or_else(|| invalid("missing 'issue'"))?;
        let issue_task = self.is_task_issue(issue);
        match action {
            "typed" => {
                let assigned = payload
                    .get("type")
                    .ok_or_else(|| invalid("missing 'type' for typed issue delivery"))?;
                required_str(assigned, "/node_id")?;
                required_str(assigned, "/name")?;
                let assigned_task = self.task_issue_type.matches(Some(assigned));
                let issue_type_absent = issue.get("type").is_none_or(Value::is_null);
                Ok((issue_task || (issue_type_absent && assigned_task), false))
            }
            "untyped" => {
                let removed = payload
                    .get("type")
                    .ok_or_else(|| invalid("missing 'type' for untyped issue delivery"))?;
                required_str(removed, "/node_id")?;
                required_str(removed, "/name")?;
                let removed_task = self.task_issue_type.matches(Some(removed));
                Ok((issue_task && !removed_task, removed_task))
            }
            _ => Ok((issue_task, issue_task)),
        }
    }

    fn validate_organization(&self, payload: &Value) -> Result<String, ConvertError> {
        let login = payload
            .pointer("/organization/login")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                invalid("payload has no 'organization.login'; configure an organization webhook")
            })?;
        if !login.eq_ignore_ascii_case(self.organization) {
            return Err(ConvertError::OrganizationMismatch(format!(
                "delivery organization '{login}' does not match configured organization '{}'",
                self.organization
            )));
        }
        required_str(payload, "/organization/node_id")
    }
    fn repository_event(&self, cs: &mut Changes, d: &Delivery) -> Mapped {
        let (action, payload, org) = (d.action, d.payload, d.org);
        let repo = payload
            .get("repository")
            .ok_or_else(|| invalid("missing 'repository'"))?;
        let repo_id = required_str(payload, "/repository/node_id")?;
        let in_org = owner_in_org(Some(repo), self.organization);
        let relation_id = rel_id(REL_IN_ORGANIZATION, &repo_id, org);
        if action == "renamed" && self.repository_filter.is_some() {
            let current = self.repository_in_scope(Some(repo))?;
            let previous = payload
                .pointer("/changes/repository/name/from")
                .and_then(Value::as_str)
                .is_some_and(|name| {
                    self.repository_filter
                        .is_some_and(|filter| filter.includes_name(name))
                });
            match (previous, current) {
                (true, false) => {
                    cs.delete(&relation_id, REL_IN_ORGANIZATION);
                    cs.delete(&repo_id, NODE_REPOSITORY);
                    return Ok(());
                }
                (false, true) => {
                    cs.node(Update, org, NODE_ORGANIZATION, org_props(payload));
                    cs.node(Insert, &repo_id, NODE_REPOSITORY, repository_props(repo));
                    cs.relation(Insert, REL_IN_ORGANIZATION, &relation_id, &repo_id, org);
                    return Ok(());
                }
                (false, false) => return Ok(()),
                (true, true) => {}
            }
        }
        match action {
            "created" | "transferred" if action == "created" || in_org => {
                let op = if action == "created" { Insert } else { Update };
                cs.node(Update, org, NODE_ORGANIZATION, org_props(payload));
                cs.node(op, &repo_id, NODE_REPOSITORY, repository_props(repo));
                cs.relation(Insert, REL_IN_ORGANIZATION, &relation_id, &repo_id, org);
            }
            "deleted" | "transferred" => {
                cs.delete(&relation_id, REL_IN_ORGANIZATION);
                cs.delete(&repo_id, NODE_REPOSITORY);
            }
            _ => cs.node(Update, &repo_id, NODE_REPOSITORY, repository_props(repo)),
        }
        Ok(())
    }

    fn issue_event(&self, cs: &mut Changes, d: &Delivery) -> Mapped {
        let (current_task, previous_task) = self.issue_task_states(d.action, d.payload)?;

        if current_task || previous_task {
            return self.task_issue_event(cs, d, current_task, previous_task);
        }
        self.work_item_event(cs, d, NODE_ISSUE)
    }

    fn task_issue_event(
        &self,
        cs: &mut Changes,
        d: &Delivery,
        current_task: bool,
        previous_task: bool,
    ) -> Mapped {
        let issue = d
            .payload
            .get("issue")
            .ok_or_else(|| invalid("missing 'issue'"))?;
        let issue_id = required_str(d.payload, "/issue/node_id")?;
        let task_database_id = required_database_id(issue, "/id")?;
        let repo = d.payload.get("repository");
        let repo_id = required_str(d.payload, "/repository/node_id")?;

        if d.action == "transferred" {
            delete_task(cs, &issue_id, &task_database_id, &repo_id);
            if let (Some(new_issue), Some(new_repo)) = (
                d.payload.pointer("/changes/new_issue"),
                d.payload.pointer("/changes/new_repository"),
            ) {
                if owner_in_org(Some(new_repo), self.organization)
                    && self.repository_in_scope(Some(new_repo))?
                    && self.is_task_issue(new_issue)
                {
                    let new_id = required_str(new_issue, "/node_id")?;
                    let new_repo_id = required_str(new_repo, "/node_id")?;
                    upsert_task(cs, new_issue, Some(new_repo), &new_id, Some(&new_repo_id))?;
                }
            }
            return Ok(());
        }

        if d.action == "deleted" {
            delete_task(cs, &issue_id, &task_database_id, &repo_id);
            return Ok(());
        }

        if !current_task {
            if item_is_open(issue, "issue")? && self.repository_in_scope(repo)? {
                clean_task_transition_artifacts(cs, &issue_id, &task_database_id);
                update_issue_from_task(cs, issue, repo, &issue_id, &repo_id)?;
            } else {
                delete_task(cs, &issue_id, &task_database_id, &repo_id);
            }
            return Ok(());
        }

        if !previous_task {
            clean_generic_transition_artifacts(cs, &issue_id);
        }
        let body = issue.get("body").and_then(Value::as_str).unwrap_or("");
        match classify_task_body(body) {
            TaskClassification::Task(_) => {
                let error_id = task_error_element_id(&issue_id);
                cs.delete(&error_id, NODE_WORKGRAPH_ERROR);
                upsert_task(cs, issue, repo, &issue_id, Some(&repo_id))?;
            }
            TaskClassification::Invalid(error) => {
                delete_task_representation(cs, &issue_id, &repo_id);
                let mut props = task_error_props(issue, repo, body);
                props.text("errorKind", "invalid-workgraph-task");
                props.text("errorCode", error.code);
                props.text("errorMessage", &error.message);
                cs.node(
                    Update,
                    &task_error_element_id(&issue_id),
                    NODE_WORKGRAPH_ERROR,
                    props,
                );
            }
        }
        Ok(())
    }

    fn sub_issue_event(&self, cs: &mut Changes, d: &Delivery) -> Mapped {
        let parent_action = in_table("parent_issue_added parent_issue_removed", d.action);
        let child = if parent_action {
            Some(
                d.payload
                    .get("sub_issue")
                    .ok_or_else(|| invalid("missing 'sub_issue' for parent_issue action"))?,
            )
        } else {
            d.payload.get("sub_issue")
        };
        let parent = if parent_action {
            d.payload.get("parent_issue")
        } else {
            Some(
                d.payload
                    .get("parent_issue")
                    .ok_or_else(|| invalid("missing 'parent_issue' for sub_issue action"))?,
            )
        };

        if in_table("parent_issue_removed sub_issue_removed", d.action) {
            let child_database_id = match child {
                Some(child) => required_database_id(child, "/id")
                    .or_else(|_| required_database_id(d.payload, "/sub_issue_id"))?,
                None => match d.payload.get("sub_issue_id") {
                    Some(_) => required_database_id(d.payload, "/sub_issue_id")?,
                    None => return Ok(()),
                },
            };
            cs.delete(&task_for_rel_id(&child_database_id), REL_TASK_FOR);
            return Ok(());
        }

        let Some(child) = child else {
            return Ok(());
        };
        let child_id = required_str(child, "/node_id")?;
        let child_database_id = required_database_id(child, "/id")?;
        if !self.is_task_issue(child) {
            return Ok(());
        }

        let parent_id = parent
            .map(|parent| required_str(parent, "/node_id"))
            .transpose()?;

        let top_repository = d.payload.get("repository");
        let child_repo = if parent_action {
            top_repository
        } else {
            d.payload.get("sub_issue_repo").or_else(|| {
                top_repository.filter(|repo| repository_is_authoritative_for(child, repo))
            })
        };
        let child_repo_id = child_repo
            .map(|repo| required_str(repo, "/node_id"))
            .transpose()?;
        let valid_task =
            match classify_task_body(child.get("body").and_then(Value::as_str).unwrap_or("")) {
                TaskClassification::Task(_) => {
                    cs.delete(&task_error_element_id(&child_id), NODE_WORKGRAPH_ERROR);
                    upsert_task(cs, child, child_repo, &child_id, child_repo_id.as_deref())?;
                    true
                }
                TaskClassification::Invalid(error) => {
                    match child_repo_id.as_deref() {
                        Some(repo_id) => delete_task_representation(cs, &child_id, repo_id),
                        None => cs.delete(&child_id, NODE_WORKGRAPH_TASK),
                    }
                    let body = child.get("body").and_then(Value::as_str).unwrap_or("");
                    let mut props = task_error_props(child, child_repo, body);
                    props.text("errorKind", "invalid-workgraph-task");
                    props.text("errorCode", error.code);
                    props.text("errorMessage", &error.message);
                    cs.node(
                        Update,
                        &task_error_element_id(&child_id),
                        NODE_WORKGRAPH_ERROR,
                        props,
                    );
                    false
                }
            };
        if let Some(parent_id) = &parent_id {
            cs.relation(
                Update,
                REL_TASK_FOR,
                &task_for_rel_id(&child_database_id),
                &child_id,
                parent_id,
            );
        }
        if !valid_task {
            return Ok(());
        }

        let Some(parent) = parent else {
            return Ok(());
        };
        let parent_id = parent_id.expect("parent ID accompanies parent");

        let parent_repo = if parent_action {
            d.payload.get("parent_issue_repo").or_else(|| {
                top_repository.filter(|repo| repository_is_authoritative_for(parent, repo))
            })
        } else {
            top_repository
        };
        if let Some(parent_repo) = parent_repo {
            if !self.is_task_issue(parent)
                && item_is_open(parent, "parent_issue")?
                && self.repository_in_scope(Some(parent_repo))?
            {
                let parent_repo_id = required_str(parent_repo, "/node_id")?;
                cs.node(
                    Update,
                    &parent_repo_id,
                    NODE_REPOSITORY,
                    repository_props(parent_repo),
                );
                update_work_item(
                    cs,
                    parent,
                    Some(parent_repo),
                    &parent_id,
                    &parent_repo_id,
                    NODE_ISSUE,
                )?;
            }
        }
        Ok(())
    }

    fn work_item_event(&self, cs: &mut Changes, d: &Delivery, label: &'static str) -> Mapped {
        let (action, payload) = (d.action, d.payload);
        let key = if label == NODE_ISSUE {
            "issue"
        } else {
            "pull_request"
        };
        let item = payload
            .get(key)
            .ok_or_else(|| invalid(format!("missing '{key}'")))?;
        let item_id = required_str(payload, &format!("/{key}/node_id"))?;
        let repo = payload.get("repository");
        let repo_id = required_str(payload, "/repository/node_id")?;
        match action {
            "closed" | "deleted" => delete_work_item(cs, &item_id, &repo_id, label),
            "opened" | "reopened" => update_work_item(cs, item, repo, &item_id, &repo_id, label)?,
            "transferred" => {
                if owner_in_org(repo, self.organization) && self.repository_in_scope(repo)? {
                    delete_work_item(cs, &item_id, &repo_id, label);
                }
                let (Some(new_item), Some(new_repo)) = (
                    payload.pointer("/changes/new_issue"),
                    payload.pointer("/changes/new_repository"),
                ) else {
                    return Ok(());
                };
                if owner_in_org(Some(new_repo), self.organization)
                    && self.repository_in_scope(Some(new_repo))?
                    && item_is_open(new_item, "changes.new_issue")?
                {
                    let new_id = required_str(new_item, "/node_id")?;
                    let new_repo_id = required_str(new_repo, "/node_id")?;
                    update_work_item(cs, new_item, Some(new_repo), &new_id, &new_repo_id, label)?;
                }
            }
            _ => {
                cs.node(Update, &item_id, label, work_item_props(item, repo, label)?);
                status_changes(cs, Update, item, repo, &item_id, label);
            }
        }
        Ok(())
    }
    fn comment_event(&self, cs: &mut Changes, d: &Delivery) -> Mapped {
        let (action, payload) = (d.action, d.payload);
        let comment = payload
            .get("comment")
            .ok_or_else(|| invalid("missing 'comment'"))?;
        let comment_id = required_str(payload, "/comment/node_id")?;
        let issue = payload
            .get("issue")
            .ok_or_else(|| invalid("missing 'issue'"))?;
        let parent = required_str(payload, "/issue/node_id")?;
        let on_pr = issue.get("pull_request").is_some();
        let plain = if on_pr {
            NODE_PULL_REQUEST_COMMENT
        } else {
            NODE_ISSUE_COMMENT
        };
        let on_task = self.is_task_issue(issue);
        let repo = payload.get("repository");
        // On an edit GitHub reports the acting identity as the delivery
        // `sender`, which is the only place the editor appears in a webhook.
        let editor_identity = (action == "edited")
            .then(|| payload.get("sender"))
            .flatten()
            .filter(|sender| !sender.is_null());
        let classify_body = |body: &str| {
            CommentNode::classify(
                body,
                &comment_id,
                &parent,
                plain,
                on_task,
                comment,
                repo,
                self.lease_trust,
                editor_identity,
            )
        };
        let current = classify_body(comment.get("body").and_then(Value::as_str).unwrap_or(""));
        // A delete removes the comment entirely, so it contributes nothing
        // regardless of what its body said; every other action contributes
        // whatever the body currently says, unless that is indeterminate.
        let contribution = if action == "deleted" {
            Some(LifecycleIntent::Retract {
                comment_node_id: comment_id.clone(),
            })
        } else {
            current.lifecycle.clone()
        };
        if let Some(contribution) = contribution {
            cs.lifecycle.push(contribution);
        }
        // Reconciliation scope must cover the body the comment is moving *away*
        // from as well as the one it is moving to. Editing a bootstrapped
        // Lease, Result, or Expiration into ordinary, v1, or invalid content
        // emits only a Retract, and a ledger that has never seen the task would
        // apply that Retract to nothing and leave the historical anchor stale.
        // Any edit or delete of a comment on a configured task is treated as
        // lifecycle-relevant for the same reason: reconciliation happens at
        // most once per task, so erring wide costs one read and erring narrow
        // loses state.
        let previous_anchor = payload
            .pointer("/changes/body/from")
            .and_then(Value::as_str)
            .map(classify_body)
            .and_then(|previous| previous.lifecycle_anchor);
        // Every anchor this delivery names — by its current body or the body it
        // is moving away from — is re-projected from the ledger afterwards.
        // That is what lets a deleted or edited-away artifact correct an anchor
        // the ledger never recorded, such as one projected by a bootstrap.
        cs.lifecycle_anchors
            .extend(current.lifecycle_anchor.iter().cloned());
        cs.lifecycle_anchors.extend(previous_anchor.iter().cloned());
        if current.lifecycle_anchor.is_some()
            || previous_anchor.is_some()
            || (on_task && in_table("edited deleted", action))
        {
            cs.lifecycle_scope = lifecycle_scope(payload, &parent);
        }
        match action {
            "created" => current.insert(cs, &comment_id, &parent),
            "deleted" => current.delete(cs, &comment_id, &parent),
            "pinned" | "unpinned" => current.update(cs),
            _ => {
                match payload
                    .pointer("/changes/body/from")
                    .and_then(Value::as_str)
                    .map(classify_body)
                {
                    None => current.update(cs),
                    Some(prev)
                        if prev.element_id != current.element_id || prev.label != current.label =>
                    {
                        prev.delete(cs, &comment_id, &parent);
                        current.insert(cs, &comment_id, &parent);
                    }
                    Some(prev)
                        if prev.specialized_relations != current.specialized_relations
                            || prev.lifecycle != current.lifecycle =>
                    {
                        current.update(cs);
                        for (label, target) in &prev.specialized_relations {
                            if !current
                                .specialized_relations
                                .iter()
                                .any(|kept| kept.0 == *label && kept.1 == *target)
                            {
                                cs.delete(&rel_id(label, &comment_id, target), label);
                            }
                        }
                        // An edited Lease that now names a different `leaseId`
                        // simply stops pointing at its previous anchor; the
                        // ledger recomputes both the anchor it left and the one
                        // it joined.
                        for (label, target) in &current.specialized_relations {
                            let id = rel_id(label, &comment_id, target);
                            cs.relation(Insert, label, &id, &comment_id, target);
                        }
                    }
                    Some(_) => {
                        current.update(cs);
                        if current.is_error {
                            let id = rel_id(REL_ERROR_ON, &comment_id, &parent);
                            cs.relation(Update, REL_ERROR_ON, &id, &current.element_id, &parent);
                        }
                    }
                }
            }
        }
        Ok(())
    }
    fn review_event(&self, cs: &mut Changes, d: &Delivery) -> Mapped {
        let (action, payload) = (d.action, d.payload);
        let review = payload
            .get("review")
            .ok_or_else(|| invalid("missing 'review'"))?;
        let review_id = required_str(payload, "/review/node_id")?;
        let pr_id = required_str(payload, "/pull_request/node_id")?;
        let mut props = ElementPropertyMap::new();
        props.table(review, REVIEW_PROPS);
        props.table(review, AUTHOR_PROPS);
        props.copy(
            "repositoryNameWithOwner",
            full_name(payload.get("repository")),
        );
        if action == "dismissed" {
            props.text("state", "dismissed");
        }
        let op = if action == "submitted" {
            Insert
        } else {
            Update
        };
        cs.node(op, &review_id, NODE_PULL_REQUEST_REVIEW, props);
        if op == Insert {
            let id = rel_id(REL_REVIEW_OF, &review_id, &pr_id);
            cs.relation(Insert, REL_REVIEW_OF, &id, &review_id, &pr_id);
        }
        Ok(())
    }
}

/// True when GitHub reports that this comment body has been edited since it
/// was created.
fn shows_edit(comment: &Value) -> bool {
    if comment
        .get("last_edited_at")
        .is_some_and(|value| !value.is_null())
    {
        return true;
    }
    match (comment.get("created_at"), comment.get("updated_at")) {
        (Some(created), Some(updated)) => created != updated,
        _ => false,
    }
}

/// The task coordinates a reconciliation needs, when the delivery carries them.
fn lifecycle_scope(payload: &Value, task_node_id: &str) -> Option<LifecycleScope> {
    Some(LifecycleScope {
        task_node_id: task_node_id.to_string(),
        repository_owner: payload
            .pointer("/repository/owner/login")
            .and_then(Value::as_str)?
            .to_string(),
        repository_name: payload
            .pointer("/repository/name")
            .and_then(Value::as_str)?
            .to_string(),
        issue_number: payload.pointer("/issue/number").and_then(Value::as_u64)?,
    })
}

fn update_work_item(
    cs: &mut Changes,
    item: &Value,
    repo: Option<&Value>,
    item_id: &str,
    repo_id: &str,
    label: &'static str,
) -> Mapped {
    cs.node(Update, item_id, label, work_item_props(item, repo, label)?);
    let id = rel_id(REL_IN_REPOSITORY, item_id, repo_id);
    cs.relation(Update, REL_IN_REPOSITORY, &id, item_id, repo_id);
    status_changes(cs, Update, item, repo, item_id, label);
    Ok(())
}

fn update_issue_from_task(
    cs: &mut Changes,
    issue: &Value,
    repo: Option<&Value>,
    issue_id: &str,
    repo_id: &str,
) -> Mapped {
    let mut props = work_item_props(issue, repo, NODE_ISSUE)?;
    for key in ["taskType", "inputs", "issueTypeId", "issueTypeName"] {
        props.insert(key, ElementValue::Null);
    }
    cs.node(Update, issue_id, NODE_ISSUE, props);
    let id = rel_id(REL_IN_REPOSITORY, issue_id, repo_id);
    cs.relation(Update, REL_IN_REPOSITORY, &id, issue_id, repo_id);
    status_changes(cs, Update, issue, repo, issue_id, NODE_ISSUE);
    Ok(())
}

fn upsert_task(
    cs: &mut Changes,
    issue: &Value,
    repo: Option<&Value>,
    issue_id: &str,
    repo_id: Option<&str>,
) -> Mapped {
    let TaskClassification::Task(task) =
        classify_task_body(issue.get("body").and_then(Value::as_str).unwrap_or(""))
    else {
        return Err(invalid(
            "task body changed classification during conversion",
        ));
    };
    let mut props = work_item_props(issue, repo, NODE_WORKGRAPH_TASK)?;
    props.text("taskType", task.task_type.as_str());
    props.insert(
        "inputs",
        ElementValue::from(&serde_json::json!(task.inputs)),
    );
    if let Some(issue_type) = issue.get("type") {
        props.copy("issueTypeId", issue_type.get("node_id"));
        props.copy("issueTypeName", issue_type.get("name"));
    }
    cs.node(Update, issue_id, NODE_WORKGRAPH_TASK, props);
    if let Some(repo_id) = repo_id {
        cs.relation(
            Update,
            REL_IN_REPOSITORY,
            &rel_id(REL_IN_REPOSITORY, issue_id, repo_id),
            issue_id,
            repo_id,
        );
    }
    Ok(())
}

fn task_error_props(issue: &Value, repo: Option<&Value>, body: &str) -> ElementPropertyMap {
    let mut props = ElementPropertyMap::new();
    props.table(issue, WORK_ITEM_PROPS);
    normalize_issue_state(&mut props, issue);
    props.table(issue, AUTHOR_PROPS);
    props.copy("repositoryNameWithOwner", full_name(repo));
    props.text("sourceTaskBody", body);
    props.copy("sourceTaskNodeId", issue.get("node_id"));
    props.copy("sourceTaskDatabaseId", issue.get("id"));
    props
}

fn task_for_rel_id(task_database_id: &str) -> String {
    format!("{REL_TASK_FOR}:{task_database_id}")
}

fn clean_generic_transition_artifacts(cs: &mut Changes, issue_id: &str) {
    let error_id = status_error_element_id(issue_id);
    cs.delete(&rel_id(REL_ERROR_ON, &error_id, issue_id), REL_ERROR_ON);
    cs.delete(&error_id, NODE_WORKGRAPH_ERROR);
}

fn clean_task_transition_artifacts(cs: &mut Changes, issue_id: &str, task_database_id: &str) {
    cs.delete(&task_for_rel_id(task_database_id), REL_TASK_FOR);
    cs.delete(&task_error_element_id(issue_id), NODE_WORKGRAPH_ERROR);
}

fn delete_task_representation(cs: &mut Changes, issue_id: &str, repo_id: &str) {
    cs.delete(
        &rel_id(REL_IN_REPOSITORY, issue_id, repo_id),
        REL_IN_REPOSITORY,
    );
    cs.delete(issue_id, NODE_WORKGRAPH_TASK);
}

fn delete_task(cs: &mut Changes, issue_id: &str, task_database_id: &str, repo_id: &str) {
    clean_task_transition_artifacts(cs, issue_id, task_database_id);
    delete_task_representation(cs, issue_id, repo_id);
}

fn delete_work_item(cs: &mut Changes, item_id: &str, repo_id: &str, label: &str) {
    let error_id = status_error_element_id(item_id);
    cs.delete(&rel_id(REL_ERROR_ON, &error_id, item_id), REL_ERROR_ON);
    cs.delete(&error_id, NODE_WORKGRAPH_ERROR);
    cs.delete(
        &rel_id(REL_IN_REPOSITORY, item_id, repo_id),
        REL_IN_REPOSITORY,
    );
    cs.delete(item_id, label);
}

fn status_changes(
    cs: &mut Changes,
    op: Op,
    item: &Value,
    repo: Option<&Value>,
    id: &str,
    label: &str,
) {
    let error_id = status_error_element_id(id);
    let relation_id = rel_id(REL_ERROR_ON, &error_id, id);
    match derive_status(item) {
        Status::Unknown => {}
        Status::Zero | Status::One(_) => {
            cs.delete(&relation_id, REL_ERROR_ON);
            cs.delete(&error_id, NODE_WORKGRAPH_ERROR);
        }
        Status::Conflict(labels) => {
            let mut props = ElementPropertyMap::new();
            props.text("errorKind", "multiple-status-labels");
            props.text("errorCode", "multiple-status-labels");
            props.text(
                "errorMessage",
                &format!(
                    "expected at most one '{STATUS_PREFIX}' label, found {}: {}",
                    labels.len(),
                    labels.join(", ")
                ),
            );
            props.insert("statusLabels", strings(labels.iter().map(String::as_str)));
            props.text("subjectNodeId", id);
            let is_issue = label == NODE_ISSUE;
            props.text(
                "subjectType",
                if is_issue { "issue" } else { "pullRequest" },
            );
            props.copy("subjectNumber", item.get("number"));
            props.copy("repositoryNameWithOwner", full_name(repo));
            cs.node(op, &error_id, NODE_WORKGRAPH_ERROR, props);
            cs.relation(op, REL_ERROR_ON, &relation_id, &error_id, id);
        }
    }
}

struct CommentNode {
    element_id: String,
    label: &'static str,
    properties: ElementPropertyMap,
    /// Relations from this comment node to other elements, beyond the
    /// `COMMENT_ON`/`ERROR_ON` parent relation.
    specialized_relations: Vec<(&'static str, String)>,
    /// What this comment currently contributes to the lease lifecycle, or
    /// `None` when its contribution is indeterminate. The comment never writes
    /// anchor state itself; the ledger recomputes each anchor from every
    /// surviving contribution.
    lifecycle: Option<LifecycleIntent>,
    /// The anchor this body names, whatever its trust, when the body is a lease
    /// lifecycle artifact. The Source reconciles a task's current comments
    /// before applying such a delivery if it has never seen that task, and
    /// always re-projects the named anchor from the ledger — which is how a
    /// deleted or edited-away artifact removes an anchor the ledger itself
    /// never recorded, such as one a bootstrap projected.
    lifecycle_anchor: Option<String>,
    is_error: bool,
}

impl CommentNode {
    #[allow(clippy::too_many_arguments)]
    fn classify(
        body: &str,
        comment_id: &str,
        task_id: &str,
        ordinary_label: &'static str,
        on_task: bool,
        comment: &Value,
        repo: Option<&Value>,
        lease_trust: Option<&LeaseTrust>,
        editor_identity: Option<&Value>,
    ) -> Self {
        let author = comment.get("user");
        // A lifecycle artifact is only trusted when the *preserved author* and
        // the identity that last edited the comment are both trusted. GitHub
        // keeps `user` as the original author across an edit, so trusting it
        // alone would let anyone with edit rights rewrite a trusted author's
        // ordinary comment into a Lease, a Result, or an Expiration.
        let editor = comment
            .get("editor")
            .filter(|editor| !editor.is_null())
            .or(editor_identity);
        // The editor must hold the *same* role the artifact requires, not merely
        // some lifecycle role. A configured reporter editing a comment into a
        // Lease is exactly as much of a forgery as a stranger doing it: a
        // reporter is not authorized to acquire capacity, and a dispatcher is
        // not authorized to report a Result or an Expiration.
        // A comment that shows it was edited but supplies no editor identity —
        // a pin or an unpin, whose payload names the actor performing *that*
        // action rather than the last editor — cannot be attributed. It is
        // indeterminate rather than trusted or untrusted: the comment makes no
        // statement at all, so it can neither acquire capacity nor overwrite
        // what a reconciliation from GitHub already established.
        let unattributed_edit = editor.is_none() && shows_edit(comment);
        let role_trusted = |role: fn(&LeaseTrust, Option<&Value>) -> bool| {
            !unattributed_edit
                && lease_trust.is_some_and(|trust| {
                    role(trust, author) && editor.is_none_or(|editor| role(trust, Some(editor)))
                })
        };
        let is_dispatcher = role_trusted(|trust, identity| trust.is_dispatcher(identity));
        let is_reporter = role_trusted(|trust, identity| trust.is_reporter(identity));

        let mut props = ElementPropertyMap::new();
        props.table(comment, AUTHOR_PROPS);
        props.copy("repositoryNameWithOwner", full_name(repo));
        // Editor provenance travels with every comment, so a query can see who
        // last rewrote a body as well as who originally authored it.
        if let Some(editor) = editor {
            props.copy("editorLogin", editor.get("login"));
            props.copy("editorId", editor.get("node_id"));
        }
        let classification = if on_task {
            classify_comment(body)
        } else {
            CommentClassification::Ordinary
        };
        let mut lifecycle = Some(LifecycleIntent::Retract {
            comment_node_id: comment_id.to_string(),
        });
        let mut lifecycle_anchor = None;
        let (element_id, label, specialized_relations) = match classification {
            CommentClassification::Ordinary => {
                props.table(comment, COMMENT_PROPS);
                let (created, updated) = (comment.get("created_at"), comment.get("updated_at"));
                if let (Some(created), Some(updated)) = (created, updated) {
                    props.insert("isEdited", ElementValue::Bool(created != updated));
                }
                (comment_id.to_string(), ordinary_label, Vec::new())
            }
            CommentClassification::Assignment(assignment) => {
                props.table(comment, PROVENANCE_PROPS);
                props.text("bodyDigest", &sha256_digest(body));
                props.insert(
                    "version",
                    ElementValue::Integer(i64::from(assignment.version)),
                );
                props.text("agentProfile", &assignment.agent_profile);
                let mut relations = vec![(REL_ASSIGNMENT_FOR, task_id.to_string())];
                match &assignment.worker_id {
                    Some(worker_id) => {
                        props.text("workerId", worker_id);
                        // A v2 Assignment places the task in exactly this
                        // worker's queue. The Source validates the profile but
                        // deliberately does not require the worker to exist or
                        // to be profile-compatible; queries assert that.
                        relations.push((REL_ASSIGNED_TO, worker_element_id(worker_id)));
                    }
                    None => props.insert("workerId", ElementValue::Null),
                }
                (
                    comment_id.to_string(),
                    NODE_WORKGRAPH_TASK_ASSIGNMENT,
                    relations,
                )
            }
            CommentClassification::Result(result) => {
                props.table(comment, PROVENANCE_PROPS);
                props.text("bodyDigest", &sha256_digest(body));
                props.insert("version", ElementValue::Integer(i64::from(result.version)));
                props.text("taskType", result.task_type.as_str());
                props.text("outcome", result.outcome.as_str());
                props.text("summary", &result.summary);
                props.insert(
                    "result",
                    ElementValue::from(&serde_json::json!(result.result)),
                );
                let mut relations = vec![(REL_RESULT_FOR, task_id.to_string())];
                match &result.lease_id {
                    Some(lease_id) => {
                        props.text("leaseId", lease_id);
                        props.insert("trusted", ElementValue::Bool(is_reporter));
                        lifecycle_anchor = Some(AnchorKey::new(task_id, lease_id).element_id());
                        if unattributed_edit {
                            lifecycle = None;
                        }
                        // Only a configured reporter's Result binds and ends a
                        // lease. Any other commenter's Result is still
                        // projected with its provenance, but it reaches no
                        // anchor and moves no lifecycle.
                        if is_reporter {
                            let key = AnchorKey::new(task_id, lease_id);
                            relations.push((REL_RESULT_FOR_LEASE, key.element_id()));
                            lifecycle = Some(LifecycleIntent::End {
                                comment_node_id: comment_id.to_string(),
                                anchor: key,
                                end: EndClaim {
                                    kind: EndKind::Result,
                                    // A Result's authoritative end instant is
                                    // the comment's own GitHub timestamp.
                                    ended_at: comment
                                        .get("created_at")
                                        .and_then(Value::as_str)
                                        .unwrap_or_default()
                                        .to_string(),
                                    lease_comment_node_id: None,
                                },
                            });
                        }
                    }
                    None => props.insert("leaseId", ElementValue::Null),
                }
                (
                    comment_id.to_string(),
                    NODE_WORKGRAPH_TASK_RESULT,
                    relations,
                )
            }
            CommentClassification::Acceptance(acceptance) => {
                props.table(comment, PROVENANCE_PROPS);
                props.text("bodyDigest", &sha256_digest(body));
                props.text("resultCommentNodeId", &acceptance.result_comment_node_id);
                props.text("resultBodyDigest", &acceptance.result_body_digest);
                props.text("summary", &acceptance.summary);
                (
                    comment_id.to_string(),
                    NODE_WORKGRAPH_TASK_RESULT_ACCEPTANCE,
                    vec![(
                        REL_ACCEPTS_RESULT,
                        acceptance.result_comment_node_id.clone(),
                    )],
                )
            }
            CommentClassification::Lease(lease) => {
                props.table(comment, PROVENANCE_PROPS);
                props.text("bodyDigest", &sha256_digest(body));
                props.text("leaseId", &lease.lease_id);
                props.text("assignmentCommentNodeId", &lease.assignment_comment_node_id);
                props.text("workerId", &lease.worker_id);
                props.text("slotId", &lease.slot_id);
                props.text("taskNodeId", task_id);
                props.text("acquiredAt", &lease.acquired_at);
                props.text("expiresAt", &lease.expires_at);
                let key = AnchorKey::new(task_id, &lease.lease_id);
                let anchor = key.element_id();
                props.text("leaseAnchorNodeId", &anchor);
                props.insert("trusted", ElementValue::Bool(is_dispatcher));
                lifecycle_anchor = Some(anchor.clone());
                if unattributed_edit {
                    lifecycle = None;
                }
                let mut relations = vec![
                    (REL_LEASE_FOR, task_id.to_string()),
                    (REL_LEASES_SLOT, worker_slot_element_id(&lease.slot_id)),
                ];
                // Only a configured dispatcher's Lease opens a lease
                // lifecycle. Any other commenter's Lease is projected with its
                // provenance but can never occupy capacity.
                if is_dispatcher {
                    relations.push((REL_LEASE_ANCHOR, anchor));
                    lifecycle = Some(LifecycleIntent::Acquire {
                        comment_node_id: comment_id.to_string(),
                        anchor: key,
                        acquisition: Acquisition {
                            worker_id: lease.worker_id.clone(),
                            slot_id: lease.slot_id.clone(),
                            acquired_at: lease.acquired_at.clone(),
                            expires_at: lease.expires_at.clone(),
                        },
                    });
                }
                (comment_id.to_string(), NODE_WORKGRAPH_TASK_LEASE, relations)
            }
            CommentClassification::LeaseExpiration(expiration) => {
                props.table(comment, PROVENANCE_PROPS);
                props.text("bodyDigest", &sha256_digest(body));
                props.text("leaseId", &expiration.lease_id);
                props.text("leaseCommentNodeId", &expiration.lease_comment_node_id);
                props.text("expiredAt", &expiration.expired_at);
                props.text("reason", &expiration.reason);
                props.insert("trusted", ElementValue::Bool(is_reporter));
                lifecycle_anchor = Some(AnchorKey::new(task_id, &expiration.lease_id).element_id());
                if unattributed_edit {
                    lifecycle = None;
                }
                let mut relations = Vec::new();
                if is_reporter {
                    let key = AnchorKey::new(task_id, &expiration.lease_id);
                    relations.push((REL_EXPIRES_LEASE, key.element_id()));
                    lifecycle = Some(LifecycleIntent::End {
                        comment_node_id: comment_id.to_string(),
                        anchor: key,
                        end: EndClaim {
                            kind: EndKind::Expired,
                            ended_at: expiration.expired_at.clone(),
                            // Only counts against the Lease comment it names.
                            lease_comment_node_id: Some(expiration.lease_comment_node_id.clone()),
                        },
                    });
                }
                (
                    comment_id.to_string(),
                    NODE_WORKGRAPH_TASK_LEASE_EXPIRATION,
                    relations,
                )
            }
            CommentClassification::Invalid(error) => {
                props.table(comment, PROVENANCE_PROPS);
                props.text("bodyDigest", &sha256_digest(body));
                props.text("errorKind", "invalid-workgraph-task-comment");
                props.text("errorCode", error.code);
                props.text("errorMessage", &error.message);
                props.text("sourceCommentBody", body);
                let id = comment_error_element_id(comment_id);
                (id, NODE_WORKGRAPH_ERROR, Vec::new())
            }
        };
        CommentNode {
            element_id,
            label,
            properties: props,
            specialized_relations,
            lifecycle,
            lifecycle_anchor,
            is_error: label == NODE_WORKGRAPH_ERROR,
        }
    }

    fn parent_relation(&self) -> &'static str {
        if self.is_error {
            REL_ERROR_ON
        } else {
            REL_COMMENT_ON
        }
    }

    fn insert(&self, cs: &mut Changes, comment_id: &str, parent: &str) {
        cs.node(
            Insert,
            &self.element_id,
            self.label,
            self.properties.clone(),
        );
        let label = self.parent_relation();
        let id = rel_id(label, comment_id, parent);
        cs.relation(Insert, label, &id, &self.element_id, parent);
        for (label, target) in &self.specialized_relations {
            let id = rel_id(label, comment_id, target);
            cs.relation(Insert, label, &id, comment_id, target);
        }
    }
    fn update(&self, cs: &mut Changes) {
        cs.node(
            Update,
            &self.element_id,
            self.label,
            self.properties.clone(),
        );
    }
    fn delete(&self, cs: &mut Changes, comment_id: &str, parent: &str) {
        for (label, target) in &self.specialized_relations {
            cs.delete(&rel_id(label, comment_id, target), label);
        }
        let label = self.parent_relation();
        cs.delete(&rel_id(label, comment_id, parent), label);
        cs.delete(&self.element_id, self.label);
        // The lease anchor is deliberately *not* deleted. It is a shared,
        // task-scoped join point that several comments may reference, so no
        // single comment may remove it — otherwise deleting one Lease comment
        // would destroy the join point another Lease and its Result still use.
        // An anchor left without a Lease edge is inert: every lifecycle
        // pattern traverses LEASE_ANCHOR from a trusted Lease.
    }
}

/// The lease anchor: the task-scoped join point that lifecycle artifacts bind
/// to, plus the derived lifecycle the workflow defines over it.
///
/// The anchor deliberately carries **no acquisition facts**. `workerId`,
/// `slotId`, `acquiredAt`, `expiresAt`, the assignment binding, and the author
/// all live on the stable `WorkGraphTaskLease` comment node, which has exactly
/// one writer for its whole lifetime. A second Lease comment naming the same
/// `(task, leaseId)` therefore cannot rewrite or delete anything the first
/// Lease authored — every exact check reads the Lease node.
///
/// `leaseId` and `taskNodeId` restate the anchor's own key, so every writer
/// writes byte-identical values for them.
fn anchor_identity_props(lease_id: &str, task_id: &str) -> ElementPropertyMap {
    let mut props = ElementPropertyMap::new();
    props.text("leaseId", lease_id);
    props.text("taskNodeId", task_id);
    props
}

/// A trusted dispatcher's Lease opens the lifecycle.
fn acquired_lease_props(lease: &TaskLease, task_id: &str) -> ElementPropertyMap {
    let mut props = anchor_identity_props(&lease.lease_id, task_id);
    props.insert("isActive", ElementValue::Bool(true));
    props.text("endReason", "none");
    props.insert("endedAt", ElementValue::Null);
    props.insert("endCommentNodeId", ElementValue::Null);
    props
}

/// A trusted reporter's Result or Expiration closes the lifecycle.
fn lease_end_props(
    lease_id: &str,
    task_id: &str,
    end_reason: &str,
    ended_at: Option<&Value>,
    end_comment_id: &str,
) -> ElementPropertyMap {
    let mut props = anchor_identity_props(lease_id, task_id);
    props.insert("isActive", ElementValue::Bool(false));
    props.text("endReason", end_reason);
    props.copy("endedAt", ended_at);
    props.text("endCommentNodeId", end_comment_id);
    props
}

#[derive(Debug, PartialEq, Eq)]
pub enum Status {
    Unknown,
    Zero,
    One(String),
    Conflict(Vec<String>),
}

pub fn derive_status(item: &Value) -> Status {
    let Some(labels) = names(item, "labels", "name") else {
        return Status::Unknown;
    };
    let mut matched: Vec<String> = labels
        .into_iter()
        .filter(|name| name.starts_with(STATUS_PREFIX))
        .map(str::to_string)
        .collect();
    matched.sort();
    match matched.len() {
        0 => Status::Zero,
        1 => Status::One(matched.remove(0)),
        _ => Status::Conflict(matched),
    }
}

fn names<'a>(container: &'a Value, key: &str, field: &'a str) -> Option<Vec<&'a str>> {
    let items = container.get(key)?.as_array()?;
    Some(
        items
            .iter()
            .filter_map(|item| item.get(field).and_then(Value::as_str))
            .collect(),
    )
}

fn item_is_open(item: &Value, path: &str) -> Result<bool, ConvertError> {
    match item.get("state").and_then(Value::as_str) {
        Some(state) if state.eq_ignore_ascii_case("open") => Ok(true),
        Some(state) if state.eq_ignore_ascii_case("closed") => Ok(false),
        _ => Err(invalid(format!(
            "'{path}.state' must be either 'open' or 'closed'"
        ))),
    }
}

fn work_item_action_projects_open(
    action: &str,
    item: &Value,
    path: &str,
) -> Result<bool, ConvertError> {
    let is_open = item_is_open(item, path)?;
    if !is_open && in_table("opened reopened", action) {
        return Err(invalid(format!(
            "'{path}.state' must be 'open' for action '{action}'"
        )));
    }
    Ok(is_open)
}

fn label_details(item: &Value) -> Result<Option<ElementValue>, ConvertError> {
    let Some(labels) = item.get("labels") else {
        return Ok(None);
    };
    let labels = labels
        .as_array()
        .ok_or_else(|| invalid("'labels' must be an array"))?;
    let mut details = Vec::with_capacity(labels.len());

    for (index, label) in labels.iter().enumerate() {
        let name = label
            .get("name")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| invalid(format!("'labels[{index}].name' must be nonempty text")))?;
        let node_id = label
            .get("node_id")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| invalid(format!("'labels[{index}].node_id' must be nonempty text")))?;
        details.push(ElementValue::from(&serde_json::json!({
            "name": name,
            "nodeId": node_id,
        })));
    }

    Ok(Some(ElementValue::List(details)))
}

fn strings<'a>(values: impl Iterator<Item = &'a str>) -> ElementValue {
    ElementValue::List(values.map(|v| ElementValue::String(Arc::from(v))).collect())
}

fn full_name(repo: Option<&Value>) -> Option<&Value> {
    repo.and_then(|r| r.get("full_name"))
}

fn owner_in_org(repo: Option<&Value>, organization: &str) -> bool {
    repo.and_then(|r| r.pointer("/owner/login"))
        .and_then(Value::as_str)
        .is_some_and(|owner| owner.eq_ignore_ascii_case(organization))
}

fn repository_is_authoritative_for(issue: &Value, repository: &Value) -> bool {
    let same_node_id = issue
        .pointer("/repository/node_id")
        .and_then(Value::as_str)
        .zip(repository.get("node_id").and_then(Value::as_str))
        .is_some_and(|(issue_id, repo_id)| issue_id == repo_id);
    if same_node_id {
        return true;
    }

    let Some(issue_url) = issue.get("repository_url").and_then(Value::as_str) else {
        return false;
    };
    if repository.get("url").and_then(Value::as_str) == Some(issue_url) {
        return true;
    }
    issue_url
        .rsplit_once("/repos/")
        .map(|(_, name)| name.trim_end_matches('/'))
        .zip(repository.get("full_name").and_then(Value::as_str))
        .is_some_and(|(issue_name, repo_name)| issue_name.eq_ignore_ascii_case(repo_name))
}

fn issue_repository_name(issue: &Value) -> Option<&str> {
    issue
        .get("repository_url")
        .and_then(Value::as_str)?
        .rsplit_once("/repos/")
        .map(|(_, name)| name.trim_end_matches('/'))?
        .rsplit_once('/')
        .map(|(_, name)| name)
        .filter(|name| !name.is_empty())
}

fn required_database_id(value: &Value, pointer: &str) -> Result<String, ConvertError> {
    let id = value
        .pointer(pointer)
        .ok_or_else(|| invalid(format!("missing '{pointer}'")))?;
    if let Some(id) = id.as_u64() {
        return Ok(id.to_string());
    }
    if let Some(id) = id.as_i64().filter(|id| *id >= 0) {
        return Ok(id.to_string());
    }
    id.as_str()
        .filter(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()))
        .map(str::to_string)
        .ok_or_else(|| invalid(format!("'{pointer}' must be a non-negative database ID")))
}

fn required_str(value: &Value, pointer: &str) -> Result<String, ConvertError> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
        .ok_or_else(|| invalid(format!("missing or empty '{pointer}'")))
}

fn rel_id(label: &str, from: &str, to: &str) -> String {
    format!("{label}:{from}:{to}")
}

/// The outcome of loading the configured worker file, as the projection sees it.
pub enum WorkerProjection<'a> {
    /// The file was fetched and strictly validated.
    Loaded {
        file: &'a WorkerFile,
        content: &'a WorkerFileContent,
    },
    /// The configured file is deterministically unusable. No worker is
    /// projected and the failure becomes an explicit `WorkGraphError` node.
    Rejected(&'a WorkGraphError),
}

/// Convert a worker-file outcome into graph changes.
///
/// `retiring` maps a `workerId` to the highest slot number ever projected for
/// it, so a capacity reduction can keep the excess slot nodes addressable while
/// taking them out of dispatch. Callers that hold no prior projection state
/// (the bootstrapper builds a fresh snapshot) pass an empty map.
///
/// `removed` lists workers that were projected before and are no longer
/// configured; they and their slots are removed outright.
pub fn worker_changes(
    source_id: &str,
    effective_from: u64,
    location: &WorkerFileLocation,
    projection: &WorkerProjection<'_>,
    retiring: &BTreeMap<String, u32>,
    removed: &BTreeMap<String, u32>,
) -> Vec<SourceChange> {
    let mut cs = Changes::new(source_id, effective_from);
    let error_id = worker_config_error_element_id();

    match projection {
        WorkerProjection::Rejected(error) => {
            let mut props = ElementPropertyMap::new();
            props.text("errorKind", "invalid-workgraph-worker-config");
            props.text("errorCode", error.code);
            props.text("errorMessage", &error.message);
            props.text("configRepository", &location.repository);
            props.text("configRef", &location.r#ref);
            props.text("configPath", &location.path);
            cs.node(Update, &error_id, NODE_WORKGRAPH_ERROR, props);
            // A rejected configuration deliberately leaves the previously
            // projected workers untouched. Silently emptying the pool would
            // make a broken config look like "no capacity configured".
            return cs.changes;
        }
        WorkerProjection::Loaded { file, content } => {
            cs.delete(&error_id, NODE_WORKGRAPH_ERROR);
            for (worker_id, slot_count) in removed {
                delete_worker(&mut cs, worker_id, *slot_count);
            }
            for worker in &file.workers {
                let worker_element = worker_element_id(&worker.worker_id);
                let mut props = ElementPropertyMap::new();
                props.text("workerId", &worker.worker_id);
                props.text("agentProfile", &worker.agent_profile);
                props.insert(
                    "configuredSlotCount",
                    ElementValue::Integer(i64::from(worker.slots)),
                );
                props.text("leaseDuration", &worker.lease_duration);
                props.insert(
                    "leaseDurationSeconds",
                    ElementValue::Integer(worker.lease_duration_seconds),
                );
                props.insert(
                    "workerFileVersion",
                    ElementValue::Integer(file.version as i64),
                );
                props.text("configRepository", &location.repository);
                props.text("configRef", &location.r#ref);
                props.text("configPath", &location.path);
                props.text("configBlobOid", &content.oid);
                props.text("configDigest", &sha256_digest(&content.text));
                cs.node(Update, &worker_element, NODE_WORKGRAPH_WORKER, props);

                let highest = (*retiring.get(&worker.worker_id).unwrap_or(&0))
                    .max(worker.slots)
                    // The retirement ledger is Source-owned durable state, but
                    // clamp it anyway so a corrupted entry can never make the
                    // projection unbounded.
                    .min(MAX_WORKER_SLOTS);
                for slot_number in 1..=highest {
                    let slot = slot_id(&worker.worker_id, slot_number);
                    let slot_element = worker_slot_element_id(&slot);
                    // Slots above the configured count are retired, not
                    // deleted: an already leased excess slot keeps a valid
                    // `LEASES_SLOT` target until its lease reaches a Result or
                    // an Expiration, while never being offered again.
                    let enabled = slot_number <= worker.slots;
                    let mut props = ElementPropertyMap::new();
                    props.text("slotId", &slot);
                    props.insert("slotNumber", ElementValue::Integer(i64::from(slot_number)));
                    props.text("workerId", &worker.worker_id);
                    props.text("agentProfile", &worker.agent_profile);
                    props.insert("enabled", ElementValue::Bool(enabled));
                    props.insert("retiring", ElementValue::Bool(!enabled));
                    props.text("leaseDuration", &worker.lease_duration);
                    props.insert(
                        "leaseDurationSeconds",
                        ElementValue::Integer(worker.lease_duration_seconds),
                    );
                    props.text("configRepository", &location.repository);
                    props.text("configRef", &location.r#ref);
                    props.text("configPath", &location.path);
                    cs.node(Update, &slot_element, NODE_WORKGRAPH_WORKER_SLOT, props);
                    let relation = rel_id(REL_HAS_SLOT, &worker_element, &slot_element);
                    cs.relation(
                        Update,
                        REL_HAS_SLOT,
                        &relation,
                        &worker_element,
                        &slot_element,
                    );
                }
            }
        }
    }
    cs.changes
}

/// Project every anchor named in `affected` from the ledger's current state.
///
/// An anchor with no surviving trusted acquisition is deleted rather than
/// materialized, so an end naming a lease that was never acquired leaves
/// nothing a query could bind to or count.
pub fn anchor_changes(
    source_id: &str,
    effective_from: u64,
    ledger: &LeaseLedger,
    affected: impl IntoIterator<Item = String>,
) -> Vec<SourceChange> {
    let mut cs = Changes::new(source_id, effective_from);
    for anchor in affected {
        match ledger.project(&anchor) {
            Some(state) => cs.node(
                Update,
                &anchor,
                NODE_WORKGRAPH_TASK_LEASE_ANCHOR,
                anchor_props(&state),
            ),
            None => cs.delete(&anchor, NODE_WORKGRAPH_TASK_LEASE_ANCHOR),
        }
    }
    cs.changes
}

fn anchor_props(state: &AnchorState) -> ElementPropertyMap {
    let mut props = ElementPropertyMap::new();
    props.text("leaseId", &state.key.lease_id);
    props.text("taskNodeId", &state.key.task_node_id);
    props.insert("isActive", ElementValue::Bool(state.is_active));
    props.text("endReason", state.end_reason);
    match &state.ended_at {
        Some(ended_at) => props.text("endedAt", ended_at),
        None => props.insert("endedAt", ElementValue::Null),
    }
    match &state.end_comment_node_id {
        Some(comment) => props.text("endCommentNodeId", comment),
        None => props.insert("endCommentNodeId", ElementValue::Null),
    }
    props.insert(
        "acquisitionCount",
        ElementValue::Integer(state.acquisition_count as i64),
    );
    props.insert(
        "endClaimCount",
        ElementValue::Integer(state.end_claim_count as i64),
    );
    props
}

fn delete_worker(cs: &mut Changes, worker_id: &str, slot_count: u32) {
    let worker_element = worker_element_id(worker_id);
    for slot_number in 1..=slot_count.min(MAX_WORKER_SLOTS) {
        let slot_element = worker_slot_element_id(&slot_id(worker_id, slot_number));
        cs.delete(
            &rel_id(REL_HAS_SLOT, &worker_element, &slot_element),
            REL_HAS_SLOT,
        );
        cs.delete(&slot_element, NODE_WORKGRAPH_WORKER_SLOT);
    }
    cs.delete(&worker_element, NODE_WORKGRAPH_WORKER);
}

fn org_props(payload: &Value) -> ElementPropertyMap {
    let mut props = ElementPropertyMap::new();
    if let Some(org) = payload.get("organization") {
        props.table(org, ORGANIZATION_PROPS);
    }
    props
}

fn repository_props(repo: &Value) -> ElementPropertyMap {
    let mut props = ElementPropertyMap::new();
    props.table(repo, REPOSITORY_PROPS);
    props.timestamp("createdAt", repo.get("created_at"));
    props.timestamp("updatedAt", repo.get("updated_at"));
    props
}

fn sha256_digest(body: &str) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(body)))
}

fn normalize_issue_state(props: &mut ElementPropertyMap, issue: &Value) {
    let state = issue
        .get("state")
        .and_then(Value::as_str)
        .map(str::to_ascii_lowercase);
    if let Some(state) = &state {
        props.text("state", state);
    }
    props.insert(
        "isOpen",
        ElementValue::Bool(state.as_deref() == Some("open")),
    );
    if let Some(state_reason) = issue.get("state_reason").and_then(Value::as_str) {
        props.text("stateReason", &state_reason.to_ascii_lowercase());
    }
}

fn project_issue_labels(props: &mut ElementPropertyMap, labels: &[&str]) {
    let status_labels: Vec<&str> = labels
        .iter()
        .copied()
        .filter(|name| name.starts_with(STATUS_PREFIX))
        .collect();
    let current_status = match status_labels.as_slice() {
        [] => "none",
        [status] => status,
        _ => "error",
    };
    props.insert("statusLabels", strings(status_labels.iter().copied()));
    props.text("currentStatus", current_status);

    let workgraph_labels: Vec<&str> = labels
        .iter()
        .copied()
        .filter(|name| name.starts_with("workgraph:"))
        .collect();
    let workgraph_include = !workgraph_labels
        .iter()
        .any(|name| matches!(*name, "workgraph:ignore" | "workgraph:error"));
    props.insert("workgraphLabels", strings(workgraph_labels.iter().copied()));
    props.insert("workgraphInclude", ElementValue::Bool(workgraph_include));
}

fn work_item_props(
    item: &Value,
    repo: Option<&Value>,
    label: &str,
) -> Result<ElementPropertyMap, ConvertError> {
    let is_issue = label != NODE_PULL_REQUEST;
    let mut props = ElementPropertyMap::new();
    props.table(item, WORK_ITEM_PROPS);
    props.table(item, AUTHOR_PROPS);
    let variant = if is_issue {
        ISSUE_ONLY_PROPS
    } else {
        PULL_REQUEST_ONLY_PROPS
    };
    props.table(item, variant);
    if is_issue {
        normalize_issue_state(&mut props, item);
    }
    let body = item.get("body").and_then(Value::as_str).unwrap_or("");
    props.text("bodyDigest", &sha256_digest(body));
    props.copy("repositoryNameWithOwner", full_name(repo));
    if label == NODE_ISSUE {
        project_issue_labels(&mut props, &[]);
    }
    if let Some(assignees) = names(item, "assignees", "login") {
        props.insert("assignees", strings(assignees.into_iter()));
    }
    if let Some(details) = label_details(item)? {
        props.insert("labelDetails", details);
        let labels =
            names(item, "labels", "name").expect("label details validate the labels array");
        if label == NODE_ISSUE {
            project_issue_labels(&mut props, &labels);
        }
        props.insert("labels", strings(labels.into_iter()));
        match if label == NODE_WORKGRAPH_TASK {
            Status::Zero
        } else {
            derive_status(item)
        } {
            Status::One(full) => {
                props.text("status", full.strip_prefix(STATUS_PREFIX).unwrap_or(&full));
                props.text("statusLabel", &full);
            }
            _ => {
                props.insert("status", ElementValue::Null);
                props.insert("statusLabel", ElementValue::Null);
            }
        }
    }
    Ok(props)
}

trait Props {
    fn text(&mut self, key: &str, value: &str);
    fn copy(&mut self, key: &str, value: Option<&Value>);
    fn table(&mut self, container: &Value, table: &str);
    fn timestamp(&mut self, key: &str, value: Option<&Value>);
}

impl Props for ElementPropertyMap {
    fn text(&mut self, key: &str, value: &str) {
        self.insert(key, ElementValue::String(Arc::from(value)));
    }
    fn copy(&mut self, key: &str, value: Option<&Value>) {
        if let Some(value) = value {
            self.insert(key, ElementValue::from(value));
        }
    }
    fn table(&mut self, container: &Value, table: &str) {
        for entry in table.split_whitespace() {
            let (key, path) = entry
                .split_once('=')
                .expect("property table entries are 'name=payload/pointer'");
            self.copy(key, container.pointer(&format!("/{path}")));
        }
    }
    fn timestamp(&mut self, key: &str, value: Option<&Value>) {
        match value.and_then(Value::as_i64).and_then(|secs| {
            DateTime::from_timestamp(secs, 0)
                .map(|dt| dt.to_rfc3339_opts(SecondsFormat::Secs, true))
        }) {
            Some(rfc3339) => self.text(key, &rfc3339),
            None => self.copy(key, value),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Op {
    Insert,
    Update,
}

struct Changes<'a> {
    source_id: &'a str,
    effective_from: u64,
    changes: Vec<SourceChange>,
    lifecycle: Vec<LifecycleIntent>,
    lifecycle_scope: Option<LifecycleScope>,
    lifecycle_anchors: Vec<String>,
}

impl<'a> Changes<'a> {
    fn new(source_id: &'a str, effective_from: u64) -> Self {
        Self {
            source_id,
            effective_from,
            changes: Vec::new(),
            lifecycle: Vec::new(),
            lifecycle_scope: None,
            lifecycle_anchors: Vec::new(),
        }
    }
    fn metadata(&self, id: &str, label: &str) -> ElementMetadata {
        ElementMetadata {
            reference: ElementReference::new(self.source_id, id),
            labels: Arc::from(vec![Arc::from(label)]),
            effective_from: self.effective_from,
        }
    }
    fn push(&mut self, op: Op, element: Element) {
        self.changes.push(match op {
            Insert => SourceChange::Insert { element },
            Update => SourceChange::Update { element },
        });
    }
    fn node(&mut self, op: Op, id: &str, label: &str, properties: ElementPropertyMap) {
        let element = Element::Node {
            metadata: self.metadata(id, label),
            properties,
        };
        self.push(op, element);
    }

    /// `from` is the relation tail and `to` the head, matching drasi-core's
    /// `(from)-[r]->(to)` convention of `in_node = from`, `out_node = to`.
    fn relation(&mut self, op: Op, label: &str, id: &str, from: &str, to: &str) {
        let element = Element::Relation {
            metadata: self.metadata(id, label),
            in_node: ElementReference::new(self.source_id, from),
            out_node: ElementReference::new(self.source_id, to),
            properties: ElementPropertyMap::new(),
        };
        self.push(op, element);
    }

    fn delete(&mut self, id: &str, label: &str) {
        let metadata = self.metadata(id, label);
        self.changes.push(SourceChange::Delete { metadata });
    }
}
