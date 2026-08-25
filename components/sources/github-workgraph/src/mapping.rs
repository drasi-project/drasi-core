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

use crate::workgraph::{
    assignment_element_id, classify, comment_error_element_id, status_error_element_id,
    Classification,
};
use chrono::{DateTime, SecondsFormat};
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
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
    NODE_WORKGRAPH_ASSIGNMENT = "WorkGraphAssignment",
    NODE_WORKGRAPH_RESULT = "WorkGraphResult",
    NODE_WORKGRAPH_ERROR = "WorkGraphError",
);

labels!(
    RELATION_LABELS,
    REL_IN_ORGANIZATION = "IN_ORGANIZATION",
    REL_IN_REPOSITORY = "IN_REPOSITORY",
    REL_COMMENT_ON = "COMMENT_ON",
    REL_REVIEW_OF = "REVIEW_OF",
    REL_RESULT_FOR = "RESULT_FOR",
    REL_ERROR_ON = "ERROR_ON",
);

pub const STATUS_PREFIX: &str = "status:";

const SUPPORTED_EVENTS: &str = "repository issues issue_comment pull_request pull_request_review";

const REPOSITORY_ACTIONS: &str = "created edited renamed archived unarchived privatized \
     publicized deleted transferred";
const ISSUE_ACTIONS: &str = "opened deleted transferred assigned closed demilestoned edited \
     field_added field_removed labeled locked milestoned pinned reopened typed unassigned \
     unlabeled unlocked unpinned untyped";
const ISSUE_COMMENT_ACTIONS: &str = "created edited deleted pinned unpinned";
const PULL_REQUEST_ACTIONS: &str = "opened assigned auto_merge_disabled auto_merge_enabled \
     closed converted_to_draft demilestoned dequeued edited enqueued labeled locked milestoned \
     ready_for_review reopened review_request_removed review_requested stacked synchronize \
     unassigned unlabeled unlocked";
const REVIEW_ACTIONS: &str = "submitted edited dismissed";

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
        _ => REVIEW_ACTIONS,
    };
    in_table(table, action)
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
    effective_from: u64,
}

impl<'a> Converter<'a> {
    pub fn new(source_id: &'a str, organization: &'a str, effective_from: u64) -> Self {
        Self {
            source_id,
            organization,
            effective_from,
        }
    }

    pub fn convert(
        &self,
        event_type: &str,
        payload: &Value,
    ) -> Result<Option<Vec<SourceChange>>, ConvertError> {
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
        let d = &Delivery {
            action,
            payload,
            org: &org_node_id,
        };
        let mut cs = Changes::new(self.source_id, self.effective_from);
        match event_type {
            "repository" => self.repository_event(&mut cs, d)?,
            "issues" => self.work_item_event(&mut cs, d, NODE_ISSUE)?,
            "pull_request" => self.work_item_event(&mut cs, d, NODE_PULL_REQUEST)?,
            "issue_comment" => self.comment_event(&mut cs, d)?,
            _ => self.review_event(&mut cs, d)?,
        }
        Ok(Some(cs.changes))
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
            "deleted" => delete_work_item(cs, &item_id, &repo_id, label),
            "opened" => insert_work_item(cs, item, repo, &item_id, &repo_id, label),
            "transferred" => {
                if owner_in_org(repo, self.organization) {
                    delete_work_item(cs, &item_id, &repo_id, label);
                }
                let (Some(new_item), Some(new_repo)) = (
                    payload.pointer("/changes/new_issue"),
                    payload.pointer("/changes/new_repository"),
                ) else {
                    return Ok(());
                };
                if owner_in_org(Some(new_repo), self.organization) {
                    let new_id = required_str(new_item, "/node_id")?;
                    let new_repo_id = required_str(new_repo, "/node_id")?;
                    insert_work_item(cs, new_item, Some(new_repo), &new_id, &new_repo_id, label);
                }
            }
            _ => {
                cs.node(Update, &item_id, label, work_item_props(item, repo, label));
                status_changes(cs, Update, item, repo, &item_id, label);
            }
        }
        Ok(())
    }
    fn comment_event(&self, cs: &mut Changes, d: &Delivery) -> Mapped {
        let (action, payload, org) = (d.action, d.payload, d.org);
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
        let repo = payload.get("repository");
        let classify_body =
            |body: &str| CommentNode::classify(body, &comment_id, org, plain, comment, repo);
        let current = classify_body(comment.get("body").and_then(Value::as_str).unwrap_or(""));
        match action {
            "created" => current.insert(cs, &comment_id, &parent),
            "deleted" => current.delete(cs, &comment_id, &parent),
            "pinned" | "unpinned" => current.update(cs),
            _ => match payload
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
                Some(prev) if prev.result_target != current.result_target => {
                    current.update(cs);
                    if let Some(old) = &prev.result_target {
                        cs.delete(&rel_id(REL_RESULT_FOR, &comment_id, old), REL_RESULT_FOR);
                    }
                    if let Some(new) = &current.result_target {
                        let id = rel_id(REL_RESULT_FOR, &comment_id, new);
                        cs.relation(Insert, REL_RESULT_FOR, &id, &comment_id, new);
                    }
                }
                Some(_) => {
                    current.update(cs);
                    if current.is_error {
                        let id = rel_id(REL_ERROR_ON, &comment_id, &parent);
                        cs.relation(Update, REL_ERROR_ON, &id, &current.element_id, &parent);
                    }
                }
            },
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

fn insert_work_item(
    cs: &mut Changes,
    item: &Value,
    repo: Option<&Value>,
    item_id: &str,
    repo_id: &str,
    label: &'static str,
) {
    cs.node(Insert, item_id, label, work_item_props(item, repo, label));
    let id = rel_id(REL_IN_REPOSITORY, item_id, repo_id);
    cs.relation(Insert, REL_IN_REPOSITORY, &id, item_id, repo_id);
    status_changes(cs, Insert, item, repo, item_id, label);
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
    result_target: Option<String>,
    is_error: bool,
}

impl CommentNode {
    fn classify(
        body: &str,
        comment_id: &str,
        org_node_id: &str,
        ordinary_label: &'static str,
        comment: &Value,
        repo: Option<&Value>,
    ) -> Self {
        let mut props = ElementPropertyMap::new();
        props.table(comment, AUTHOR_PROPS);
        props.copy("repositoryNameWithOwner", full_name(repo));
        let (element_id, label, result_target) = match classify(body) {
            Classification::Ordinary => {
                props.table(comment, COMMENT_PROPS);
                let (created, updated) = (comment.get("created_at"), comment.get("updated_at"));
                if let (Some(created), Some(updated)) = (created, updated) {
                    props.insert("isEdited", ElementValue::Bool(created != updated));
                }
                (comment_id.to_string(), ordinary_label, None)
            }
            Classification::Assignment(a) => {
                props.table(comment, PROVENANCE_PROPS);
                props.text("assignmentId", &a.assignment_id);
                props.text("agentProfile", &a.agent_profile);
                props.insert("priority", ElementValue::Integer(a.priority));
                props.text("taskType", a.task_type.as_str());
                props.insert("task", ElementValue::from(&serde_json::json!(a.task)));
                let id = assignment_element_id(org_node_id, &a.assignment_id);
                (id, NODE_WORKGRAPH_ASSIGNMENT, None)
            }
            Classification::Result(r) => {
                props.table(comment, PROVENANCE_PROPS);
                props.text("assignmentId", &r.assignment_id);
                props.text("taskType", r.task_type.as_str());
                props.text("outcome", r.outcome.as_str());
                props.text("summary", &r.summary);
                props.insert("result", ElementValue::from(&serde_json::json!(r.result)));
                let target = assignment_element_id(org_node_id, &r.assignment_id);
                (comment_id.to_string(), NODE_WORKGRAPH_RESULT, Some(target))
            }
            Classification::Invalid(error) => {
                props.table(comment, PROVENANCE_PROPS);
                props.text("errorKind", "invalid-workgraph-comment");
                props.text("errorCode", error.code);
                props.text("errorMessage", &error.message);
                props.text("sourceCommentBody", body);
                let id = comment_error_element_id(comment_id);
                (id, NODE_WORKGRAPH_ERROR, None)
            }
        };
        CommentNode {
            element_id,
            label,
            properties: props,
            result_target,
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
        if let Some(target) = &self.result_target {
            let id = rel_id(REL_RESULT_FOR, comment_id, target);
            cs.relation(Insert, REL_RESULT_FOR, &id, comment_id, target);
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
        if let Some(target) = &self.result_target {
            cs.delete(&rel_id(REL_RESULT_FOR, comment_id, target), REL_RESULT_FOR);
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

fn work_item_props(item: &Value, repo: Option<&Value>, label: &str) -> ElementPropertyMap {
    let is_issue = label == NODE_ISSUE;
    let mut props = ElementPropertyMap::new();
    props.table(item, WORK_ITEM_PROPS);
    props.table(item, AUTHOR_PROPS);
    let variant = if is_issue {
        ISSUE_ONLY_PROPS
    } else {
        PULL_REQUEST_ONLY_PROPS
    };
    props.table(item, variant);
    let body = item.get("body").and_then(Value::as_str).unwrap_or("");
    props.text(
        "bodyDigest",
        &format!("sha256:{}", hex::encode(Sha256::digest(body))),
    );
    props.copy("repositoryNameWithOwner", full_name(repo));
    if let Some(assignees) = names(item, "assignees", "login") {
        props.insert("assignees", strings(assignees.into_iter()));
    }
    if let Some(labels) = names(item, "labels", "name") {
        props.insert("labels", strings(labels.into_iter()));
        match derive_status(item) {
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
    props
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
}

impl<'a> Changes<'a> {
    fn new(source_id: &'a str, effective_from: u64) -> Self {
        Self {
            source_id,
            effective_from,
            changes: Vec::new(),
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
