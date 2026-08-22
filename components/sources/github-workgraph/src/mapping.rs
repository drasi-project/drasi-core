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

use crate::agents::{AgentFile, AgentFileContent, AgentFileLocation, MAX_AGENT_SLOTS};
use crate::config::{LeaseTrust, RepositoryFilter, TaskIssueType};
use crate::lease_ledger::{
    ActiveLease, AgentRuntime, AllocationArtifact, AllocationDelta, AllocationEvent,
};
use crate::workgraph::{
    agent_config_error_element_id, agent_element_id, agent_slot_element_id, classify_comment,
    classify_task_body, comment_error_element_id, lease_element_id, slot_id,
    status_error_element_id, task_error_element_id, CommentClassification, TaskClassification,
    TaskType, WorkGraphError,
};
use chrono::{DateTime, SecondsFormat};
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
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
    NODE_WORKGRAPH_TASK_FEEDBACK = "WorkGraphTaskFeedback",
    NODE_WORKGRAPH_TASK_RESULT_ACCEPTANCE = "WorkGraphTaskResultAcceptance",
    NODE_WORKGRAPH_TASK_LEASE = "WorkGraphTaskLease",
    NODE_WORKGRAPH_AGENT = "WorkGraphAgent",
    NODE_WORKGRAPH_AGENT_SLOT = "WorkGraphAgentSlot",
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
    REL_FEEDBACK_FOR = "FEEDBACK_FOR",
    REL_ACCEPTS_RESULT = "ACCEPTS_RESULT",
    REL_TASK_FOR = "TASK_FOR",
    REL_HAS_SLOT = "HAS_SLOT",
    REL_LEASE_FOR = "LEASE_FOR",
    REL_LEASES_SLOT = "LEASES_SLOT",
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

pub struct Conversion {
    pub changes: Vec<SourceChange>,
    pub allocation: Option<AllocationEvent>,
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
            allocation: cs.allocation,
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
        if previous_task && (!current_task || in_table("closed deleted transferred", d.action)) {
            cs.allocation = Some(AllocationEvent::TaskCancelled {
                task_node_id: issue_id.clone(),
            });
        }

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
                cs.allocation = Some(AllocationEvent::TaskCancelled {
                    task_node_id: issue_id.clone(),
                });
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
        let task_open = !on_task || item_is_open(issue, "issue")?;
        let task_type =
            match classify_task_body(issue.get("body").and_then(Value::as_str).unwrap_or("")) {
                TaskClassification::Task(task) => Some(task.task_type),
                TaskClassification::Invalid(_) => None,
            };
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
                task_type,
            )
        };
        let current = classify_body(comment.get("body").and_then(Value::as_str).unwrap_or(""));
        if on_task && in_table("created edited deleted", action) {
            let artifact = (action != "deleted")
                .then(|| current.allocation.clone())
                .flatten();
            let artifact = match artifact {
                Some(AllocationArtifact::Assignment { .. }) if !task_open => None,
                artifact => artifact,
            };
            cs.allocation = Some(AllocationEvent::Comment {
                comment_node_id: comment_id.clone(),
                task_node_id: parent.clone(),
                artifact,
            });
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
                            || prev.allocation != current.allocation =>
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
    allocation: Option<AllocationArtifact>,
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
        task_type: Option<TaskType>,
    ) -> Self {
        let author = comment.get("user");
        // the identity that last edited the comment are both trusted. GitHub
        // keeps `user` as the original author across an edit, so trusting it
        // alone would let anyone with edit rights rewrite a trusted author's
        // ordinary comment into a protocol artifact.
        let editor = comment
            .get("editor")
            .filter(|editor| !editor.is_null())
            .or(editor_identity);
        let unattributed_edit = editor.is_none() && shows_edit(comment);
        let role_trusted = |role: fn(&LeaseTrust, Option<&Value>) -> bool| {
            !unattributed_edit
                && lease_trust.is_some_and(|trust| {
                    role(trust, author) && editor.is_none_or(|editor| role(trust, Some(editor)))
                })
        };
        let is_assigner = role_trusted(|trust, identity| trust.is_assigner(identity));
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
        let mut allocation = None;
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
                let body_digest = sha256_digest(body);
                props.text("bodyDigest", &body_digest);
                props.insert("version", ElementValue::Integer(1));
                props.text("agentId", &assignment.agent_id);
                props.insert("trusted", ElementValue::Bool(is_assigner));
                if let Some(task_type) = task_type {
                    allocation = Some(AllocationArtifact::Assignment {
                        trusted: is_assigner,
                        task_type,
                        agent_id: assignment.agent_id.clone(),
                        created_at: comment
                            .get("created_at")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    });
                }
                (
                    comment_id.to_string(),
                    NODE_WORKGRAPH_TASK_ASSIGNMENT,
                    vec![
                        (REL_ASSIGNMENT_FOR, task_id.to_string()),
                        (REL_ASSIGNED_TO, agent_element_id(&assignment.agent_id)),
                    ],
                )
            }
            CommentClassification::Result(result) => {
                props.table(comment, PROVENANCE_PROPS);
                let body_digest = sha256_digest(body);
                props.text("bodyDigest", &body_digest);
                props.insert("version", ElementValue::Integer(1));
                props.text("taskType", result.task_type.as_str());
                props.text("leaseId", &result.lease_id);
                props.text("outcome", result.outcome.as_str());
                props.text("summary", &result.summary);
                props.insert(
                    "result",
                    ElementValue::from(&serde_json::json!(result.result)),
                );
                props.insert("trusted", ElementValue::Bool(false));
                allocation = Some(AllocationArtifact::Result {
                    reporter_trusted: is_reporter,
                    task_type: result.task_type,
                    lease_id: result.lease_id.clone(),
                    outcome: result.outcome,
                    body_digest,
                });
                (
                    comment_id.to_string(),
                    NODE_WORKGRAPH_TASK_RESULT,
                    vec![(REL_RESULT_FOR, task_id.to_string())],
                )
            }
            CommentClassification::Feedback(feedback) => {
                props.table(comment, PROVENANCE_PROPS);
                let body_digest = sha256_digest(body);
                props.text("bodyDigest", &body_digest);
                props.text("resultCommentNodeId", &feedback.result_comment_node_id);
                props.text("resultBodyDigest", &feedback.result_body_digest);
                props.text("feedback", &feedback.feedback);
                props.insert("trusted", ElementValue::Bool(false));
                allocation = Some(AllocationArtifact::Feedback {
                    reporter_trusted: is_reporter,
                    result_comment_node_id: feedback.result_comment_node_id.clone(),
                    result_body_digest: feedback.result_body_digest.clone(),
                    body_digest,
                });
                (
                    comment_id.to_string(),
                    NODE_WORKGRAPH_TASK_FEEDBACK,
                    vec![(REL_FEEDBACK_FOR, feedback.result_comment_node_id.clone())],
                )
            }
            CommentClassification::Acceptance(acceptance) => {
                props.table(comment, PROVENANCE_PROPS);
                let body_digest = sha256_digest(body);
                props.text("bodyDigest", &body_digest);
                props.text("resultCommentNodeId", &acceptance.result_comment_node_id);
                props.text("resultBodyDigest", &acceptance.result_body_digest);
                props.text("summary", &acceptance.summary);
                props.insert("trusted", ElementValue::Bool(false));
                allocation = Some(AllocationArtifact::Acceptance {
                    reporter_trusted: is_reporter,
                    result_comment_node_id: acceptance.result_comment_node_id.clone(),
                    result_body_digest: acceptance.result_body_digest.clone(),
                    body_digest,
                });
                (
                    comment_id.to_string(),
                    NODE_WORKGRAPH_TASK_RESULT_ACCEPTANCE,
                    vec![(
                        REL_ACCEPTS_RESULT,
                        acceptance.result_comment_node_id.clone(),
                    )],
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
            allocation,
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
    }
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

/// The outcome of loading the configured agent file, as the projection sees it.
pub enum AgentProjection<'a> {
    /// The file was fetched and strictly validated.
    Loaded {
        file: &'a AgentFile,
        content: &'a AgentFileContent,
    },
    /// The configured file is deterministically unusable. No agent is
    /// projected and the failure becomes an explicit `WorkGraphError` node.
    Rejected(&'a WorkGraphError),
}

/// Convert an agent-file outcome into graph changes.
///
pub fn agent_changes(
    source_id: &str,
    effective_from: u64,
    location: &AgentFileLocation,
    projection: &AgentProjection<'_>,
    retiring: &BTreeMap<String, BTreeSet<u32>>,
    removed: &BTreeMap<String, BTreeSet<u32>>,
) -> Vec<SourceChange> {
    let mut cs = Changes::new(source_id, effective_from);
    let error_id = agent_config_error_element_id();

    match projection {
        AgentProjection::Rejected(error) => {
            let mut props = ElementPropertyMap::new();
            props.text("errorKind", "invalid-workgraph-agent-config");
            props.text("errorCode", error.code);
            props.text("errorMessage", &error.message);
            props.text("configRepository", &location.repository);
            props.text("configRef", &location.r#ref);
            props.text("configPath", &location.path);
            cs.node(Update, &error_id, NODE_WORKGRAPH_ERROR, props);
            // A rejected configuration deliberately leaves the previously
            // projected agents untouched. Silently emptying the pool would
            // make a broken config look like "no capacity configured".
            return cs.changes;
        }
        AgentProjection::Loaded { file, content } => {
            cs.delete(&error_id, NODE_WORKGRAPH_ERROR);
            for (agent_id, slots) in removed {
                delete_agent(&mut cs, agent_id, slots);
            }
            for agent in &file.agents {
                let agent_element = agent_element_id(&agent.agent_id);
                let mut props = ElementPropertyMap::new();
                props.text("agentId", &agent.agent_id);
                props.insert(
                    "configuredSlotCount",
                    ElementValue::Integer(i64::from(agent.slots)),
                );
                props.insert("queueDepth", ElementValue::Integer(0));
                props.insert("activeLeaseCount", ElementValue::Integer(0));
                props.insert(
                    "availableSlotCount",
                    ElementValue::Integer(i64::from(agent.slots)),
                );
                props.text("leaseDuration", &agent.lease_duration);
                props.insert(
                    "leaseDurationSeconds",
                    ElementValue::Integer(agent.lease_duration_seconds),
                );
                props.insert(
                    "agentFileVersion",
                    ElementValue::Integer(file.version as i64),
                );
                props.text("configRepository", &location.repository);
                props.text("configRef", &location.r#ref);
                props.text("configPath", &location.path);
                props.text("configBlobOid", &content.oid);
                props.text("configDigest", &sha256_digest(&content.text));
                cs.node(Update, &agent_element, NODE_WORKGRAPH_AGENT, props);

                let retiring = retiring.get(&agent.agent_id).cloned().unwrap_or_default();
                let slots: BTreeSet<u32> =
                    (1..=agent.slots).chain(retiring.iter().copied()).collect();
                for slot_number in slots {
                    let slot = slot_id(&agent.agent_id, slot_number);
                    let slot_element = agent_slot_element_id(&slot);
                    let enabled = slot_number <= agent.slots;
                    let mut props = ElementPropertyMap::new();
                    props.text("slotId", &slot);
                    props.insert("slotNumber", ElementValue::Integer(i64::from(slot_number)));
                    props.text("agentId", &agent.agent_id);
                    props.insert("enabled", ElementValue::Bool(enabled));
                    props.insert("retiring", ElementValue::Bool(!enabled));
                    props.text("leaseDuration", &agent.lease_duration);
                    props.insert(
                        "leaseDurationSeconds",
                        ElementValue::Integer(agent.lease_duration_seconds),
                    );
                    props.text("configRepository", &location.repository);
                    props.text("configRef", &location.r#ref);
                    props.text("configPath", &location.path);
                    cs.node(Update, &slot_element, NODE_WORKGRAPH_AGENT_SLOT, props);
                    let relation = rel_id(REL_HAS_SLOT, &agent_element, &slot_element);
                    cs.relation(
                        Update,
                        REL_HAS_SLOT,
                        &relation,
                        &agent_element,
                        &slot_element,
                    );
                }
            }
        }
    }
    cs.changes
}

pub fn set_artifact_trusted(changes: &mut [SourceChange], comment_id: &str, trusted: bool) {
    for change in changes {
        let element = match change {
            SourceChange::Insert { element } | SourceChange::Update { element } => element,
            _ => continue,
        };
        if element.get_reference().element_id.as_ref() != comment_id {
            continue;
        }
        if let Element::Node {
            metadata,
            properties,
        } = element
        {
            let protocol_artifact = metadata.labels.iter().any(|label| {
                matches!(
                    label.as_ref(),
                    NODE_WORKGRAPH_TASK_ASSIGNMENT
                        | NODE_WORKGRAPH_TASK_RESULT
                        | NODE_WORKGRAPH_TASK_FEEDBACK
                        | NODE_WORKGRAPH_TASK_RESULT_ACCEPTANCE
                )
            });
            if protocol_artifact {
                properties.insert("trusted", ElementValue::Bool(trusted));
            }
        }
    }
}
pub fn allocation_changes(
    source_id: &str,
    effective_from: u64,
    delta: &AllocationDelta,
    runtime: &BTreeMap<String, AgentRuntime>,
) -> Vec<SourceChange> {
    let mut cs = Changes::new(source_id, effective_from);
    for lease in &delta.ended {
        delete_lease(&mut cs, lease);
    }
    for (agent_id, slot_number) in &delta.removed_slots {
        let agent = agent_element_id(agent_id);
        let slot = agent_slot_element_id(&slot_id(agent_id, *slot_number));
        cs.delete(&rel_id(REL_HAS_SLOT, &agent, &slot), REL_HAS_SLOT);
        cs.delete(&slot, NODE_WORKGRAPH_AGENT_SLOT);
    }
    for agent_id in &delta.removed_agents {
        cs.delete(&agent_element_id(agent_id), NODE_WORKGRAPH_AGENT);
    }
    for assignment_id in &delta.untrusted_assignments {
        let mut props = ElementPropertyMap::new();
        props.insert("trusted", ElementValue::Bool(false));
        cs.node(Update, assignment_id, NODE_WORKGRAPH_TASK_ASSIGNMENT, props);
    }
    for feedback_id in &delta.untrusted_feedback {
        let mut props = ElementPropertyMap::new();
        props.insert("trusted", ElementValue::Bool(false));
        cs.node(Update, feedback_id, NODE_WORKGRAPH_TASK_FEEDBACK, props);
    }
    for acceptance_id in &delta.untrusted_acceptances {
        let mut props = ElementPropertyMap::new();
        props.insert("trusted", ElementValue::Bool(false));
        cs.node(
            Update,
            acceptance_id,
            NODE_WORKGRAPH_TASK_RESULT_ACCEPTANCE,
            props,
        );
    }
    for lease in &delta.started {
        upsert_lease(&mut cs, lease);
    }
    for agent_id in &delta.affected_agents {
        let Some(runtime) = runtime.get(agent_id) else {
            continue;
        };
        let agent = agent_element_id(agent_id);
        let mut props = ElementPropertyMap::new();
        props.insert(
            "configuredSlotCount",
            ElementValue::Integer(i64::from(runtime.configured_slots)),
        );
        props.insert(
            "queueDepth",
            ElementValue::Integer(runtime.queue_depth as i64),
        );
        props.insert(
            "activeLeaseCount",
            ElementValue::Integer(runtime.active_lease_count as i64),
        );
        props.insert(
            "availableSlotCount",
            ElementValue::Integer(runtime.available_slot_count as i64),
        );
        cs.node(Update, &agent, NODE_WORKGRAPH_AGENT, props);
        for slot_number in &runtime.retiring_slots {
            let slot_id = slot_id(agent_id, *slot_number);
            let slot = agent_slot_element_id(&slot_id);
            let mut props = ElementPropertyMap::new();
            props.text("slotId", &slot_id);
            props.insert("slotNumber", ElementValue::Integer(i64::from(*slot_number)));
            props.text("agentId", agent_id);
            props.insert("enabled", ElementValue::Bool(false));
            props.insert("retiring", ElementValue::Bool(true));
            cs.node(Update, &slot, NODE_WORKGRAPH_AGENT_SLOT, props);
            cs.relation(
                Update,
                REL_HAS_SLOT,
                &rel_id(REL_HAS_SLOT, &agent, &slot),
                &agent,
                &slot,
            );
        }
    }
    cs.changes
}

fn upsert_lease(cs: &mut Changes, lease: &ActiveLease) {
    let element_id = lease_element_id(&lease.task_node_id, &lease.lease_id);
    let mut props = ElementPropertyMap::new();
    props.text("leaseId", &lease.lease_id);
    props.text("taskNodeId", &lease.task_node_id);
    props.text("assignmentCommentNodeId", &lease.assignment_comment_node_id);
    props.text("agentId", &lease.agent_id);
    props.text("slotId", &lease.slot_id);
    props.text("taskType", lease.task_type.as_str());
    props.text("acquiredAt", &lease.acquired_at);
    props.text("expiresAt", &lease.expires_at);
    cs.node(Update, &element_id, NODE_WORKGRAPH_TASK_LEASE, props);
    cs.relation(
        Update,
        REL_LEASE_FOR,
        &rel_id(REL_LEASE_FOR, &element_id, &lease.task_node_id),
        &element_id,
        &lease.task_node_id,
    );
    let slot = agent_slot_element_id(&lease.slot_id);
    cs.relation(
        Update,
        REL_LEASES_SLOT,
        &rel_id(REL_LEASES_SLOT, &element_id, &slot),
        &element_id,
        &slot,
    );
}

fn delete_lease(cs: &mut Changes, lease: &ActiveLease) {
    let element_id = lease_element_id(&lease.task_node_id, &lease.lease_id);
    cs.delete(
        &rel_id(REL_LEASE_FOR, &element_id, &lease.task_node_id),
        REL_LEASE_FOR,
    );
    cs.delete(
        &rel_id(
            REL_LEASES_SLOT,
            &element_id,
            &agent_slot_element_id(&lease.slot_id),
        ),
        REL_LEASES_SLOT,
    );
    cs.delete(&element_id, NODE_WORKGRAPH_TASK_LEASE);
}
fn delete_agent(cs: &mut Changes, agent_id: &str, slots: &BTreeSet<u32>) {
    let agent_element = agent_element_id(agent_id);
    for slot_number in slots
        .iter()
        .copied()
        .filter(|slot| *slot <= MAX_AGENT_SLOTS)
    {
        let slot_element = agent_slot_element_id(&slot_id(agent_id, slot_number));
        cs.delete(
            &rel_id(REL_HAS_SLOT, &agent_element, &slot_element),
            REL_HAS_SLOT,
        );
        cs.delete(&slot_element, NODE_WORKGRAPH_AGENT_SLOT);
    }
    cs.delete(&agent_element, NODE_WORKGRAPH_AGENT);
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
    allocation: Option<AllocationEvent>,
}

impl<'a> Changes<'a> {
    fn new(source_id: &'a str, effective_from: u64) -> Self {
        Self {
            source_id,
            effective_from,
            changes: Vec::new(),
            allocation: None,
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
