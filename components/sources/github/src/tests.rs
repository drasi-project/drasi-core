use crate::config::GitHubSourceConfig;
use crate::descriptor::GitHubSourceDescriptor;
use crate::graphql::{
    ActorRef, Connection, FetchedRoot, IssueCommentData, IssueData, NodeIdRef, OwnerRef, PageInfo,
    ProjectFieldRef, ProjectIdentityRef, ProjectItemContent, ProjectItemData,
    ProjectItemFieldValue, PullRequestData, RepositoryRef,
};
use crate::mapping::{map_root_diff, map_webhook_object_delete, node_labels, relation_labels};
use crate::webhook::{parse_locator, verify_signature};
use drasi_core::models::ElementValue;
use drasi_plugin_sdk::prelude::SourcePluginDescriptor;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::HashMap;

fn page_info() -> PageInfo {
    PageInfo {
        has_next_page: false,
        end_cursor: None,
    }
}

fn sha256_text(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    format!("sha256:{}", hex::encode(hasher.finalize()))
}

fn empty_connection<T>() -> Connection<T> {
    Connection {
        nodes: Vec::new(),
        page_info: page_info(),
    }
}

fn sample_issue(title: &str) -> IssueData {
    IssueData {
        id: "I_1".to_string(),
        number: 1,
        title: title.to_string(),
        body: Some("issue body".to_string()),
        state: "OPEN".to_string(),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        updated_at: "2026-01-01T00:00:00Z".to_string(),
        closed_at: None,
        url: "https://github.com/acme/repo/issues/1".to_string(),
        author: Some(OwnerRef {
            login: "octocat".to_string(),
        }),
        repository: RepositoryRef {
            id: "R_1".to_string(),
            name_with_owner: "acme/repo".to_string(),
        },
        assignees: empty_connection(),
        labels: empty_connection(),
        comments: empty_connection(),
    }
}

fn sample_pull_request(body: Option<&str>) -> PullRequestData {
    PullRequestData {
        id: "PR_1".to_string(),
        number: 7,
        title: "Pull request".to_string(),
        body: body.map(str::to_string),
        state: "OPEN".to_string(),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        updated_at: "2026-01-01T00:00:00Z".to_string(),
        closed_at: None,
        merged_at: None,
        url: "https://github.com/acme/repo/pull/7".to_string(),
        is_draft: false,
        head_ref_name: Some("feature".to_string()),
        base_ref_name: Some("main".to_string()),
        author: Some(OwnerRef {
            login: "octocat".to_string(),
        }),
        repository: RepositoryRef {
            id: "R_1".to_string(),
            name_with_owner: "acme/repo".to_string(),
        },
        assignees: empty_connection(),
        labels: empty_connection(),
        comments: empty_connection(),
        reviews: empty_connection(),
    }
}

fn sample_issue_comment() -> IssueCommentData {
    IssueCommentData {
        id: "IC_1".to_string(),
        body: Some("comment".to_string()),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        updated_at: "2026-01-01T00:00:00Z".to_string(),
        url: "https://github.com/acme/repo/issues/1#issuecomment-1".to_string(),
        is_minimized: false,
        author: Some(ActorRef {
            id: Some("U_NODE_1".to_string()),
            login: Some("octocat".to_string()),
            actor_type: Some("User".to_string()),
            database_id: Some(42),
        }),
        issue: Some(NodeIdRef {
            id: "I_1".to_string(),
        }),
        pull_request: None,
        repository: RepositoryRef {
            id: "R_1".to_string(),
            name_with_owner: "acme/repo".to_string(),
        },
    }
}

fn sample_project_item(status: &str) -> ProjectItemData {
    ProjectItemData {
        id: "PVTI_1".to_string(),
        item_type: "ISSUE".to_string(),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        updated_at: "2026-01-01T00:00:00Z".to_string(),
        project: ProjectIdentityRef {
            id: "PVT_1".to_string(),
            number: 1,
            owner: OwnerRef {
                login: "acme".to_string(),
            },
        },
        content: Some(ProjectItemContent::Issue {
            id: "I_1".to_string(),
            number: 1,
            title: "Issue".to_string(),
            state: "OPEN".to_string(),
            repository: RepositoryRef {
                id: "R_1".to_string(),
                name_with_owner: "acme/repo".to_string(),
            },
        }),
        field_values: Connection {
            nodes: vec![ProjectItemFieldValue::ProjectV2ItemFieldSingleSelectValue {
                name: Some(status.to_string()),
                field: Some(ProjectFieldRef {
                    id: "status".to_string(),
                    name: "Status".to_string(),
                }),
                option_id: Some("opt1".to_string()),
            }],
            page_info: page_info(),
        },
    }
}

#[test]
fn config_schema_rejects_removed_minimal_v1_fields() {
    let value = json!({
        "token": "x",
        "repositories": ["acme/repo"],
        "projects": [],
        "webhook": { "secret": "x" },
        "durability": { "enabled": true, "max_events": 1000, "capacity_policy": "RejectIncoming" },
        "graphqlUrl": "https://api.github.com/graphql",
        "reconcileIntervalSecs": 30,
        "skipInitialBootstrap": true
    });
    assert!(
        serde_json::from_value::<GitHubSourceConfig>(value).is_err(),
        "Removed fields must be rejected by deny_unknown_fields"
    );
}

#[tokio::test]
async fn descriptor_requires_secret_reference_for_token_and_webhook_secret() {
    let descriptor = GitHubSourceDescriptor;
    let cfg = json!({
        "token": "plain-token",
        "repositories": ["acme/repo"],
        "projects": [],
        "webhook": {
            "host": "127.0.0.1",
            "port": 8080,
            "path": "/webhook",
            "secret": "plain-secret",
            "bodyLimitBytes": 1024
        },
        "durability": { "enabled": true, "max_events": 1000, "capacity_policy": "RejectIncoming" },
        "graphqlUrl": "https://api.github.com/graphql"
    });

    let result = descriptor.create_source("github-test", &cfg, true).await;
    assert!(result.is_err(), "plain config should be rejected");
    let err = match result {
        Ok(_) => panic!("expected descriptor secret-reference validation failure"),
        Err(err) => err,
    };
    let message = format!("{err:#}");
    assert!(
        message.contains("SecretReference"),
        "expected SecretReference enforcement, got: {message}"
    );
}

#[test]
fn schema_contract_exports_exact_node_and_relation_label_counts() {
    assert_eq!(node_labels().len(), 8, "expected exactly 8 node labels");
    assert_eq!(
        relation_labels().len(),
        6,
        "expected exactly 6 relation labels"
    );
}

#[test]
fn mapping_contract_preserves_body_digest_status_and_author_fields() {
    let (issue_changes, _) = map_root_diff(
        "github-source",
        &FetchedRoot::Issue(sample_issue("Issue A")),
        None,
        1,
    )
    .expect("map issue");
    let issue_insert = issue_changes
        .into_iter()
        .find_map(|change| match change {
            drasi_core::models::SourceChange::Insert { element }
                if element.get_metadata().reference.element_id.as_ref() == "I_1" =>
            {
                Some(element)
            }
            _ => None,
        })
        .expect("issue insert");
    let issue_props = issue_insert.get_properties();
    assert_eq!(
        prop_string(issue_props, "bodyDigest"),
        Some(sha256_text("issue body"))
    );
    assert!(issue_props.get("performedViaGithubAppId").is_none());

    let (pr_changes, _) = map_root_diff(
        "github-source",
        &FetchedRoot::PullRequest(sample_pull_request(Some("pr body"))),
        None,
        1,
    )
    .expect("map pr");
    let pr_insert = pr_changes
        .into_iter()
        .find_map(|change| match change {
            drasi_core::models::SourceChange::Insert { element }
                if element.get_metadata().reference.element_id.as_ref() == "PR_1" =>
            {
                Some(element)
            }
            _ => None,
        })
        .expect("pr insert");
    let pr_props = pr_insert.get_properties();
    assert_eq!(
        prop_string(pr_props, "bodyDigest"),
        Some(sha256_text("pr body"))
    );

    let (comment_changes, _) = map_root_diff(
        "github-source",
        &FetchedRoot::IssueComment(sample_issue_comment()),
        None,
        1,
    )
    .expect("map issue comment");
    let comment_insert = comment_changes
        .into_iter()
        .find_map(|change| match change {
            drasi_core::models::SourceChange::Insert { element }
                if element.get_metadata().reference.element_id.as_ref() == "IC_1" =>
            {
                Some(element)
            }
            _ => None,
        })
        .expect("comment insert");
    let comment_props = comment_insert.get_properties();
    assert_eq!(
        prop_string(comment_props, "authorLogin").as_deref(),
        Some("octocat")
    );
    assert_eq!(
        prop_string(comment_props, "authorId").as_deref(),
        Some("U_NODE_1")
    );
    assert_eq!(prop_int(comment_props, "authorDatabaseId"), Some(42));
    assert_eq!(
        prop_string(comment_props, "authorType").as_deref(),
        Some("User")
    );
    assert!(comment_props.get("performedViaGithubAppId").is_none());

    let (project_item_changes, _) = map_root_diff(
        "github-source",
        &FetchedRoot::ProjectItem(sample_project_item("In Progress")),
        None,
        1,
    )
    .expect("map project item");
    let project_item_insert = project_item_changes
        .into_iter()
        .find_map(|change| match change {
            drasi_core::models::SourceChange::Insert { element }
                if element.get_metadata().reference.element_id.as_ref() == "PVTI_1" =>
            {
                Some(element)
            }
            _ => None,
        })
        .expect("project item insert");
    let project_item_props = project_item_insert.get_properties();
    assert_eq!(
        prop_string(project_item_props, "statusName").as_deref(),
        Some("In Progress")
    );
    assert_eq!(
        prop_string(project_item_props, "statusFieldId").as_deref(),
        Some("status")
    );
    assert_eq!(
        prop_string(project_item_props, "statusOptionId").as_deref(),
        Some("opt1")
    );
}

#[test]
fn body_digest_matches_shared_exact_utf8_vector_for_issues_and_pull_requests() {
    const BODY: &str = "Context\nWorkGraph-Validation: pass\n";
    const DIGEST: &str = "sha256:9faac769ff6962c7f331881d97518ff6a9df338da679c5d4851577cb7404a7fa";

    let mut issue = sample_issue("Digest issue");
    issue.body = Some(BODY.to_string());
    let mut pull_request = sample_pull_request(Some(BODY));

    for root in [
        FetchedRoot::Issue(issue),
        FetchedRoot::PullRequest(pull_request.clone()),
    ] {
        let root_id = root.root_id().to_string();
        let (changes, _) = map_root_diff("github-source", &root, None, 1).expect("map root");
        let element = changes
            .into_iter()
            .find_map(|change| match change {
                drasi_core::models::SourceChange::Insert { element }
                    if element.get_metadata().reference.element_id.as_ref() == root_id =>
                {
                    Some(element)
                }
                _ => None,
            })
            .expect("root insert");
        assert_eq!(
            prop_string(element.get_properties(), "body").as_deref(),
            Some(BODY)
        );
        assert_eq!(
            prop_string(element.get_properties(), "bodyDigest").as_deref(),
            Some(DIGEST)
        );
    }

    pull_request.body = None;
    let (changes, _) = map_root_diff(
        "github-source",
        &FetchedRoot::PullRequest(pull_request),
        None,
        1,
    )
    .expect("map null-body pull request");
    let element = changes
        .into_iter()
        .find_map(|change| match change {
            drasi_core::models::SourceChange::Insert { element }
                if element.get_metadata().reference.element_id.as_ref() == "PR_1" =>
            {
                Some(element)
            }
            _ => None,
        })
        .expect("pull request insert");
    assert_eq!(
        prop_string(element.get_properties(), "bodyDigest").as_deref(),
        Some("sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
    );
}

#[test]
fn webhook_hmac_and_locator_contract() {
    let body = serde_json::to_vec(&json!({
        "action": "edited",
        "issue": { "node_id": "I_1" },
        "repository": { "full_name": "Acme/Repo" }
    }))
    .expect("serialize payload");

    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(b"secret").expect("hmac");
    use hmac::Mac;
    mac.update(&body);
    let signature = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
    verify_signature(b"secret", &body, &signature).expect("valid signature");

    let locator = parse_locator("issues", &body).expect("parse locator");
    assert_eq!(locator.event_type, "issues");
    assert_eq!(locator.action, "edited");
    assert_eq!(locator.node_id.as_deref(), Some("I_1"));
    assert_eq!(locator.repository_full_name.as_deref(), Some("acme/repo"));
}

#[test]
fn deleted_webhook_requires_node_id_and_maps_one_object_delete() {
    let malformed = serde_json::to_vec(&json!({
        "action": "deleted",
        "issue": {},
        "repository": { "full_name": "acme/repo" }
    }))
    .expect("serialize malformed delete");
    assert!(
        parse_locator("issues", &malformed).is_err(),
        "deleted delivery without node ID must be rejected before admission"
    );

    let body = serde_json::to_vec(&json!({
        "action": "deleted",
        "issue": { "node_id": "I_DELETE" },
        "repository": { "full_name": "acme/repo" }
    }))
    .expect("serialize delete");
    let locator = parse_locator("issues", &body).expect("parse delete locator");
    let change =
        map_webhook_object_delete("github-source", &locator, 42).expect("map object delete");
    let drasi_core::models::SourceChange::Delete { metadata } = change else {
        panic!("expected exactly one delete change");
    };
    assert_eq!(metadata.reference.source_id.as_ref(), "github-source");
    assert_eq!(metadata.reference.element_id.as_ref(), "I_DELETE");
    assert_eq!(
        metadata
            .labels
            .iter()
            .map(|label| label.as_ref())
            .collect::<Vec<_>>(),
        vec!["GitHubIssue"]
    );
}

#[test]
fn mapping_delete_from_snapshot_is_deterministic() {
    let (_, snapshot) = map_root_diff(
        "github-source",
        &FetchedRoot::Issue(sample_issue("Before Delete")),
        None,
        42,
    )
    .expect("map issue snapshot");
    let deletes =
        crate::mapping::map_root_delete_from_snapshot("github-source", Some(&snapshot), 1000);
    let ids = deletes
        .into_iter()
        .filter_map(|change| match change {
            drasi_core::models::SourceChange::Delete { metadata } => {
                Some(metadata.reference.element_id.as_ref().to_string())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut sorted_ids = ids.clone();
    sorted_ids.sort();
    assert_eq!(ids, sorted_ids, "delete order must be stable by element ID");
    let replay_ids =
        crate::mapping::map_root_delete_from_snapshot("github-source", Some(&snapshot), 1000)
            .into_iter()
            .filter_map(|change| match change {
                drasi_core::models::SourceChange::Delete { metadata } => {
                    Some(metadata.reference.element_id.as_ref().to_string())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
    assert_eq!(ids, replay_ids, "crash replay must preserve delete order");
    assert!(!ids.is_empty(), "expected delete set from snapshot");
    assert!(ids.contains(&"I_1".to_string()));
}
fn prop_string(props: &drasi_core::models::ElementPropertyMap, key: &str) -> Option<String> {
    match props.get(key) {
        Some(ElementValue::String(v)) => Some(v.to_string()),
        _ => None,
    }
}

fn prop_int(props: &drasi_core::models::ElementPropertyMap, key: &str) -> Option<i64> {
    match props.get(key) {
        Some(ElementValue::Integer(v)) => Some(*v),
        _ => None,
    }
}
