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

//! Mapping from GitHub authoritative models to Drasi `SourceChange`.

use crate::graphql::{
    ActorRef, FetchedRoot, IssueCommentData, IssueData, ProjectData, ProjectItemContent,
    ProjectItemData, ProjectItemFieldValue, PullRequestData, PullRequestReviewCommentData,
    PullRequestReviewData, RepositoryData,
};
use crate::types::{RootSnapshot, SnapshotElement, WebhookLocator};
use anyhow::{anyhow, Result};
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

const NODE_REPOSITORY: &str = "GitHubRepository";
const NODE_ISSUE: &str = "GitHubIssue";
const NODE_PULL_REQUEST: &str = "GitHubPullRequest";
const NODE_ISSUE_COMMENT: &str = "GitHubIssueComment";
const NODE_PULL_REQUEST_REVIEW: &str = "GitHubPullRequestReview";
const NODE_PULL_REQUEST_REVIEW_COMMENT: &str = "GitHubPullRequestReviewComment";
const NODE_PROJECT: &str = "GitHubProject";
const NODE_PROJECT_ITEM: &str = "GitHubProjectItem";

const REL_IN_REPOSITORY: &str = "IN_REPOSITORY";
const REL_COMMENT_ON: &str = "COMMENT_ON";
const REL_REVIEW_OF: &str = "REVIEW_OF";
const REL_PART_OF_REVIEW: &str = "PART_OF_REVIEW";
const REL_IN_PROJECT: &str = "IN_PROJECT";
const REL_TRACKS: &str = "TRACKS";

/// Labels exported by this source.
pub fn node_labels() -> Vec<String> {
    vec![
        NODE_REPOSITORY,
        NODE_ISSUE,
        NODE_PULL_REQUEST,
        NODE_ISSUE_COMMENT,
        NODE_PULL_REQUEST_REVIEW,
        NODE_PULL_REQUEST_REVIEW_COMMENT,
        NODE_PROJECT,
        NODE_PROJECT_ITEM,
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

/// Relation labels exported by this source.
pub fn relation_labels() -> Vec<String> {
    vec![
        REL_IN_REPOSITORY,
        REL_COMMENT_ON,
        REL_REVIEW_OF,
        REL_PART_OF_REVIEW,
        REL_IN_PROJECT,
        REL_TRACKS,
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

/// Build change-set by diffing previous root snapshot with newly fetched root.
pub fn map_root_diff(
    source_id: &str,
    root: &FetchedRoot,
    previous: Option<&RootSnapshot>,
    effective_from: u64,
) -> Result<(Vec<SourceChange>, RootSnapshot)> {
    let next_snapshot = build_snapshot(root);
    let changes = diff_snapshots(source_id, previous, &next_snapshot, effective_from);
    Ok((changes, next_snapshot))
}

/// Build a deletion-only change-set from previously persisted snapshot.
pub fn map_root_delete_from_snapshot(
    source_id: &str,
    previous: Option<&RootSnapshot>,
    effective_from: u64,
) -> Vec<SourceChange> {
    let Some(previous) = previous else {
        return Vec::new();
    };
    let mut elements = previous.elements.values().collect::<Vec<_>>();
    elements.sort_by(|left, right| {
        left.id
            .cmp(&right.id)
            .then_with(|| left.element_type.cmp(&right.element_type))
    });
    elements
        .into_iter()
        .map(|e| SourceChange::Delete {
            metadata: element_metadata(source_id, &e.id, &e.labels, effective_from),
        })
        .collect()
}

pub fn map_webhook_object_delete(
    source_id: &str,
    locator: &WebhookLocator,
    effective_from: u64,
) -> Result<SourceChange> {
    if locator.action != "deleted" {
        return Err(anyhow!("webhook action is not deleted"));
    }
    let node_id = locator
        .node_id
        .as_deref()
        .filter(|node_id| !node_id.trim().is_empty())
        .ok_or_else(|| anyhow!("deleted webhook locator is missing node ID"))?;
    let label = locator
        .deleted_node_label()
        .ok_or_else(|| anyhow!("unsupported deleted event type '{}'", locator.event_type))?;

    Ok(SourceChange::Delete {
        metadata: element_metadata(source_id, node_id, &[label.to_string()], effective_from),
    })
}

fn diff_snapshots(
    source_id: &str,
    previous: Option<&RootSnapshot>,
    next: &RootSnapshot,
    effective_from: u64,
) -> Vec<SourceChange> {
    let mut changes = Vec::new();
    let empty_previous = RootSnapshot {
        root_id: String::new(),
        root_kind: String::new(),
        repository_full_name: None,
        committed_delivery_id: None,
        committed_sequence: None,
        elements: HashMap::new(),
    };
    let prev = previous.unwrap_or(&empty_previous);

    for (id, element) in &next.elements {
        match prev.elements.get(id) {
            None => {
                if let Some(insert) = snapshot_to_insert(source_id, element, effective_from) {
                    changes.push(insert);
                }
            }
            Some(prev_element) if prev_element != element => {
                if let Some(update) = snapshot_to_update(source_id, element, effective_from) {
                    changes.push(update);
                }
            }
            _ => {}
        }
    }

    for (id, element) in &prev.elements {
        if !next.elements.contains_key(id) {
            changes.push(SourceChange::Delete {
                metadata: element_metadata(source_id, id, &element.labels, effective_from),
            });
        }
    }

    changes
}

fn snapshot_to_insert(
    source_id: &str,
    element: &SnapshotElement,
    effective_from: u64,
) -> Option<SourceChange> {
    snapshot_to_element(source_id, element, effective_from)
        .map(|el| SourceChange::Insert { element: el })
}

fn snapshot_to_update(
    source_id: &str,
    element: &SnapshotElement,
    effective_from: u64,
) -> Option<SourceChange> {
    snapshot_to_element(source_id, element, effective_from)
        .map(|el| SourceChange::Update { element: el })
}

fn snapshot_to_element(
    source_id: &str,
    element: &SnapshotElement,
    effective_from: u64,
) -> Option<Element> {
    let metadata = element_metadata(source_id, &element.id, &element.labels, effective_from);
    let properties = props_from_json(&element.properties);

    match element.element_type.as_str() {
        "node" => Some(Element::Node {
            metadata,
            properties,
        }),
        "relation" => {
            let in_node = element.in_node_id.as_ref()?;
            let out_node = element.out_node_id.as_ref()?;
            Some(Element::Relation {
                metadata,
                in_node: ElementReference::new(source_id, in_node),
                out_node: ElementReference::new(source_id, out_node),
                properties,
            })
        }
        _ => None,
    }
}

fn build_snapshot(root: &FetchedRoot) -> RootSnapshot {
    let mut elements = HashMap::new();
    match root {
        FetchedRoot::Repository(repository) => build_repository_snapshot(repository, &mut elements),
        FetchedRoot::Issue(issue) => build_issue_snapshot(issue, &mut elements),
        FetchedRoot::PullRequest(pr) => build_pull_request_snapshot(pr, &mut elements),
        FetchedRoot::IssueComment(comment) => build_issue_comment_snapshot(comment, &mut elements),
        FetchedRoot::PullRequestReview(review) => build_review_snapshot(review, &mut elements),
        FetchedRoot::PullRequestReviewComment(comment) => {
            build_review_comment_snapshot(comment, &mut elements)
        }
        FetchedRoot::Project(project) => build_project_snapshot(project, &mut elements),
        FetchedRoot::ProjectItem(item) => build_project_item_snapshot(item, &mut elements),
    };

    RootSnapshot {
        root_id: root.root_id().to_string(),
        root_kind: root.root_kind().to_string(),
        repository_full_name: root.repository_full_name().map(str::to_string),
        committed_delivery_id: None,
        committed_sequence: None,
        elements,
    }
}

fn build_repository_snapshot(
    repository: &RepositoryData,
    elements: &mut HashMap<String, SnapshotElement>,
) {
    upsert_node(
        elements,
        &repository.id,
        vec![NODE_REPOSITORY.to_string()],
        json_object(&[
            (
                "nameWithOwner",
                serde_json::json!(repository.name_with_owner),
            ),
            ("name", serde_json::json!(repository.name)),
            ("owner", serde_json::json!(repository.owner.login)),
            ("description", serde_json::json!(repository.description)),
            ("url", serde_json::json!(repository.url)),
            ("isArchived", serde_json::json!(repository.is_archived)),
            ("isPrivate", serde_json::json!(repository.is_private)),
            ("createdAt", serde_json::json!(repository.created_at)),
            ("updatedAt", serde_json::json!(repository.updated_at)),
            (
                "defaultBranch",
                serde_json::json!(repository
                    .default_branch_ref
                    .as_ref()
                    .map(|b| b.name.as_str())
                    .unwrap_or_default()),
            ),
        ]),
    );
}

fn build_issue_snapshot(issue: &IssueData, elements: &mut HashMap<String, SnapshotElement>) {
    upsert_node(
        elements,
        &issue.id,
        vec![NODE_ISSUE.to_string()],
        json_object(&[
            ("number", serde_json::json!(issue.number)),
            ("title", serde_json::json!(issue.title)),
            ("body", serde_json::json!(issue.body)),
            ("bodyDigest", serde_json::json!(body_digest(&issue.body))),
            ("state", serde_json::json!(issue.state)),
            ("createdAt", serde_json::json!(issue.created_at)),
            ("updatedAt", serde_json::json!(issue.updated_at)),
            ("closedAt", serde_json::json!(issue.closed_at)),
            (
                "authorLogin",
                serde_json::json!(issue.author.as_ref().map(|a| a.login.clone())),
            ),
            ("url", serde_json::json!(issue.url)),
            (
                "repositoryNameWithOwner",
                serde_json::json!(issue.repository.name_with_owner),
            ),
            (
                "isEdited",
                serde_json::json!(issue.updated_at != issue.created_at),
            ),
            (
                "assignees",
                serde_json::json!(issue
                    .assignees
                    .nodes
                    .iter()
                    .map(|a| a.login.clone())
                    .collect::<Vec<_>>()),
            ),
            (
                "labels",
                serde_json::json!(issue
                    .labels
                    .nodes
                    .iter()
                    .map(|l| l.name.clone())
                    .collect::<Vec<_>>()),
            ),
        ]),
    );

    upsert_relation(
        elements,
        REL_IN_REPOSITORY,
        &issue.id,
        &issue.repository.id,
        serde_json::Value::Object(serde_json::Map::new()),
    );

    for comment in &issue.comments.nodes {
        upsert_comment_snapshot_elements(comment, elements);
        upsert_relation(
            elements,
            REL_COMMENT_ON,
            &comment.id,
            &issue.id,
            serde_json::Value::Object(serde_json::Map::new()),
        );
    }
}

fn build_pull_request_snapshot(
    pr: &PullRequestData,
    elements: &mut HashMap<String, SnapshotElement>,
) {
    upsert_node(
        elements,
        &pr.id,
        vec![NODE_PULL_REQUEST.to_string()],
        json_object(&[
            ("number", serde_json::json!(pr.number)),
            ("title", serde_json::json!(pr.title)),
            ("body", serde_json::json!(pr.body)),
            ("bodyDigest", serde_json::json!(body_digest(&pr.body))),
            ("state", serde_json::json!(pr.state)),
            ("createdAt", serde_json::json!(pr.created_at)),
            ("updatedAt", serde_json::json!(pr.updated_at)),
            ("closedAt", serde_json::json!(pr.closed_at)),
            ("mergedAt", serde_json::json!(pr.merged_at)),
            (
                "authorLogin",
                serde_json::json!(pr.author.as_ref().map(|a| a.login.clone())),
            ),
            ("url", serde_json::json!(pr.url)),
            (
                "repositoryNameWithOwner",
                serde_json::json!(pr.repository.name_with_owner),
            ),
            ("isDraft", serde_json::json!(pr.is_draft)),
            (
                "isEdited",
                serde_json::json!(pr.updated_at != pr.created_at),
            ),
            ("headRefName", serde_json::json!(pr.head_ref_name)),
            ("baseRefName", serde_json::json!(pr.base_ref_name)),
            (
                "assignees",
                serde_json::json!(pr
                    .assignees
                    .nodes
                    .iter()
                    .map(|a| a.login.clone())
                    .collect::<Vec<_>>()),
            ),
            (
                "labels",
                serde_json::json!(pr
                    .labels
                    .nodes
                    .iter()
                    .map(|l| l.name.clone())
                    .collect::<Vec<_>>()),
            ),
        ]),
    );

    upsert_relation(
        elements,
        REL_IN_REPOSITORY,
        &pr.id,
        &pr.repository.id,
        serde_json::Value::Object(serde_json::Map::new()),
    );

    for comment in &pr.comments.nodes {
        upsert_comment_snapshot_elements(comment, elements);
        upsert_relation(
            elements,
            REL_COMMENT_ON,
            &comment.id,
            &pr.id,
            serde_json::Value::Object(serde_json::Map::new()),
        );
    }

    for review in &pr.reviews.nodes {
        upsert_review_snapshot_elements(review, elements);
        upsert_relation(
            elements,
            REL_REVIEW_OF,
            &review.id,
            &pr.id,
            serde_json::Value::Object(serde_json::Map::new()),
        );

        for review_comment in &review.comments.nodes {
            upsert_review_comment_snapshot_elements(review_comment, elements);
            upsert_relation(
                elements,
                REL_PART_OF_REVIEW,
                &review_comment.id,
                &review.id,
                serde_json::Value::Object(serde_json::Map::new()),
            );
        }
    }
}

fn build_issue_comment_snapshot(
    comment: &IssueCommentData,
    elements: &mut HashMap<String, SnapshotElement>,
) {
    upsert_comment_snapshot_elements(comment, elements);
    if let Some(issue) = &comment.issue {
        upsert_relation(
            elements,
            REL_COMMENT_ON,
            &comment.id,
            &issue.id,
            serde_json::Value::Object(serde_json::Map::new()),
        );
    }
    if let Some(pr) = &comment.pull_request {
        upsert_relation(
            elements,
            REL_COMMENT_ON,
            &comment.id,
            &pr.id,
            serde_json::Value::Object(serde_json::Map::new()),
        );
    }
}

fn build_review_snapshot(
    review: &PullRequestReviewData,
    elements: &mut HashMap<String, SnapshotElement>,
) {
    upsert_review_snapshot_elements(review, elements);
    upsert_relation(
        elements,
        REL_REVIEW_OF,
        &review.id,
        &review.pull_request.id,
        serde_json::Value::Object(serde_json::Map::new()),
    );
    for comment in &review.comments.nodes {
        upsert_review_comment_snapshot_elements(comment, elements);
        upsert_relation(
            elements,
            REL_PART_OF_REVIEW,
            &comment.id,
            &review.id,
            serde_json::Value::Object(serde_json::Map::new()),
        );
    }
}

fn build_review_comment_snapshot(
    comment: &PullRequestReviewCommentData,
    elements: &mut HashMap<String, SnapshotElement>,
) {
    upsert_review_comment_snapshot_elements(comment, elements);
    upsert_relation(
        elements,
        REL_PART_OF_REVIEW,
        &comment.id,
        &comment.pull_request_review.id,
        serde_json::Value::Object(serde_json::Map::new()),
    );
}

fn build_project_snapshot(project: &ProjectData, elements: &mut HashMap<String, SnapshotElement>) {
    upsert_node(
        elements,
        &project.id,
        vec![NODE_PROJECT.to_string()],
        json_object(&[
            ("title", serde_json::json!(project.title)),
            ("number", serde_json::json!(project.number)),
            ("url", serde_json::json!(project.url)),
            ("createdAt", serde_json::json!(project.created_at)),
            ("updatedAt", serde_json::json!(project.updated_at)),
            ("owner", serde_json::json!(project.owner.login)),
        ]),
    );

    for item in &project.items.nodes {
        upsert_project_item_snapshot_elements(item, elements);
        upsert_relation(
            elements,
            REL_IN_PROJECT,
            &item.id,
            &project.id,
            serde_json::Value::Object(serde_json::Map::new()),
        );
        upsert_project_item_tracks_relation(item, elements);
    }
}

fn build_project_item_snapshot(
    item: &ProjectItemData,
    elements: &mut HashMap<String, SnapshotElement>,
) {
    upsert_project_item_snapshot_elements(item, elements);
    upsert_relation(
        elements,
        REL_IN_PROJECT,
        &item.id,
        &item.project.id,
        serde_json::Value::Object(serde_json::Map::new()),
    );
    upsert_project_item_tracks_relation(item, elements);
}

fn upsert_project_item_tracks_relation(
    item: &ProjectItemData,
    elements: &mut HashMap<String, SnapshotElement>,
) {
    let Some(content) = item.content.as_ref() else {
        return;
    };

    let content_id = match content {
        ProjectItemContent::Issue { id, .. } | ProjectItemContent::PullRequest { id, .. } => id,
        ProjectItemContent::DraftIssue { .. } => return,
    };

    upsert_relation(
        elements,
        REL_TRACKS,
        &item.id,
        content_id,
        serde_json::Value::Object(serde_json::Map::new()),
    );
}

fn upsert_comment_snapshot_elements(
    comment: &IssueCommentData,
    elements: &mut HashMap<String, SnapshotElement>,
) {
    upsert_node(
        elements,
        &comment.id,
        vec![NODE_ISSUE_COMMENT.to_string()],
        json_object(&[
            ("body", serde_json::json!(comment.body)),
            ("createdAt", serde_json::json!(comment.created_at)),
            ("updatedAt", serde_json::json!(comment.updated_at)),
            (
                "authorLogin",
                serde_json::json!(actor_login(comment.author.as_ref())),
            ),
            (
                "authorId",
                serde_json::json!(actor_id(comment.author.as_ref())),
            ),
            (
                "authorDatabaseId",
                serde_json::json!(actor_database_id(comment.author.as_ref())),
            ),
            (
                "authorType",
                serde_json::json!(actor_type(comment.author.as_ref())),
            ),
            ("url", serde_json::json!(comment.url)),
            (
                "isEdited",
                serde_json::json!(comment.updated_at != comment.created_at),
            ),
            ("isMinimized", serde_json::json!(comment.is_minimized)),
            (
                "repositoryNameWithOwner",
                serde_json::json!(comment.repository.name_with_owner),
            ),
        ]),
    );
}

fn upsert_review_snapshot_elements(
    review: &PullRequestReviewData,
    elements: &mut HashMap<String, SnapshotElement>,
) {
    upsert_node(
        elements,
        &review.id,
        vec![NODE_PULL_REQUEST_REVIEW.to_string()],
        json_object(&[
            ("state", serde_json::json!(review.state)),
            ("body", serde_json::json!(review.body)),
            ("createdAt", serde_json::json!(review.created_at)),
            ("updatedAt", serde_json::json!(review.updated_at)),
            (
                "authorLogin",
                serde_json::json!(actor_login(review.author.as_ref())),
            ),
            (
                "authorId",
                serde_json::json!(actor_id(review.author.as_ref())),
            ),
            (
                "authorDatabaseId",
                serde_json::json!(actor_database_id(review.author.as_ref())),
            ),
            (
                "authorType",
                serde_json::json!(actor_type(review.author.as_ref())),
            ),
            ("url", serde_json::json!(review.url)),
        ]),
    );
}

fn upsert_review_comment_snapshot_elements(
    comment: &PullRequestReviewCommentData,
    elements: &mut HashMap<String, SnapshotElement>,
) {
    upsert_node(
        elements,
        &comment.id,
        vec![NODE_PULL_REQUEST_REVIEW_COMMENT.to_string()],
        json_object(&[
            ("body", serde_json::json!(comment.body)),
            ("path", serde_json::json!(comment.path)),
            ("position", serde_json::json!(comment.position)),
            ("line", serde_json::json!(comment.line)),
            ("createdAt", serde_json::json!(comment.created_at)),
            ("updatedAt", serde_json::json!(comment.updated_at)),
            (
                "authorLogin",
                serde_json::json!(actor_login(comment.author.as_ref())),
            ),
            (
                "authorId",
                serde_json::json!(actor_id(comment.author.as_ref())),
            ),
            (
                "authorDatabaseId",
                serde_json::json!(actor_database_id(comment.author.as_ref())),
            ),
            (
                "authorType",
                serde_json::json!(actor_type(comment.author.as_ref())),
            ),
            ("url", serde_json::json!(comment.url)),
            (
                "isEdited",
                serde_json::json!(comment.updated_at != comment.created_at),
            ),
            ("diffHunk", serde_json::json!(comment.diff_hunk)),
            (
                "repositoryNameWithOwner",
                serde_json::json!(comment.repository.name_with_owner),
            ),
        ]),
    );
}

fn upsert_project_item_snapshot_elements(
    item: &ProjectItemData,
    elements: &mut HashMap<String, SnapshotElement>,
) {
    let mut values = serde_json::Map::new();
    values.insert("type".to_string(), serde_json::json!(item.item_type));
    values.insert("createdAt".to_string(), serde_json::json!(item.created_at));
    values.insert("updatedAt".to_string(), serde_json::json!(item.updated_at));

    let mut status_field_id = serde_json::Value::Null;
    let mut status_option_id = serde_json::Value::Null;
    let mut status_name = serde_json::Value::Null;

    for field in &item.field_values.nodes {
        match field {
            ProjectItemFieldValue::ProjectV2ItemFieldSingleSelectValue {
                name,
                field,
                option_id,
            } => {
                if let Some(field_ref) = field {
                    let key = format!("field_{}", normalize_field_name(&field_ref.name));
                    values.insert(key, serde_json::json!(name.clone().unwrap_or_default()));
                    if field_ref.name.eq_ignore_ascii_case("status") {
                        status_field_id = serde_json::json!(field_ref.id);
                        status_option_id = serde_json::json!(option_id);
                        status_name = serde_json::json!(name);
                    }
                }
            }
            ProjectItemFieldValue::ProjectV2ItemFieldTextValue { text, field } => {
                if let Some(field_ref) = field {
                    let key = format!("field_{}", normalize_field_name(&field_ref.name));
                    values.insert(key, serde_json::json!(text));
                }
            }
            ProjectItemFieldValue::Unknown => {}
        }
    }

    values.insert("statusFieldId".to_string(), status_field_id);
    values.insert("statusOptionId".to_string(), status_option_id);
    values.insert("statusName".to_string(), status_name);

    if let Some(content) = &item.content {
        match content {
            ProjectItemContent::Issue {
                id,
                number,
                title,
                state,
                repository,
            } => {
                values.insert("contentType".to_string(), serde_json::json!("ISSUE"));
                values.insert("contentId".to_string(), serde_json::json!(id));
                values.insert("contentNumber".to_string(), serde_json::json!(number));
                values.insert("contentTitle".to_string(), serde_json::json!(title));
                values.insert("contentState".to_string(), serde_json::json!(state));
                values.insert(
                    "repositoryNameWithOwner".to_string(),
                    serde_json::json!(repository.name_with_owner),
                );
            }
            ProjectItemContent::PullRequest {
                id,
                number,
                title,
                state,
                repository,
            } => {
                values.insert("contentType".to_string(), serde_json::json!("PULL_REQUEST"));
                values.insert("contentId".to_string(), serde_json::json!(id));
                values.insert("contentNumber".to_string(), serde_json::json!(number));
                values.insert("contentTitle".to_string(), serde_json::json!(title));
                values.insert("contentState".to_string(), serde_json::json!(state));
                values.insert(
                    "repositoryNameWithOwner".to_string(),
                    serde_json::json!(repository.name_with_owner),
                );
            }
            ProjectItemContent::DraftIssue { id, title, body } => {
                values.insert("contentType".to_string(), serde_json::json!("DRAFT_ISSUE"));
                values.insert("contentId".to_string(), serde_json::json!(id));
                values.insert("contentTitle".to_string(), serde_json::json!(title));
                values.insert("contentBody".to_string(), serde_json::json!(body));
            }
        }
    }

    upsert_node(
        elements,
        &item.id,
        vec![NODE_PROJECT_ITEM.to_string()],
        serde_json::Value::Object(values),
    );
}

fn upsert_node(
    elements: &mut HashMap<String, SnapshotElement>,
    id: &str,
    labels: Vec<String>,
    properties: serde_json::Value,
) {
    elements.insert(
        id.to_string(),
        SnapshotElement {
            element_type: "node".to_string(),
            id: id.to_string(),
            labels,
            properties,
            in_node_id: None,
            out_node_id: None,
        },
    );
}

fn upsert_relation(
    elements: &mut HashMap<String, SnapshotElement>,
    relation_label: &str,
    out_node_id: &str,
    in_node_id: &str,
    properties: serde_json::Value,
) {
    let id = format!("{relation_label}:{out_node_id}:{in_node_id}");
    elements.insert(
        id.clone(),
        SnapshotElement {
            element_type: "relation".to_string(),
            id,
            labels: vec![relation_label.to_string()],
            properties,
            in_node_id: Some(in_node_id.to_string()),
            out_node_id: Some(out_node_id.to_string()),
        },
    );
}

fn actor_login(actor: Option<&ActorRef>) -> Option<String> {
    actor.and_then(|a| a.login.clone())
}

fn actor_id(actor: Option<&ActorRef>) -> Option<String> {
    actor.and_then(|a| a.id.clone())
}

fn actor_database_id(actor: Option<&ActorRef>) -> Option<i64> {
    actor.and_then(|a| a.database_id)
}

fn actor_type(actor: Option<&ActorRef>) -> Option<String> {
    actor.and_then(|a| a.actor_type.clone())
}

fn body_digest(body: &Option<String>) -> String {
    let digest = Sha256::digest(body.as_deref().unwrap_or("").as_bytes());
    format!("sha256:{}", hex::encode(digest))
}

fn json_object(entries: &[(&str, serde_json::Value)]) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for (k, v) in entries {
        map.insert((*k).to_string(), v.clone());
    }
    serde_json::Value::Object(map)
}

fn normalize_field_name(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_matches('_')
        .to_string()
}

fn props_from_json(value: &serde_json::Value) -> ElementPropertyMap {
    let mut props = ElementPropertyMap::new();
    let serde_json::Value::Object(map) = value else {
        return props;
    };
    for (k, v) in map {
        props.insert(k, ElementValue::from(v));
    }
    props
}

fn element_metadata(
    source_id: &str,
    element_id: &str,
    labels: &[String],
    effective_from: u64,
) -> ElementMetadata {
    ElementMetadata {
        reference: ElementReference::new(source_id, element_id),
        labels: labels
            .iter()
            .map(|l| Arc::<str>::from(l.as_str()))
            .collect::<Vec<_>>()
            .into(),
        effective_from,
    }
}

/// Return repositories inferred from project item content.
pub fn repositories_from_project_items(items: &[ProjectItemData]) -> HashSet<String> {
    let mut repos = HashSet::new();
    for item in items {
        let Some(content) = &item.content else {
            continue;
        };
        match content {
            ProjectItemContent::Issue { repository, .. }
            | ProjectItemContent::PullRequest { repository, .. } => {
                repos.insert(repository.name_with_owner.to_ascii_lowercase());
            }
            ProjectItemContent::DraftIssue { .. } => {}
        }
    }
    repos
}
