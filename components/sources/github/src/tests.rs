use crate::config::{GitHubSourceConfig, ProjectSpec, WebhookConfig};
use crate::descriptor::{GitHubSourceConfigDto, GitHubSourceDescriptor};
use crate::graphql::{
    Connection, FetchedRoot, GitHubGraphQLClient, IssueCommentData, IssueData, LabelRef, NodeIdRef,
    OwnerRef, PageInfo, ProjectIdentityRef, ProjectItemContent, ProjectItemData,
    ProjectItemFieldValue, PullRequestData, PullRequestReviewData, PullRequestReviewRef,
    ReconcileSnapshot, RepositoryData, RepositoryRef, UserRef,
};
use crate::hydrator::{
    load_root_snapshot, process_admission, save_root_snapshot, snapshot_key_for_locator,
    HydratorParams,
};
use crate::mapping::{map_reconcile_snapshot, map_root_diff, node_labels, relation_labels};
use crate::rate_limit::{classify_retry, exp_backoff};
use crate::reconciler::{run_reconciler_loop, ReconcilerParams};
use crate::source::GitHubSourceBuilder;
use crate::types::{HydratorHealth, RootSnapshot, SnapshotElement, WebhookLocator};
use crate::webhook::{encode_admission_change, parse_locator, verify_signature};
use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::post;
use axum::{Json, Router};
use drasi_core::models::SourceChange;
use drasi_lib::channels::DispatchMode;
use drasi_lib::state_store::{
    MemoryStateStoreProvider, StateStoreError, StateStoreProvider, StateStoreResult,
};
use drasi_lib::wal::{CapacityPolicy, WalProvider};
use drasi_lib::{DrasiLib, DurabilityConfig, Source};
use drasi_plugin_sdk::{ConfigValue, SourcePluginDescriptor};
use drasi_state_store_redb::RedbStateStoreProvider;
use drasi_wal_redb::RedbWalProvider;
use hmac::Mac;
use reqwest::header::{HeaderMap as ReqwestHeaderMap, HeaderValue, AUTHORIZATION};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Notify, RwLock};

fn empty_page_info() -> PageInfo {
    PageInfo {
        has_next_page: false,
        end_cursor: None,
    }
}

fn single_connection<T>(nodes: Vec<T>) -> Connection<T> {
    Connection {
        nodes,
        page_info: empty_page_info(),
    }
}

fn sample_issue(title: &str) -> IssueData {
    IssueData {
        id: "I_1".to_string(),
        number: 42,
        title: title.to_string(),
        body: Some("body".to_string()),
        state: "OPEN".to_string(),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        updated_at: "2026-01-01T00:00:00Z".to_string(),
        closed_at: None,
        url: "https://github.com/acme/repo/issues/42".to_string(),
        author: Some(OwnerRef {
            login: "octocat".to_string(),
        }),
        repository: RepositoryRef {
            id: "R_1".to_string(),
            name_with_owner: "acme/repo".to_string(),
        },
        assignees: single_connection(vec![UserRef {
            id: "U_1".to_string(),
            login: "assignee".to_string(),
        }]),
        labels: single_connection(vec![LabelRef {
            id: "L_1".to_string(),
            name: "bug".to_string(),
        }]),
        comments: single_connection(vec![IssueCommentData {
            id: "IC_1".to_string(),
            body: Some("comment".to_string()),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            url: "https://example/comment".to_string(),
            is_minimized: false,
            author: None,
            issue: Some(NodeIdRef {
                id: "I_1".to_string(),
            }),
            pull_request: None,
            repository: RepositoryRef {
                id: "R_1".to_string(),
                name_with_owner: "acme/repo".to_string(),
            },
        }]),
    }
}

fn sample_pull_request(body: Option<String>) -> PullRequestData {
    PullRequestData {
        id: "PR_1".to_string(),
        number: 7,
        title: "Pull request".to_string(),
        body,
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
        assignees: single_connection(Vec::new()),
        labels: single_connection(Vec::new()),
        comments: single_connection(Vec::new()),
        reviews: single_connection(Vec::new()),
    }
}

fn sample_project_item() -> ProjectItemData {
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
            number: 42,
            title: "Issue".to_string(),
            state: "OPEN".to_string(),
            repository: RepositoryRef {
                id: "R_1".to_string(),
                name_with_owner: "acme/repo".to_string(),
            },
        }),
        field_values: single_connection(vec![
            ProjectItemFieldValue::ProjectV2ItemFieldSingleSelectValue {
                name: Some("In Progress".to_string()),
                field: Some(crate::graphql::ProjectFieldRef {
                    id: "status".to_string(),
                    name: "Status".to_string(),
                }),
                option_id: Some("opt1".to_string()),
            },
        ]),
    }
}

fn valid_config_with_port(port: u16) -> GitHubSourceConfig {
    GitHubSourceConfig {
        token: "test-token".to_string(),
        repositories: vec!["acme/repo".to_string()],
        projects: vec![ProjectSpec {
            owner: "acme".to_string(),
            number: 1,
        }],
        webhook: WebhookConfig {
            host: "127.0.0.1".to_string(),
            port,
            path: "/webhook".to_string(),
            secret: "secret".to_string(),
            body_limit_bytes: 1024 * 1024,
        },
        reconcile_interval_secs: 60,
        durability: DurabilityConfig {
            enabled: true,
            max_events: 16,
            capacity_policy: CapacityPolicy::RejectIncoming,
        },
        graphql_url: "http://127.0.0.1:9/graphql".to_string(),
        skip_initial_bootstrap: true,
    }
}

async fn find_available_port() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    listener.local_addr().expect("local addr").port()
}

fn test_source_base(id: &str) -> drasi_lib::sources::base::SourceBase {
    drasi_lib::sources::base::SourceBase::new(drasi_lib::sources::base::SourceBaseParams::new(id))
        .expect("create source base")
}

struct FaultyStateStoreProvider {
    inner: Arc<dyn StateStoreProvider>,
    fail_store: String,
    fail_key: String,
    fail_get: bool,
    fail_set: bool,
}

#[async_trait::async_trait]
impl StateStoreProvider for FaultyStateStoreProvider {
    async fn get(&self, store_id: &str, key: &str) -> StateStoreResult<Option<Vec<u8>>> {
        if self.fail_get && store_id == self.fail_store && key == self.fail_key {
            return Err(StateStoreError::StorageError(
                "injected effective-repos load failure".to_string(),
            ));
        }
        self.inner.get(store_id, key).await
    }

    async fn set(&self, store_id: &str, key: &str, value: Vec<u8>) -> StateStoreResult<()> {
        if self.fail_set && store_id == self.fail_store && key == self.fail_key {
            return Err(StateStoreError::StorageError(
                "injected reconcile-index commit failure".to_string(),
            ));
        }
        self.inner.set(store_id, key, value).await
    }

    async fn delete(&self, store_id: &str, key: &str) -> StateStoreResult<bool> {
        self.inner.delete(store_id, key).await
    }

    async fn contains_key(&self, store_id: &str, key: &str) -> StateStoreResult<bool> {
        self.inner.contains_key(store_id, key).await
    }

    async fn get_many(
        &self,
        store_id: &str,
        keys: &[&str],
    ) -> StateStoreResult<HashMap<String, Vec<u8>>> {
        self.inner.get_many(store_id, keys).await
    }

    async fn set_many(&self, store_id: &str, entries: &[(&str, &[u8])]) -> StateStoreResult<()> {
        self.inner.set_many(store_id, entries).await
    }

    async fn delete_many(&self, store_id: &str, keys: &[&str]) -> StateStoreResult<usize> {
        self.inner.delete_many(store_id, keys).await
    }

    async fn clear_store(&self, store_id: &str) -> StateStoreResult<usize> {
        self.inner.clear_store(store_id).await
    }

    async fn list_keys(&self, store_id: &str) -> StateStoreResult<Vec<String>> {
        self.inner.list_keys(store_id).await
    }

    async fn store_exists(&self, store_id: &str) -> StateStoreResult<bool> {
        self.inner.store_exists(store_id).await
    }

    async fn key_count(&self, store_id: &str) -> StateStoreResult<usize> {
        self.inner.key_count(store_id).await
    }

    async fn sync(&self) -> StateStoreResult<()> {
        self.inner.sync().await
    }

    fn is_durable(&self) -> bool {
        self.inner.is_durable()
    }
}

#[test]
fn signature_validation_accepts_valid_signature() {
    let secret = b"top-secret";
    let body = br#"{"action":"opened"}"#;
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(secret).expect("hmac init");
    mac.update(body);
    let signature = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));

    assert!(verify_signature(secret, body, &signature).is_ok());
}

#[test]
fn signature_validation_rejects_tampered_payload() {
    let secret = b"top-secret";
    let body = br#"{"action":"opened"}"#;
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(secret).expect("hmac init");
    mac.update(body);
    let signature = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));

    let tampered = br#"{"action":"edited"}"#;
    assert!(verify_signature(secret, tampered, &signature).is_err());
}

#[test]
fn signature_validation_rejects_malformed_header() {
    assert!(verify_signature(b"secret", b"{}", "abcdef").is_err());
}

#[test]
fn mapping_issue_produces_expected_nodes_and_relations() {
    let mut issue = sample_issue("initial title");
    issue.body = Some("Context\nWorkGraph-Validation: pass\n".to_string());
    let root = FetchedRoot::Issue(issue);
    let (changes, snapshot): (Vec<SourceChange>, RootSnapshot) =
        map_root_diff("github-src", &root, None, 1_000).expect("map");

    assert!(!changes.is_empty());
    assert!(snapshot.elements.contains_key("I_1"));
    assert!(snapshot.elements.contains_key("IN_REPOSITORY:I_1:R_1"));
    assert!(snapshot.elements.contains_key("COMMENT_ON:IC_1:I_1"));
    let properties = &snapshot.elements["I_1"].properties;
    assert_eq!(
        properties["body"],
        json!("Context\nWorkGraph-Validation: pass\n")
    );
    assert_eq!(
        properties["bodyDigest"],
        json!("sha256:9faac769ff6962c7f331881d97518ff6a9df338da679c5d4851577cb7404a7fa")
    );
}

#[test]
fn mapping_pull_request_preserves_body_and_adds_authoritative_digest() {
    let body = "Context\nWorkGraph-Validation: pass\n";
    let root = FetchedRoot::PullRequest(sample_pull_request(Some(body.to_string())));
    let (_, snapshot) = map_root_diff("github-src", &root, None, 1_000).expect("map");

    let properties = &snapshot.elements["PR_1"].properties;
    assert_eq!(properties["body"], json!(body));
    assert_eq!(
        properties["bodyDigest"],
        json!("sha256:9faac769ff6962c7f331881d97518ff6a9df338da679c5d4851577cb7404a7fa")
    );
}

#[test]
fn mapping_body_digest_hashes_missing_body_as_empty_string() {
    let root = FetchedRoot::PullRequest(sample_pull_request(None));
    let (_, snapshot) = map_root_diff("github-src", &root, None, 1_000).expect("map");

    let properties = &snapshot.elements["PR_1"].properties;
    assert_eq!(properties["body"], serde_json::Value::Null);
    assert_eq!(
        properties["bodyDigest"],
        json!("sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
    );
}

#[test]
fn mapping_project_item_emits_tracks_relation() {
    let item = sample_project_item();
    let root = FetchedRoot::ProjectItem(item);
    let (_, snapshot) = map_root_diff("github-src", &root, None, 1_000).expect("map");

    assert!(snapshot.elements.contains_key("IN_PROJECT:PVTI_1:PVT_1"));
    assert!(snapshot.elements.contains_key("TRACKS:PVTI_1:I_1"));
    assert!(!snapshot.elements.contains_key("HAS_ITEM:PVT_1:PVTI_1"));
    let properties = &snapshot.elements["PVTI_1"].properties;
    assert_eq!(properties["statusFieldId"], json!("status"));
    assert_eq!(properties["statusOptionId"], json!("opt1"));
    assert_eq!(properties["statusName"], json!("In Progress"));
}

#[test]
fn mapping_comment_review_shapes_include_author_fields() {
    let issue_comment = IssueCommentData {
        id: "IC_meta".to_string(),
        body: Some("comment".to_string()),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        updated_at: "2026-01-02T00:00:00Z".to_string(),
        url: "https://github.com/acme/repo/issues/1#issuecomment-1".to_string(),
        is_minimized: false,
        author: Some(crate::graphql::ActorRef {
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
    };
    let review = PullRequestReviewData {
        id: "RV_1".to_string(),
        state: "APPROVED".to_string(),
        body: Some("looks good".to_string()),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        updated_at: "2026-01-02T00:00:00Z".to_string(),
        url: "https://github.com/acme/repo/pull/1#review-1".to_string(),
        author: Some(crate::graphql::ActorRef {
            id: Some("U_NODE_2".to_string()),
            login: Some("reviewer".to_string()),
            actor_type: Some("Bot".to_string()),
            database_id: Some(77),
        }),
        pull_request: crate::graphql::PullRequestRef {
            id: "PR_1".to_string(),
            repository: RepositoryRef {
                id: "R_1".to_string(),
                name_with_owner: "acme/repo".to_string(),
            },
        },
        comments: single_connection(Vec::new()),
    };
    let review_comment = crate::graphql::PullRequestReviewCommentData {
        id: "RC_1".to_string(),
        body: Some("nit".to_string()),
        path: Some("src/lib.rs".to_string()),
        position: Some(1),
        line: Some(10),
        diff_hunk: Some("@@".to_string()),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        updated_at: "2026-01-02T00:00:00Z".to_string(),
        url: "https://github.com/acme/repo/pull/1#discussion_r1".to_string(),
        author: Some(crate::graphql::ActorRef {
            id: Some("U_NODE_3".to_string()),
            login: Some("reviewer2".to_string()),
            actor_type: Some("User".to_string()),
            database_id: Some(88),
        }),
        repository: RepositoryRef {
            id: "R_1".to_string(),
            name_with_owner: "acme/repo".to_string(),
        },
        pull_request_review: PullRequestReviewRef {
            id: "RV_1".to_string(),
            pull_request: crate::graphql::PullRequestRef {
                id: "PR_1".to_string(),
                repository: RepositoryRef {
                    id: "R_1".to_string(),
                    name_with_owner: "acme/repo".to_string(),
                },
            },
        },
    };

    let (_, comment_snapshot) = map_root_diff(
        "github-src",
        &FetchedRoot::IssueComment(issue_comment),
        None,
        1_000,
    )
    .expect("map issue comment");
    let (_, review_snapshot) = map_root_diff(
        "github-src",
        &FetchedRoot::PullRequestReview(review),
        None,
        1_000,
    )
    .expect("map review");
    let (_, review_comment_snapshot) = map_root_diff(
        "github-src",
        &FetchedRoot::PullRequestReviewComment(review_comment),
        None,
        1_000,
    )
    .expect("map review comment");

    let comment_props = &comment_snapshot.elements["IC_meta"].properties;
    assert_eq!(comment_props["authorId"], json!("U_NODE_1"));
    assert_eq!(comment_props["authorDatabaseId"], json!(42));
    assert_eq!(comment_props["authorType"], json!("User"));
    assert!(comment_props.get("performedViaGithubAppId").is_none());
    assert_eq!(comment_props["isEdited"], json!(true));

    let review_props = &review_snapshot.elements["RV_1"].properties;
    assert_eq!(review_props["authorId"], json!("U_NODE_2"));
    assert_eq!(review_props["authorDatabaseId"], json!(77));
    assert_eq!(review_props["authorType"], json!("Bot"));
    assert!(review_props.get("performedViaGithubAppId").is_none());

    let review_comment_props = &review_comment_snapshot.elements["RC_1"].properties;
    assert_eq!(review_comment_props["authorId"], json!("U_NODE_3"));
    assert_eq!(review_comment_props["authorDatabaseId"], json!(88));
    assert_eq!(review_comment_props["authorType"], json!("User"));
    assert!(review_comment_props
        .get("performedViaGithubAppId")
        .is_none());
    assert_eq!(review_comment_props["isEdited"], json!(true));
}

#[test]
fn relation_labels_match_contract() {
    let labels = relation_labels().into_iter().collect::<HashSet<_>>();
    let expected = HashSet::from([
        "IN_PROJECT".to_string(),
        "TRACKS".to_string(),
        "IN_REPOSITORY".to_string(),
        "COMMENT_ON".to_string(),
        "REVIEW_OF".to_string(),
        "PART_OF_REVIEW".to_string(),
    ]);
    assert_eq!(labels, expected);
}

#[test]
fn node_labels_match_contract() {
    let labels = node_labels().into_iter().collect::<HashSet<_>>();
    let expected = HashSet::from([
        "GitHubRepository".to_string(),
        "GitHubIssue".to_string(),
        "GitHubPullRequest".to_string(),
        "GitHubIssueComment".to_string(),
        "GitHubPullRequestReview".to_string(),
        "GitHubPullRequestReviewComment".to_string(),
        "GitHubProject".to_string(),
        "GitHubProjectItem".to_string(),
    ]);
    assert_eq!(labels, expected);
}

#[test]
fn mapping_update_emits_update_change() {
    let initial = FetchedRoot::Issue(sample_issue("initial"));
    let (_, snapshot) = map_root_diff("github-src", &initial, None, 1_000).expect("map initial");

    let updated = FetchedRoot::Issue(sample_issue("updated"));
    let (changes, _) =
        map_root_diff("github-src", &updated, Some(&snapshot), 2_000).expect("map update");

    assert!(changes.iter().any(|change| match change {
        SourceChange::Update { element } => element.get_reference().element_id.as_ref() == "I_1",
        _ => false,
    }));
}

#[test]
fn config_dto_deserialization_applies_defaults() {
    let config = serde_json::json!({
        "token": { "kind": "Secret", "name": "pat" },
        "webhook": {
            "secret": { "kind": "Secret", "name": "hook" }
        },
        "repositories": ["acme/repo"]
    });

    let dto: GitHubSourceConfigDto = serde_json::from_value(config).expect("dto");
    assert_eq!(dto.reconcile_interval_secs, ConfigValue::Static(300));
    match dto.token {
        ConfigValue::Secret { name } => assert_eq!(name, "pat"),
        _ => panic!("token must be secret"),
    }
}

#[test]
fn config_dto_accepts_exact_dogfood_shape() {
    let config = serde_json::json!({
        "token": { "kind": "Secret", "name": "github-pat" },
        "repositories": ["drasi-project/drasi-workgraph-demo"],
        "projects": [{ "owner": "drasi-project", "number": 3 }],
        "webhook": {
            "host": "${WEBHOOK_HOST:-127.0.0.1}",
            "port": "${WEBHOOK_PORT:-9000}",
            "path": "/github/events",
            "secret": { "kind": "Secret", "name": "github-webhook-secret" },
            "bodyLimitBytes": 10485760
        },
        "reconcileIntervalSecs": 300,
        "durability": {
            "enabled": true,
            "max_events": 10000,
            "capacity_policy": "RejectIncoming"
        },
        "graphqlUrl": "https://api.github.com/graphql",
        "skipInitialBootstrap": false
    });

    let dto: GitHubSourceConfigDto = serde_json::from_value(config).expect("dogfood DTO");
    assert_eq!(
        dto.repositories,
        vec![ConfigValue::Static(
            "drasi-project/drasi-workgraph-demo".to_string()
        )]
    );
    assert_eq!(
        dto.projects[0].owner,
        ConfigValue::Static("drasi-project".to_string())
    );
    assert_eq!(dto.projects[0].number, ConfigValue::Static(3));
    assert_eq!(
        dto.webhook.body_limit_bytes,
        ConfigValue::Static(10_485_760)
    );
    assert!(dto.durability.enabled);
    assert_eq!(dto.durability.max_events, 10_000);
    assert_eq!(
        dto.durability.capacity_policy,
        CapacityPolicy::RejectIncoming
    );
}

#[test]
fn config_dto_denies_unknown_fields() {
    let config = serde_json::json!({
        "token": { "kind": "Secret", "name": "pat" },
        "webhook": {
            "secret": { "kind": "Secret", "name": "hook" },
            "unknownField": true
        },
        "repositories": ["acme/repo"]
    });
    assert!(serde_json::from_value::<GitHubSourceConfigDto>(config).is_err());
}

#[test]
fn descriptor_schema_has_no_dangling_references() {
    fn check_refs(value: &serde_json::Value, schemas: &serde_json::Map<String, serde_json::Value>) {
        match value {
            serde_json::Value::Object(object) => {
                if let Some(reference) = object.get("$ref").and_then(|value| value.as_str()) {
                    let name = reference
                        .strip_prefix("#/components/schemas/")
                        .expect("schema references must target components/schemas");
                    assert!(
                        schemas.contains_key(name),
                        "schema reference {reference} is not registered"
                    );
                }
                for child in object.values() {
                    check_refs(child, schemas);
                }
            }
            serde_json::Value::Array(array) => {
                for child in array {
                    check_refs(child, schemas);
                }
            }
            _ => {}
        }
    }

    let schemas: serde_json::Value =
        serde_json::from_str(&GitHubSourceDescriptor.config_schema_json()).expect("schema JSON");
    let schemas = schemas.as_object().expect("schema map");
    assert!(schemas.contains_key("source.github.GitHubSourceConfig"));
    check_refs(&serde_json::Value::Object(schemas.clone()), schemas);
}

#[tokio::test]
async fn direct_builder_properties_and_configuration_snapshot_redact_secrets() {
    let pat = "literal-pat-must-not-leak";
    let webhook_secret = "literal-webhook-secret-must-not-leak";
    let mut config = valid_config_with_port(0);
    config.token = pat.to_string();
    config.webhook.secret = webhook_secret.to_string();
    let source = GitHubSourceBuilder::new("github-secret-test")
        .with_config(config)
        .with_auto_start(false)
        .build()
        .expect("build source");

    let properties_json = serde_json::to_string(&source.properties()).expect("properties JSON");
    assert!(!properties_json.contains(pat));
    assert!(!properties_json.contains(webhook_secret));
    assert!(!source.properties().contains_key("token"));
    assert!(source.properties()["webhook"].get("secret").is_none());

    let core = DrasiLib::builder()
        .with_id("github-secret-core")
        .with_source(source)
        .build()
        .await
        .expect("build core");
    let snapshot_json =
        serde_json::to_string(&core.snapshot_configuration().await.expect("snapshot"))
            .expect("snapshot JSON");
    assert!(!snapshot_json.contains(pat));
    assert!(!snapshot_json.contains(webhook_secret));
}

#[test]
fn dispatch_mode_reports_builder_configuration() {
    let source = GitHubSourceBuilder::new("github-broadcast")
        .with_config(valid_config_with_port(0))
        .with_dispatch_mode(DispatchMode::Broadcast)
        .build()
        .expect("build source");
    assert_eq!(source.dispatch_mode(), DispatchMode::Broadcast);
}

#[test]
fn config_rejects_overwrite_oldest_policy() {
    let mut config = valid_config_with_port(8080);
    config.durability.capacity_policy = CapacityPolicy::OverwriteOldest;
    assert!(config.validate().is_err());
}

#[test]
fn config_accepts_reject_incoming_policy() {
    let config = valid_config_with_port(8080);
    assert!(config.validate().is_ok());
}

#[test]
fn rate_limit_retry_after_header_is_honored() {
    let mut headers = ReqwestHeaderMap::new();
    headers.insert("retry-after", HeaderValue::from_static("3"));
    let decision = classify_retry(reqwest::StatusCode::TOO_MANY_REQUESTS, &headers, 0);
    assert!(decision.retryable);
    assert_eq!(decision.delay.as_secs(), 3);
}

#[test]
fn rate_limit_forbidden_retries_only_when_exhausted() {
    let mut exhausted_headers = ReqwestHeaderMap::new();
    exhausted_headers.insert("x-ratelimit-remaining", HeaderValue::from_static("0"));
    exhausted_headers.insert("retry-after", HeaderValue::from_static("1"));
    let exhausted = classify_retry(reqwest::StatusCode::FORBIDDEN, &exhausted_headers, 0);
    assert!(exhausted.retryable);
    assert_eq!(exhausted.delay.as_secs(), 1);

    let mut non_exhausted_headers = ReqwestHeaderMap::new();
    non_exhausted_headers.insert("x-ratelimit-remaining", HeaderValue::from_static("42"));
    let non_exhausted = classify_retry(reqwest::StatusCode::FORBIDDEN, &non_exhausted_headers, 0);
    assert!(!non_exhausted.retryable);
}

#[test]
fn rate_limit_reset_header_is_honored_for_exhausted_forbidden() {
    let reset_epoch = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs()
        + 2)
    .to_string();
    let mut headers = ReqwestHeaderMap::new();
    headers.insert("x-ratelimit-remaining", HeaderValue::from_static("0"));
    headers.insert(
        "x-ratelimit-reset",
        HeaderValue::from_str(&reset_epoch).unwrap(),
    );
    let decision = classify_retry(reqwest::StatusCode::FORBIDDEN, &headers, 0);
    assert!(decision.retryable);
    assert!(decision.delay.as_secs() <= 2);
}

#[test]
fn exponential_backoff_is_capped() {
    assert_eq!(exp_backoff(0).as_secs(), 1);
    assert_eq!(exp_backoff(6).as_secs(), 64);
    assert_eq!(exp_backoff(9).as_secs(), 64);
}

#[test]
fn locator_parsing_extracts_issue_node_id() {
    let payload = br#"{
        "action":"opened",
        "issue":{"node_id":"I_abc"},
        "repository":{"full_name":"Acme/Repo"}
    }"#;
    let locator = parse_locator("issues", payload).expect("parse");
    assert_eq!(locator.node_id.as_deref(), Some("I_abc"));
    assert_eq!(locator.repository_full_name.as_deref(), Some("acme/repo"));
}

#[test]
fn locator_parsing_handles_missing_optional_fields() {
    let payload = br#"{"action":"edited"}"#;
    let locator = parse_locator("issues", payload).expect("parse");
    assert_eq!(locator.action, "edited");
    assert!(locator.node_id.is_none());
}

#[test]
fn locator_parsing_issue_comment_prefers_comment_node_id() {
    let payload = br#"{
        "action":"created",
        "issue":{"node_id":"I_parent"},
        "comment":{"node_id":"IC_child"},
        "repository":{"full_name":"acme/repo"}
    }"#;
    let locator = parse_locator("issue_comment", payload).expect("parse");
    assert_eq!(locator.node_id.as_deref(), Some("IC_child"));
}

#[test]
fn locator_parsing_review_prefers_review_node_id() {
    let payload = br#"{
        "action":"submitted",
        "pull_request":{"node_id":"PR_parent"},
        "review":{"node_id":"R_child"},
        "repository":{"full_name":"acme/repo"}
    }"#;
    let locator = parse_locator("pull_request_review", payload).expect("parse");
    assert_eq!(locator.node_id.as_deref(), Some("R_child"));
}

#[test]
fn locator_parsing_project_item_uses_project_node_id_shape() {
    let payload = br#"{
        "action":"edited",
        "projects_v2_item":{"node_id":"PVTI_1","project_node_id":"PVT_1"}
    }"#;
    let locator = parse_locator("projects_v2_item", payload).expect("parse");
    assert_eq!(locator.node_id.as_deref(), Some("PVTI_1"));
    assert_eq!(locator.project_id.as_deref(), Some("PVT_1"));
    assert!(locator.project_owner.is_none());
    assert!(locator.project_number.is_none());
}

#[tokio::test]
async fn graphql_client_sends_bearer_token_header() {
    #[derive(Clone, Default)]
    struct AuthState {
        auth_header: Arc<RwLock<Option<String>>>,
    }

    async fn handler(
        State(state): State<AuthState>,
        headers: HeaderMap,
        Json(_payload): Json<serde_json::Value>,
    ) -> Json<serde_json::Value> {
        let auth = headers
            .get(AUTHORIZATION)
            .and_then(|h| h.to_str().ok())
            .map(str::to_string);
        *state.auth_header.write().await = auth;
        Json(json!({
            "data": {
                "repository": {
                    "id": "R_1",
                    "name": "repo",
                    "nameWithOwner": "acme/repo",
                    "owner": { "login": "acme" },
                    "description": null,
                    "url": "https://github.com/acme/repo",
                    "isArchived": false,
                    "isPrivate": false,
                    "createdAt": "2026-01-01T00:00:00Z",
                    "updatedAt": "2026-01-01T00:00:00Z",
                    "defaultBranchRef": { "name": "main" }
                }
            }
        }))
    }

    let state = AuthState::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("mock local addr");
    let app = Router::new()
        .route("/graphql", post(handler))
        .with_state(state.clone());
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let client = GitHubGraphQLClient::new(
        format!("http://{addr}/graphql"),
        "test-pat-token".to_string(),
    )
    .expect("client");
    client
        .fetch_repository("acme", "repo")
        .await
        .expect("fetch repository");

    let auth = state.auth_header.read().await.clone().expect("auth header");
    assert_eq!(auth, "Bearer test-pat-token");
    server.abort();
}

#[tokio::test]
async fn graphql_fetch_issue_comment_parses_authoritative_shape_fields() {
    async fn handler(Json(payload): Json<serde_json::Value>) -> Json<serde_json::Value> {
        let query = payload["query"].as_str().expect("query document");
        assert!(!query.contains("performedViaGithubApp"));
        assert!(!query.contains("__typename\n        id\n        login"));
        for actor_type in ["User", "Bot", "Organization", "Mannequin"] {
            assert!(query.contains(&format!("... on {actor_type} {{ id databaseId }}")));
        }
        assert!(query.contains("... on EnterpriseUserAccount { id }"));
        Json(json!({
            "data": {
                "node": {
                    "__typename": "IssueComment",
                    "id": "IC_1",
                    "body": "comment",
                    "createdAt": "2026-01-01T00:00:00Z",
                    "updatedAt": "2026-01-02T00:00:00Z",
                    "url": "https://github.com/acme/repo/issues/1#issuecomment-1",
                    "isMinimized": false,
                    "author": {
                        "__typename": "User",
                        "id": "U_NODE_1",
                        "login": "octocat",
                        "databaseId": 7
                    },
                    "issue": { "id": "I_1" },
                    "pullRequest": null,
                    "repository": { "id": "R_1", "nameWithOwner": "acme/repo" }
                }
            }
        }))
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");
    let app = Router::new().route("/graphql", post(handler));
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let client = GitHubGraphQLClient::new(format!("http://{addr}/graphql"), "pat".to_string())
        .expect("client");
    let comment = client
        .fetch_issue_comment("IC_1")
        .await
        .expect("fetch")
        .expect("comment");

    assert_eq!(
        comment
            .author
            .as_ref()
            .and_then(|author| author.id.as_deref()),
        Some("U_NODE_1")
    );
    assert_eq!(
        comment
            .author
            .as_ref()
            .and_then(|author| author.database_id),
        Some(7)
    );
    assert_eq!(
        comment
            .author
            .as_ref()
            .and_then(|author| author.actor_type.as_deref()),
        Some("User")
    );
    server.abort();
}

#[tokio::test]
async fn graphql_client_retries_5xx_with_backoff_and_succeeds() {
    #[derive(Clone, Default)]
    struct RetryState {
        calls: Arc<AtomicUsize>,
    }

    async fn handler(
        State(state): State<RetryState>,
        Json(_payload): Json<serde_json::Value>,
    ) -> (axum::http::StatusCode, Json<serde_json::Value>) {
        let attempt = state.calls.fetch_add(1, Ordering::SeqCst);
        if attempt == 0 {
            return (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "temporary outage" })),
            );
        }
        (
            axum::http::StatusCode::OK,
            Json(json!({
                "data": {
                    "repository": {
                        "id": "R_1",
                        "name": "repo",
                        "nameWithOwner": "acme/repo",
                        "owner": { "login": "acme" },
                        "description": null,
                        "url": "https://github.com/acme/repo",
                        "isArchived": false,
                        "isPrivate": false,
                        "createdAt": "2026-01-01T00:00:00Z",
                        "updatedAt": "2026-01-01T00:00:00Z",
                        "defaultBranchRef": { "name": "main" }
                    }
                }
            })),
        )
    }

    let state = RetryState::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");
    let app = Router::new()
        .route("/graphql", post(handler))
        .with_state(state.clone());
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let client = GitHubGraphQLClient::new(format!("http://{addr}/graphql"), "pat".to_string())
        .expect("client");
    let fetch_task = tokio::spawn(async move { client.fetch_repository("acme", "repo").await });
    let result = fetch_task.await.expect("task join").expect("fetch success");
    assert!(result.is_some());
    assert_eq!(state.calls.load(Ordering::SeqCst), 2);
    server.abort();
}

#[tokio::test]
async fn graphql_client_retries_transient_transport_failure() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");

    let server = tokio::spawn(async move {
        for attempt in 0..2 {
            let (mut stream, _) = listener.accept().await.expect("accept");
            if attempt == 0 {
                drop(stream);
                continue;
            }

            let mut request_buffer = [0u8; 2048];
            let _ = stream.read(&mut request_buffer).await;
            let body = json!({
                "data": {
                    "repository": {
                        "id": "R_1",
                        "name": "repo",
                        "nameWithOwner": "acme/repo",
                        "owner": { "login": "acme" },
                        "description": null,
                        "url": "https://github.com/acme/repo",
                        "isArchived": false,
                        "isPrivate": false,
                        "createdAt": "2026-01-01T00:00:00Z",
                        "updatedAt": "2026-01-01T00:00:00Z",
                        "defaultBranchRef": { "name": "main" }
                    }
                }
            })
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.flush().await;
        }
    });

    let client = GitHubGraphQLClient::new(format!("http://{addr}/graphql"), "pat".to_string())
        .expect("client");
    let fetch_task = tokio::spawn(async move { client.fetch_repository("acme", "repo").await });
    let result = fetch_task.await.expect("task join").expect("fetch success");
    assert!(result.is_some());
    server.await.expect("server task");
}

#[tokio::test]
async fn graphql_client_retries_retryable_graphql_error_then_succeeds() {
    #[derive(Clone, Default)]
    struct RetryState {
        calls: Arc<AtomicUsize>,
    }

    async fn handler(
        State(state): State<RetryState>,
        Json(_payload): Json<serde_json::Value>,
    ) -> Json<serde_json::Value> {
        let attempt = state.calls.fetch_add(1, Ordering::SeqCst);
        if attempt == 0 {
            return Json(json!({
                "errors": [{
                    "message": "Secondary rate limit. Please try again shortly.",
                    "type": "RATE_LIMITED"
                }]
            }));
        }
        Json(json!({
            "data": {
                "repository": {
                    "id": "R_1",
                    "name": "repo",
                    "nameWithOwner": "acme/repo",
                    "owner": { "login": "acme" },
                    "description": null,
                    "url": "https://github.com/acme/repo",
                    "isArchived": false,
                    "isPrivate": false,
                    "createdAt": "2026-01-01T00:00:00Z",
                    "updatedAt": "2026-01-01T00:00:00Z",
                    "defaultBranchRef": { "name": "main" }
                }
            }
        }))
    }

    let state = RetryState::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");
    let app = Router::new()
        .route("/graphql", post(handler))
        .with_state(state.clone());
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let client = GitHubGraphQLClient::new(format!("http://{addr}/graphql"), "pat".to_string())
        .expect("client");
    let fetch_task = tokio::spawn(async move { client.fetch_repository("acme", "repo").await });
    let result = fetch_task.await.expect("task join").expect("fetch success");
    assert!(result.is_some());
    assert_eq!(state.calls.load(Ordering::SeqCst), 2);
    server.abort();
}

#[tokio::test]
async fn graphql_client_does_not_retry_permanent_graphql_errors() {
    #[derive(Clone, Default)]
    struct RetryState {
        calls: Arc<AtomicUsize>,
    }

    async fn handler(
        State(state): State<RetryState>,
        Json(_payload): Json<serde_json::Value>,
    ) -> Json<serde_json::Value> {
        state.calls.fetch_add(1, Ordering::SeqCst);
        Json(json!({
            "errors": [{
                "message": "Could not resolve to a node with the global id",
                "type": "NOT_FOUND"
            }]
        }))
    }

    let state = RetryState::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");
    let app = Router::new()
        .route("/graphql", post(handler))
        .with_state(state.clone());
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let client = GitHubGraphQLClient::new(format!("http://{addr}/graphql"), "pat".to_string())
        .expect("client");
    let err = client
        .fetch_repository("acme", "repo")
        .await
        .expect_err("permanent error should fail");
    assert!(format!("{err:#}").contains("returned errors"));
    assert_eq!(state.calls.load(Ordering::SeqCst), 1);
    server.abort();
}

#[tokio::test]
async fn fetch_repository_treats_path_not_found_as_absent() {
    async fn handler(Json(_payload): Json<serde_json::Value>) -> Json<serde_json::Value> {
        Json(json!({
            "data": { "repository": null },
            "errors": [{
                "message": "Could not resolve to a Repository",
                "type": "NOT_FOUND",
                "path": ["repository"]
            }]
        }))
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");
    let app = Router::new().route("/graphql", post(handler));
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let client = GitHubGraphQLClient::new(format!("http://{addr}/graphql"), "pat".to_string())
        .expect("client");
    let repository = client
        .fetch_repository("acme", "deleted")
        .await
        .expect("path-specific NOT_FOUND should be authoritative absence");
    assert!(repository.is_none());
    server.abort();
}

#[tokio::test]
async fn project_owner_lookup_accepts_only_alternate_namespace_not_found() {
    async fn handler(Json(payload): Json<serde_json::Value>) -> Json<serde_json::Value> {
        let query = payload["query"].as_str().expect("query document");
        assert!(!query.contains("owner { login }"));
        assert!(query.contains("... on Organization { login }"));
        assert!(query.contains("... on User { login }"));
        let owner = payload["variables"]["owner"].as_str().unwrap_or_default();
        let project = |id: &str, owner: &str| {
            json!({
                "id": id,
                "title": "Roadmap",
                "number": 1,
                "url": format!("https://github.com/users/{owner}/projects/1"),
                "createdAt": "2026-01-01T00:00:00Z",
                "updatedAt": "2026-01-01T00:00:00Z",
                "owner": { "login": owner },
                "items": {
                    "pageInfo": { "hasNextPage": false, "endCursor": null },
                    "nodes": []
                }
            })
        };

        if owner == "acme" {
            Json(json!({
                "data": {
                    "organization": { "projectV2": project("PVT_org", owner) },
                    "user": null
                },
                "errors": [{
                    "type": "NOT_FOUND",
                    "path": ["user"],
                    "locations": [{ "line": 8, "column": 3 }],
                    "message": "Could not resolve to a User with the login of 'acme'."
                }]
            }))
        } else {
            Json(json!({
                "data": {
                    "organization": null,
                    "user": { "projectV2": project("PVT_user", owner) }
                },
                "errors": [{
                    "type": "NOT_FOUND",
                    "path": ["organization"],
                    "locations": [{ "line": 2, "column": 3 }],
                    "message": "Could not resolve to an Organization with the login of 'octocat'."
                }]
            }))
        }
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");
    let app = Router::new().route("/graphql", post(handler));
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let client = GitHubGraphQLClient::new(format!("http://{addr}/graphql"), "pat".to_string())
        .expect("client");

    let organization_project = client
        .fetch_project_by_owner_number("acme", 1)
        .await
        .expect("organization project")
        .expect("organization project exists");
    assert_eq!(organization_project.id, "PVT_org");
    assert_eq!(organization_project.owner.login, "acme");

    let user_project = client
        .fetch_project_by_owner_number("octocat", 1)
        .await
        .expect("user project")
        .expect("user project exists");
    assert_eq!(user_project.id, "PVT_user");
    assert_eq!(user_project.owner.login, "octocat");
    server.abort();
}

#[tokio::test]
async fn nullable_node_lookup_only_accepts_path_specific_not_found() {
    async fn handler(Json(payload): Json<serde_json::Value>) -> Json<serde_json::Value> {
        let id = payload["variables"]["id"].as_str().unwrap_or_default();
        if id == "I_deleted" {
            return Json(json!({
                "data": { "node": null },
                "errors": [{
                    "type": "NOT_FOUND",
                    "path": ["node"],
                    "locations": [{ "line": 2, "column": 3 }],
                    "message": "Could not resolve to a node with the global id of 'I_deleted'"
                }]
            }));
        }
        Json(json!({
            "data": { "node": null },
            "errors": [{
                "type": "FORBIDDEN",
                "path": ["node"],
                "message": "Resource not accessible by personal access token"
            }]
        }))
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");
    let app = Router::new().route("/graphql", post(handler));
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let client = GitHubGraphQLClient::new(format!("http://{addr}/graphql"), "pat".to_string())
        .expect("client");

    assert!(client
        .fetch_issue("I_deleted")
        .await
        .expect("deleted node is authoritative absence")
        .is_none());
    let err = client
        .fetch_issue("I_forbidden")
        .await
        .expect_err("permission errors must not become absence");
    assert!(format!("{err:#}").contains("Resource not accessible"));
    server.abort();
}

#[tokio::test]
async fn fetch_issue_paginates_comments_across_pages() {
    #[derive(Clone, Default)]
    struct ServerState {
        comment_page_calls: Arc<AtomicUsize>,
    }

    async fn handler(
        State(state): State<ServerState>,
        Json(payload): Json<serde_json::Value>,
    ) -> Json<serde_json::Value> {
        let query = payload
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let cursor = payload
            .get("variables")
            .and_then(|v| v.get("cursor"))
            .and_then(|v| v.as_str());

        if query.contains("comments(first: 100, after: $cursor)") {
            state.comment_page_calls.fetch_add(1, Ordering::SeqCst);
            return Json(json!({
                "data": {
                    "node": {
                        "__typename": "Issue",
                        "comments": {
                            "pageInfo": { "hasNextPage": false, "endCursor": null },
                            "nodes": [{
                                "id": "IC_2",
                                "body": "second",
                                "createdAt": "2026-01-02T00:00:00Z",
                                "updatedAt": "2026-01-02T00:00:00Z",
                                "url": "https://github.com/acme/repo/issues/1#issuecomment-2",
                                "isMinimized": false,
                                "author": { "__typename": "User", "id": "U_2", "login": "user2", "databaseId": 2 },
                                "issue": { "id": "I_1" },
                                "pullRequest": null,
                                "repository": { "id": "R_1", "nameWithOwner": "acme/repo" }
                            }]
                        }
                    }
                }
            }));
        }

        assert!(cursor.is_none());
        Json(json!({
            "data": {
                "node": {
                    "__typename": "Issue",
                    "id": "I_1",
                    "number": 1,
                    "title": "Issue",
                    "body": "body",
                    "state": "OPEN",
                    "createdAt": "2026-01-01T00:00:00Z",
                    "updatedAt": "2026-01-01T00:00:00Z",
                    "closedAt": null,
                    "url": "https://github.com/acme/repo/issues/1",
                    "author": { "login": "octocat" },
                    "repository": { "id": "R_1", "nameWithOwner": "acme/repo" },
                    "assignees": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] },
                    "labels": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] },
                    "comments": {
                        "pageInfo": { "hasNextPage": true, "endCursor": "c1" },
                        "nodes": [{
                            "id": "IC_1",
                            "body": "first",
                            "createdAt": "2026-01-01T00:00:00Z",
                            "updatedAt": "2026-01-01T00:00:00Z",
                            "url": "https://github.com/acme/repo/issues/1#issuecomment-1",
                            "isMinimized": false,
                            "author": { "__typename": "Bot", "id": "U_1", "login": "user1", "databaseId": 1 },
                            "issue": { "id": "I_1" },
                            "pullRequest": null,
                            "repository": { "id": "R_1", "nameWithOwner": "acme/repo" }
                        }]
                    }
                }
            }
        }))
    }

    let state = ServerState::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");
    let app = Router::new()
        .route("/graphql", post(handler))
        .with_state(state.clone());
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let client = GitHubGraphQLClient::new(format!("http://{addr}/graphql"), "pat".to_string())
        .expect("client");
    let issue = client
        .fetch_issue("I_1")
        .await
        .expect("fetch")
        .expect("issue");

    assert_eq!(issue.comments.nodes.len(), 2);
    assert_eq!(issue.comments.nodes[0].id, "IC_1");
    assert_eq!(issue.comments.nodes[1].id, "IC_2");
    assert_eq!(
        issue.comments.nodes[0]
            .author
            .as_ref()
            .and_then(|actor| actor.actor_type.as_deref()),
        Some("Bot")
    );
    assert_eq!(state.comment_page_calls.load(Ordering::SeqCst), 1);
    server.abort();
}

#[tokio::test]
async fn fetch_pull_request_paginates_reviews_and_review_comments() {
    #[derive(Clone, Default)]
    struct ServerState {
        review_page_calls: Arc<AtomicUsize>,
        review_comment_page_calls: Arc<AtomicUsize>,
    }

    async fn handler(
        State(state): State<ServerState>,
        Json(payload): Json<serde_json::Value>,
    ) -> Json<serde_json::Value> {
        let query = payload
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let cursor = payload
            .get("variables")
            .and_then(|v| v.get("cursor"))
            .and_then(|v| v.as_str());

        if query.contains("reviews(first: 100, after: $cursor)") {
            state.review_page_calls.fetch_add(1, Ordering::SeqCst);
            return Json(json!({
                "data": {
                    "node": {
                        "__typename": "PullRequest",
                        "reviews": {
                            "pageInfo": { "hasNextPage": false, "endCursor": null },
                            "nodes": [{
                                "id": "R_2",
                                "state": "COMMENTED",
                                "body": null,
                                "createdAt": "2026-01-03T00:00:00Z",
                                "updatedAt": "2026-01-03T00:00:00Z",
                                "url": "https://github.com/acme/repo/pull/1#review-2",
                                "author": { "__typename": "User", "id": "U_2", "login": "reviewer2", "databaseId": 2 },
                                "pullRequest": { "id": "PR_1", "repository": { "id": "R_1", "nameWithOwner": "acme/repo" } },
                                "comments": {
                                    "pageInfo": { "hasNextPage": false, "endCursor": null },
                                    "nodes": []
                                }
                            }]
                        }
                    }
                }
            }));
        }

        if query.contains("... on PullRequestReview {") && query.contains("after: $cursor") {
            state
                .review_comment_page_calls
                .fetch_add(1, Ordering::SeqCst);
            return Json(json!({
                "data": {
                    "node": {
                        "__typename": "PullRequestReview",
                        "comments": {
                            "pageInfo": { "hasNextPage": false, "endCursor": null },
                            "nodes": [{
                                "id": "RC_2",
                                "body": "second review comment",
                                "path": "src/lib.rs",
                                "position": 2,
                                "line": 20,
                                "diffHunk": "@@",
                                "createdAt": "2026-01-02T00:00:00Z",
                                "updatedAt": "2026-01-02T00:00:00Z",
                                "url": "https://github.com/acme/repo/pull/1#discussion_r2",
                                "author": { "__typename": "Bot", "id": "U_3", "login": "reviewer3", "databaseId": 3 },
                                "repository": { "id": "R_1", "nameWithOwner": "acme/repo" },
                                "pullRequestReview": { "id": "R_1", "pullRequest": { "id": "PR_1", "repository": { "id": "R_1", "nameWithOwner": "acme/repo" } } }
                            }]
                        }
                    }
                }
            }));
        }

        assert!(cursor.is_none());
        let response = serde_json::from_str::<serde_json::Value>(
            r#"{
              "data": {
                "node": {
                  "__typename": "PullRequest",
                  "id": "PR_1",
                  "number": 1,
                  "title": "PR title",
                  "body": "PR body",
                  "state": "OPEN",
                  "createdAt": "2026-01-01T00:00:00Z",
                  "updatedAt": "2026-01-01T00:00:00Z",
                  "closedAt": null,
                  "mergedAt": null,
                  "url": "https://github.com/acme/repo/pull/1",
                  "isDraft": false,
                  "headRefName": "feature",
                  "baseRefName": "main",
                  "author": { "login": "octocat" },
                  "repository": { "id": "R_1", "nameWithOwner": "acme/repo" },
                  "assignees": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] },
                  "labels": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] },
                  "comments": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] },
                  "reviews": {
                    "pageInfo": { "hasNextPage": true, "endCursor": "r1" },
                    "nodes": [{
                      "id": "R_1",
                      "state": "APPROVED",
                      "body": null,
                      "createdAt": "2026-01-01T00:00:00Z",
                      "updatedAt": "2026-01-01T00:00:00Z",
                      "url": "https://github.com/acme/repo/pull/1#review-1",
                      "author": { "__typename": "User", "id": "U_1", "login": "reviewer1", "databaseId": 1 },
                      "pullRequest": { "id": "PR_1", "repository": { "id": "R_1", "nameWithOwner": "acme/repo" } },
                      "comments": {
                        "pageInfo": { "hasNextPage": true, "endCursor": "rc1" },
                        "nodes": [{
                          "id": "RC_1",
                          "body": "first review comment",
                          "path": "src/lib.rs",
                          "position": 1,
                          "line": 10,
                          "diffHunk": "@@",
                          "createdAt": "2026-01-01T00:00:00Z",
                          "updatedAt": "2026-01-01T00:00:00Z",
                          "url": "https://github.com/acme/repo/pull/1#discussion_r1",
                          "pullRequestReview": { "id": "R_1", "pullRequest": { "id": "PR_1", "repository": { "id": "R_1", "nameWithOwner": "acme/repo" } } },
                          "author": { "__typename": "User", "id": "U_1", "login": "reviewer1", "databaseId": 1 },
                          "repository": { "id": "R_1", "nameWithOwner": "acme/repo" }
                        }]
                      }
                    }]
                  }
                }
              }
            }"#,
        )
        .expect("valid pull request response json");
        Json(response)
    }

    let state = ServerState::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");
    let app = Router::new()
        .route("/graphql", post(handler))
        .with_state(state.clone());
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let client = GitHubGraphQLClient::new(format!("http://{addr}/graphql"), "pat".to_string())
        .expect("client");
    let pr = client
        .fetch_pull_request("PR_1")
        .await
        .expect("fetch")
        .expect("pr");

    assert_eq!(pr.reviews.nodes.len(), 2);
    assert_eq!(pr.reviews.nodes[0].comments.nodes.len(), 2);
    assert_eq!(pr.reviews.nodes[0].comments.nodes[1].id, "RC_2");
    assert_eq!(
        pr.reviews.nodes[0]
            .author
            .as_ref()
            .and_then(|author| author.actor_type.as_deref()),
        Some("User")
    );
    assert_eq!(state.review_page_calls.load(Ordering::SeqCst), 1);
    assert_eq!(state.review_comment_page_calls.load(Ordering::SeqCst), 1);
    server.abort();
}

#[tokio::test]
async fn fetch_project_item_paginates_field_values() {
    #[derive(Clone, Default)]
    struct ServerState {
        field_values_page_calls: Arc<AtomicUsize>,
    }

    async fn handler(
        State(state): State<ServerState>,
        Json(payload): Json<serde_json::Value>,
    ) -> Json<serde_json::Value> {
        let query = payload
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or_default();

        if query.contains("fieldValues(first: 50, after: $cursor)") {
            state.field_values_page_calls.fetch_add(1, Ordering::SeqCst);
            return Json(json!({
                "data": {
                    "node": {
                        "__typename": "ProjectV2Item",
                        "fieldValues": {
                            "pageInfo": { "hasNextPage": false, "endCursor": null },
                            "nodes": [{
                                "__typename": "ProjectV2ItemFieldTextValue",
                                "text": "extra",
                                "field": { "id": "f2", "name": "Notes" }
                            }]
                        }
                    }
                }
            }));
        }

        Json(json!({
            "data": {
                "node": {
                    "__typename": "ProjectV2Item",
                    "id": "PVTI_1",
                    "type": "ISSUE",
                    "createdAt": "2026-01-01T00:00:00Z",
                    "updatedAt": "2026-01-01T00:00:00Z",
                    "project": {
                        "id": "PVT_1",
                        "number": 1,
                        "owner": { "login": "acme" }
                    },
                    "content": {
                        "__typename": "Issue",
                        "id": "I_1",
                        "number": 1,
                        "title": "Issue",
                        "state": "OPEN",
                        "repository": { "id": "R_1", "nameWithOwner": "acme/repo" }
                    },
                    "fieldValues": {
                        "pageInfo": { "hasNextPage": true, "endCursor": "fv1" },
                        "nodes": [{
                            "__typename": "ProjectV2ItemFieldSingleSelectValue",
                            "name": "In Progress",
                            "optionId": "opt1",
                            "field": { "id": "f1", "name": "Status" }
                        }]
                    }
                }
            }
        }))
    }

    let state = ServerState::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");
    let app = Router::new()
        .route("/graphql", post(handler))
        .with_state(state.clone());
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let client = GitHubGraphQLClient::new(format!("http://{addr}/graphql"), "pat".to_string())
        .expect("client");
    let item = client
        .fetch_project_item("PVTI_1")
        .await
        .expect("fetch")
        .expect("item");

    assert_eq!(item.field_values.nodes.len(), 2);
    assert_eq!(state.field_values_page_calls.load(Ordering::SeqCst), 1);
    server.abort();
}

#[tokio::test]
async fn fetch_issue_errors_when_has_next_page_missing_end_cursor() {
    async fn handler(Json(_payload): Json<serde_json::Value>) -> Json<serde_json::Value> {
        Json(json!({
            "data": {
                "node": {
                    "__typename": "Issue",
                    "id": "I_1",
                    "number": 1,
                    "title": "Issue",
                    "body": "body",
                    "state": "OPEN",
                    "createdAt": "2026-01-01T00:00:00Z",
                    "updatedAt": "2026-01-01T00:00:00Z",
                    "closedAt": null,
                    "url": "https://github.com/acme/repo/issues/1",
                    "author": { "login": "octocat" },
                    "repository": { "id": "R_1", "nameWithOwner": "acme/repo" },
                    "assignees": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] },
                    "labels": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] },
                    "comments": { "pageInfo": { "hasNextPage": true, "endCursor": null }, "nodes": [] }
                }
            }
        }))
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");
    let app = Router::new().route("/graphql", post(handler));
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let client = GitHubGraphQLClient::new(format!("http://{addr}/graphql"), "pat".to_string())
        .expect("client");
    let err = client
        .fetch_issue("I_1")
        .await
        .expect_err("missing cursor must fail");
    assert!(format!("{err:#}").contains("hasNextPage=true but endCursor was absent"));
    server.abort();
}

#[tokio::test]
async fn fetch_issue_errors_when_root_disappears_after_first_page() {
    async fn handler(Json(payload): Json<serde_json::Value>) -> Json<serde_json::Value> {
        let query = payload
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        if query.contains("comments(first: 100, after: $cursor)") {
            return Json(json!({ "data": { "node": null } }));
        }

        Json(json!({
            "data": {
                "node": {
                    "__typename": "Issue",
                    "id": "I_1",
                    "number": 1,
                    "title": "Issue",
                    "body": "body",
                    "state": "OPEN",
                    "createdAt": "2026-01-01T00:00:00Z",
                    "updatedAt": "2026-01-01T00:00:00Z",
                    "closedAt": null,
                    "url": "https://github.com/acme/repo/issues/1",
                    "author": { "login": "octocat" },
                    "repository": { "id": "R_1", "nameWithOwner": "acme/repo" },
                    "assignees": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] },
                    "labels": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] },
                    "comments": { "pageInfo": { "hasNextPage": true, "endCursor": "c1" }, "nodes": [] }
                }
            }
        }))
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");
    let app = Router::new().route("/graphql", post(handler));
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let client = GitHubGraphQLClient::new(format!("http://{addr}/graphql"), "pat".to_string())
        .expect("client");
    let err = client
        .fetch_issue("I_1")
        .await
        .expect_err("disappearing paginated root must fail");
    assert!(format!("{err:#}").contains("disappeared after first page"));
    server.abort();
}

#[tokio::test]
async fn fetch_all_issues_errors_when_repository_disappears_after_first_page() {
    async fn handler(Json(payload): Json<serde_json::Value>) -> Json<serde_json::Value> {
        let cursor = payload
            .get("variables")
            .and_then(|v| v.get("cursor"))
            .and_then(|v| v.as_str());

        if cursor.is_some() {
            return Json(json!({ "data": { "repository": null } }));
        }

        Json(json!({
            "data": {
                "repository": {
                    "issues": {
                        "pageInfo": { "hasNextPage": true, "endCursor": "next" },
                        "nodes": [{
                            "id": "I_1",
                            "number": 1,
                            "title": "Issue",
                            "body": "body",
                            "state": "OPEN",
                            "createdAt": "2026-01-01T00:00:00Z",
                            "updatedAt": "2026-01-01T00:00:00Z",
                            "closedAt": null,
                            "url": "https://github.com/acme/repo/issues/1",
                            "author": { "login": "octocat" },
                            "repository": { "id": "R_1", "nameWithOwner": "acme/repo" },
                            "assignees": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] },
                            "labels": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] },
                            "comments": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] }
                        }]
                    }
                }
            }
        }))
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");
    let app = Router::new().route("/graphql", post(handler));
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let client = GitHubGraphQLClient::new(format!("http://{addr}/graphql"), "pat".to_string())
        .expect("client");
    let err = client
        .fetch_all_issues("acme/repo")
        .await
        .expect_err("repository disappearance must fail");
    assert!(format!("{err:#}").contains("Pagination root disappeared after first page"));
    server.abort();
}

#[tokio::test]
async fn hydrator_does_not_commit_partial_snapshot_on_pagination_failure() {
    async fn handler(Json(payload): Json<serde_json::Value>) -> Json<serde_json::Value> {
        let query = payload
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        if query.contains("comments(first: 100, after: $cursor)") {
            return Json(json!({ "data": { "node": null } }));
        }

        Json(json!({
            "data": {
                "node": {
                    "__typename": "Issue",
                    "id": "I_1",
                    "number": 1,
                    "title": "from-graphql",
                    "body": "body",
                    "state": "OPEN",
                    "createdAt": "2026-01-01T00:00:00Z",
                    "updatedAt": "2026-01-01T00:00:00Z",
                    "closedAt": null,
                    "url": "https://github.com/acme/repo/issues/1",
                    "author": { "login": "octocat" },
                    "repository": { "id": "R_1", "nameWithOwner": "acme/repo" },
                    "assignees": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] },
                    "labels": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] },
                    "comments": { "pageInfo": { "hasNextPage": true, "endCursor": "c1" }, "nodes": [] }
                }
            }
        }))
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");
    let app = Router::new().route("/graphql", post(handler));
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let temp = TempDir::new().expect("tempdir");
    let wal = Arc::new(RedbWalProvider::new(temp.path()));
    wal.register(
        "src",
        DurabilityConfig {
            enabled: true,
            max_events: 32,
            capacity_policy: CapacityPolicy::RejectIncoming,
        }
        .to_wal_config(),
    )
    .await
    .expect("register wal");
    let state_store = Arc::new(MemoryStateStoreProvider::new());
    let base = test_source_base("src");
    let params = HydratorParams {
        source_id: "src".to_string(),
        base,
        wal: wal.clone(),
        state_store: state_store.clone(),
        api_client: Arc::new(
            GitHubGraphQLClient::new(format!("http://{addr}/graphql"), "pat".to_string())
                .expect("client"),
        ),
        projects: vec![],
        effective_repos: Arc::new(RwLock::new(HashSet::from(["acme/repo".to_string()]))),
        notify: Arc::new(tokio::sync::Notify::new()),
        health: Arc::new(RwLock::new(HydratorHealth::default())),
        processing_gate: Arc::new(tokio::sync::Mutex::new(())),
        shutdown: tokio::sync::watch::channel(false).1,
    };

    let locator = WebhookLocator {
        event_type: "issues".to_string(),
        action: "edited".to_string(),
        node_id: Some("I_1".to_string()),
        repository_full_name: Some("acme/repo".to_string()),
        parent_issue_id: None,
        parent_pull_request_id: None,
        project_id: None,
        project_owner: None,
        project_number: None,
    };
    let key = snapshot_key_for_locator(&locator, None);
    let (_, previous_snapshot) = map_root_diff(
        "src",
        &FetchedRoot::Issue(sample_issue("stable")),
        None,
        1_000,
    )
    .expect("snapshot");
    save_root_snapshot(state_store.as_ref(), "src", &key, &previous_snapshot)
        .await
        .expect("save previous");

    let admission = encode_admission_change("src", "delivery-partial", &locator).expect("encode");
    let sequence = wal.append("src", &admission).await.expect("append");
    let err = process_admission(&params, sequence, &admission)
        .await
        .expect_err("pagination failure must fail the admission");
    assert!(format!("{err:#}").contains("disappeared after first page"));

    let persisted = load_root_snapshot(state_store.as_ref(), "src", &key)
        .await
        .expect("load persisted snapshot")
        .expect("snapshot exists");
    assert_eq!(
        persisted.elements["I_1"].properties["title"],
        json!("stable")
    );
    assert!(wal.oldest_sequence("src").await.expect("oldest").is_some());
    server.abort();
}

#[tokio::test]
async fn processing_gate_serializes_reconcile_and_hydrator_delete() {
    #[derive(Clone)]
    struct ServerState {
        reconcile_pause_used: Arc<AtomicUsize>,
        reconcile_started: Arc<Notify>,
        reconcile_release: Arc<Notify>,
    }

    async fn handler(
        State(state): State<ServerState>,
        Json(payload): Json<serde_json::Value>,
    ) -> Json<serde_json::Value> {
        let query = payload
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let cursor = payload
            .get("variables")
            .and_then(|v| v.get("cursor"))
            .and_then(|v| v.as_str());

        if query.contains("query($owner: String!, $name: String!)")
            && query.contains("defaultBranchRef")
        {
            return Json(json!({
                "data": {
                    "repository": {
                        "id": "R_1",
                        "name": "repo",
                        "nameWithOwner": "acme/repo",
                        "owner": { "login": "acme" },
                        "description": null,
                        "url": "https://github.com/acme/repo",
                        "isArchived": false,
                        "isPrivate": false,
                        "createdAt": "2026-01-01T00:00:00Z",
                        "updatedAt": "2026-01-01T00:00:00Z",
                        "defaultBranchRef": null
                    }
                }
            }));
        }

        if query.contains("issues(first: 100, after: $cursor") {
            if state.reconcile_pause_used.fetch_add(1, Ordering::SeqCst) == 0 {
                state.reconcile_started.notify_waiters();
                state.reconcile_release.notified().await;
            }

            if cursor.is_some() {
                return Json(json!({
                    "data": {
                        "repository": {
                            "issues": {
                                "pageInfo": { "hasNextPage": false, "endCursor": null },
                                "nodes": []
                            }
                        }
                    }
                }));
            }

            return Json(json!({
                "data": {
                    "repository": {
                        "issues": {
                            "pageInfo": { "hasNextPage": false, "endCursor": null },
                            "nodes": [{
                                "id": "I_1",
                                "number": 1,
                                "title": "stale issue",
                                "body": "body",
                                "state": "OPEN",
                                "createdAt": "2026-01-01T00:00:00Z",
                                "updatedAt": "2026-01-01T00:00:00Z",
                                "closedAt": null,
                                "url": "https://github.com/acme/repo/issues/1",
                                "author": { "login": "octocat" },
                                "repository": { "id": "R_1", "nameWithOwner": "acme/repo" },
                                "assignees": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] },
                                "labels": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] },
                                "comments": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] }
                            }]
                        }
                    }
                }
            }));
        }

        if query.contains("pullRequests(first: 100, after: $cursor") {
            return Json(json!({
                "data": {
                    "repository": {
                        "pullRequests": {
                            "pageInfo": { "hasNextPage": false, "endCursor": null },
                            "nodes": []
                        }
                    }
                }
            }));
        }

        if query.contains("query($id: ID!)") && query.contains("... on Issue") {
            return Json(json!({ "data": { "node": null } }));
        }

        Json(json!({ "data": {} }))
    }

    let server_state = ServerState {
        reconcile_pause_used: Arc::new(AtomicUsize::new(0)),
        reconcile_started: Arc::new(Notify::new()),
        reconcile_release: Arc::new(Notify::new()),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");
    let app = Router::new()
        .route("/graphql", post(handler))
        .with_state(server_state.clone());
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let temp = TempDir::new().expect("tempdir");
    let wal = Arc::new(RedbWalProvider::new(temp.path()));
    wal.register(
        "src",
        DurabilityConfig {
            enabled: true,
            max_events: 32,
            capacity_policy: CapacityPolicy::RejectIncoming,
        }
        .to_wal_config(),
    )
    .await
    .expect("register wal");
    let state_store = Arc::new(MemoryStateStoreProvider::new());
    let processing_gate = Arc::new(tokio::sync::Mutex::new(()));
    let api_client = Arc::new(
        GitHubGraphQLClient::new(format!("http://{addr}/graphql"), "pat".to_string())
            .expect("client"),
    );
    let effective_repos = Arc::new(RwLock::new(HashSet::from(["acme/repo".to_string()])));

    let issue = sample_issue("stable");
    let (_, root_snapshot) = map_root_diff("src", &FetchedRoot::Issue(issue.clone()), None, 1_000)
        .expect("root snapshot");
    let locator = WebhookLocator {
        event_type: "issues".to_string(),
        action: "deleted".to_string(),
        node_id: Some("I_1".to_string()),
        repository_full_name: Some("acme/repo".to_string()),
        parent_issue_id: None,
        parent_pull_request_id: None,
        project_id: None,
        project_owner: None,
        project_number: None,
    };
    let root_key = snapshot_key_for_locator(&locator, None);
    save_root_snapshot(state_store.as_ref(), "src", &root_key, &root_snapshot)
        .await
        .expect("save initial root snapshot");

    let mut reconcile_snapshot = ReconcileSnapshot::default();
    reconcile_snapshot.repositories.insert(
        "R_1".to_string(),
        RepositoryData {
            id: "R_1".to_string(),
            name: "repo".to_string(),
            name_with_owner: "acme/repo".to_string(),
            owner: OwnerRef {
                login: "acme".to_string(),
            },
            description: None,
            url: "https://github.com/acme/repo".to_string(),
            is_archived: false,
            is_private: false,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            default_branch_ref: None,
        },
    );
    reconcile_snapshot
        .issues
        .insert(issue.id.clone(), issue.clone());
    let (_, reconcile_index) =
        map_reconcile_snapshot("src", &reconcile_snapshot, &HashMap::new(), 1_000);
    crate::hydrator::save_reconcile_index(state_store.as_ref(), "src", &reconcile_index)
        .await
        .expect("seed reconcile index");

    let (reconcile_shutdown_tx, reconcile_shutdown_rx) = tokio::sync::watch::channel(false);
    let reconcile_params = ReconcilerParams {
        source_id: "src".to_string(),
        base: test_source_base("src"),
        state_store: state_store.clone(),
        api_client: api_client.clone(),
        projects: vec![],
        static_repos: HashSet::from(["acme/repo".to_string()]),
        effective_repos: effective_repos.clone(),
        interval_secs: 3600,
        run_initial_pass: true,
        processing_gate: processing_gate.clone(),
        shutdown: reconcile_shutdown_rx,
    };
    let reconciler_task = tokio::spawn(async move { run_reconciler_loop(reconcile_params).await });

    tokio::time::timeout(
        Duration::from_secs(2),
        server_state.reconcile_started.notified(),
    )
    .await
    .expect("reconcile should begin and pause");

    let admission =
        encode_admission_change("src", "delivery-gated-delete", &locator).expect("encode");
    let sequence = wal.append("src", &admission).await.expect("append");
    let hydrate_params = HydratorParams {
        source_id: "src".to_string(),
        base: test_source_base("src"),
        wal: wal.clone(),
        state_store: state_store.clone(),
        api_client: api_client.clone(),
        projects: vec![],
        effective_repos,
        notify: Arc::new(tokio::sync::Notify::new()),
        health: Arc::new(RwLock::new(HydratorHealth::default())),
        processing_gate: processing_gate.clone(),
        shutdown: tokio::sync::watch::channel(false).1,
    };
    let hydrate_task =
        tokio::spawn(async move { process_admission(&hydrate_params, sequence, &admission).await });

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !hydrate_task.is_finished(),
        "hydrator should wait for the shared processing gate while reconcile holds it"
    );

    server_state.reconcile_release.notify_waiters();
    hydrate_task
        .await
        .expect("join hydrate task")
        .expect("hydrate delete should succeed");

    reconcile_shutdown_tx.send(true).expect("send shutdown");
    tokio::time::timeout(Duration::from_secs(2), reconciler_task)
        .await
        .expect("reconciler should stop quickly")
        .expect("join reconciler task")
        .expect("reconciler loop should exit cleanly");

    let tombstone = load_root_snapshot(state_store.as_ref(), "src", &root_key)
        .await
        .expect("load root snapshot")
        .expect("root snapshot should exist");
    assert!(tombstone.elements.is_empty());

    let index = crate::hydrator::load_reconcile_index(state_store.as_ref(), "src")
        .await
        .expect("load reconcile index");
    assert!(!index.contains_key("I_1"));
    assert!(wal.oldest_sequence("src").await.expect("oldest").is_none());
    server.abort();
}

#[tokio::test]
async fn start_rejects_non_durable_state_store() {
    let source = GitHubSourceBuilder::new("github-source-durable-test")
        .with_config(valid_config_with_port(0))
        .build()
        .expect("build source");

    let temp = TempDir::new().expect("tempdir");
    let wal = Arc::new(RedbWalProvider::new(temp.path()));
    let core = DrasiLib::builder()
        .with_id("github-source-durable-core")
        .with_source(source)
        .with_state_store_provider(Arc::new(MemoryStateStoreProvider::new()))
        .with_wal_provider(wal)
        .build()
        .await
        .expect("build drasi");

    let err = core.start().await.expect_err("start should fail");
    assert!(format!("{err:#}").contains("is_durable"));
}

#[tokio::test]
async fn start_fails_fast_on_corrupted_effective_repos_state() {
    let source_id = "github-source-corrupt-effective-repos";
    let source = GitHubSourceBuilder::new(source_id)
        .with_config(valid_config_with_port(0))
        .build()
        .expect("build source");

    let temp = TempDir::new().expect("tempdir");
    let wal = Arc::new(RedbWalProvider::new(temp.path()));
    let state_store = Arc::new(
        RedbStateStoreProvider::new(temp.path().join("corrupt-state.redb")).expect("state store"),
    );
    state_store
        .set(source_id, "effective-repos", b"{invalid".to_vec())
        .await
        .expect("seed corrupted state");

    let core = DrasiLib::builder()
        .with_id("github-source-corrupt-effective-core")
        .with_source(source)
        .with_state_store_provider(state_store.clone())
        .with_wal_provider(wal)
        .build()
        .await
        .expect("build drasi");

    let err = core.start().await.expect_err("start should fail");
    let err_text = format!("{err:#}");
    assert!(err_text.contains("Failed to load persisted effective repositories"));

    let persisted = state_store
        .get(source_id, "effective-repos")
        .await
        .expect("read persisted state")
        .expect("state present");
    assert_eq!(persisted, b"{invalid".to_vec());
}

#[tokio::test]
async fn start_fails_fast_when_loading_effective_repos_errors() {
    let source_id = "github-source-faulty-effective-repos";
    let source = GitHubSourceBuilder::new(source_id)
        .with_config(valid_config_with_port(0))
        .build()
        .expect("build source");

    let temp = TempDir::new().expect("tempdir");
    let wal = Arc::new(RedbWalProvider::new(temp.path()));
    let durable_inner: Arc<dyn StateStoreProvider> = Arc::new(
        RedbStateStoreProvider::new(temp.path().join("faulty-state.redb")).expect("state store"),
    );
    let faulty_store: Arc<dyn StateStoreProvider> = Arc::new(FaultyStateStoreProvider {
        inner: durable_inner,
        fail_store: source_id.to_string(),
        fail_key: "effective-repos".to_string(),
        fail_get: true,
        fail_set: false,
    });

    let core = DrasiLib::builder()
        .with_id("github-source-faulty-effective-core")
        .with_source(source)
        .with_state_store_provider(faulty_store)
        .with_wal_provider(wal)
        .build()
        .await
        .expect("build drasi");

    let err = core.start().await.expect_err("start should fail");
    let err_text = format!("{err:#}");
    assert!(err_text.contains("Failed to load persisted effective repositories"));
    assert!(err_text.contains("effective-repos"));
}

#[tokio::test]
async fn stop_aborts_hung_graphql_task_and_allows_listener_restart() {
    #[derive(Clone, Default)]
    struct HungState {
        request_started: Arc<Notify>,
    }

    async fn hung_handler(
        State(state): State<HungState>,
        Json(_payload): Json<serde_json::Value>,
    ) -> Json<serde_json::Value> {
        state.request_started.notify_one();
        std::future::pending().await
    }

    let hung_state = HungState::default();
    let graphql_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind GraphQL server");
    let graphql_addr = graphql_listener.local_addr().expect("GraphQL addr");
    let server_state = hung_state.clone();
    let graphql_server = tokio::spawn(async move {
        let _ = axum::serve(
            graphql_listener,
            Router::new()
                .route("/graphql", post(hung_handler))
                .with_state(server_state),
        )
        .await;
    });

    let webhook_port = find_available_port().await;
    let mut config = valid_config_with_port(webhook_port);
    config.graphql_url = format!("http://{graphql_addr}/graphql");
    let source = GitHubSourceBuilder::new("github-hung-stop")
        .with_config(config)
        .build()
        .expect("build source");
    let temp = TempDir::new().expect("tempdir");
    let core = DrasiLib::builder()
        .with_id("github-hung-stop-core")
        .with_source(source)
        .with_state_store_provider(Arc::new(
            RedbStateStoreProvider::new(temp.path().join("state.redb")).expect("state store"),
        ))
        .with_wal_provider(Arc::new(RedbWalProvider::new(temp.path())))
        .build()
        .await
        .expect("build core");
    core.start().await.expect("start core");

    let body = br#"{"action":"edited","issue":{"node_id":"I_hung"},"repository":{"full_name":"acme/repo"}}"#;
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(b"secret").expect("hmac");
    mac.update(body);
    let response = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{webhook_port}/webhook"))
        .header(
            "X-Hub-Signature-256",
            format!("sha256={}", hex::encode(mac.finalize().into_bytes())),
        )
        .header("X-GitHub-Delivery", "hung-delivery")
        .header("X-GitHub-Event", "issues")
        .body(body.as_slice().to_vec())
        .send()
        .await
        .expect("send webhook");
    assert!(response.status().is_success());
    tokio::time::timeout(
        Duration::from_secs(2),
        hung_state.request_started.notified(),
    )
    .await
    .expect("hung GraphQL request should start");

    tokio::time::timeout(Duration::from_secs(8), core.stop())
        .await
        .expect("stop must be bounded")
        .expect("stop core");
    drasi_lib::wait_for_status(
        &core.component_graph(),
        "github-hung-stop",
        &[drasi_lib::channels::ComponentStatus::Stopped],
        Duration::from_secs(2),
    )
    .await
    .expect("stopped status must be observed");
    core.start()
        .await
        .expect("listener must restart on the same port");
    tokio::time::timeout(Duration::from_secs(8), core.stop())
        .await
        .expect("second stop must be bounded")
        .expect("second stop");
    graphql_server.abort();
    let _ = graphql_server.await;
}

#[tokio::test]
async fn hydrator_null_node_for_non_delete_action_returns_error() {
    async fn handler(Json(_payload): Json<serde_json::Value>) -> Json<serde_json::Value> {
        Json(json!({ "data": { "node": null } }))
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");
    let app = Router::new().route("/graphql", post(handler));
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let temp = TempDir::new().expect("tempdir");
    let wal = Arc::new(RedbWalProvider::new(temp.path()));
    wal.register(
        "src",
        DurabilityConfig {
            enabled: true,
            max_events: 32,
            capacity_policy: CapacityPolicy::RejectIncoming,
        }
        .to_wal_config(),
    )
    .await
    .expect("register wal");
    let state_store = Arc::new(MemoryStateStoreProvider::new());
    let base = test_source_base("src");
    let params = HydratorParams {
        source_id: "src".to_string(),
        base,
        wal: wal.clone(),
        state_store: state_store.clone(),
        api_client: Arc::new(
            GitHubGraphQLClient::new(format!("http://{addr}/graphql"), "pat".to_string())
                .expect("client"),
        ),
        projects: vec![ProjectSpec {
            owner: "acme".to_string(),
            number: 1,
        }],
        effective_repos: Arc::new(RwLock::new(HashSet::from(["acme/repo".to_string()]))),
        notify: Arc::new(tokio::sync::Notify::new()),
        health: Arc::new(RwLock::new(HydratorHealth::default())),
        processing_gate: Arc::new(tokio::sync::Mutex::new(())),
        shutdown: tokio::sync::watch::channel(false).1,
    };

    let locator = WebhookLocator {
        event_type: "issues".to_string(),
        action: "edited".to_string(),
        node_id: Some("I_1".to_string()),
        repository_full_name: Some("acme/repo".to_string()),
        parent_issue_id: None,
        parent_pull_request_id: None,
        project_id: None,
        project_owner: None,
        project_number: None,
    };
    let admission = encode_admission_change("src", "delivery-1", &locator).expect("encode");
    let sequence = wal.append("src", &admission).await.expect("append");

    let err = process_admission(&params, sequence, &admission)
        .await
        .expect_err("non-delete null should retry");
    assert!(format!("{err:#}").contains("node=null"));
    assert!(wal.oldest_sequence("src").await.expect("oldest").is_some());
    server.abort();
}

#[tokio::test]
async fn snapshot_delete_cleans_incident_tracks_without_duplicate_or_item_delete() {
    async fn handler(Json(_payload): Json<serde_json::Value>) -> Json<serde_json::Value> {
        Json(json!({
            "data": { "node": null },
            "errors": [{
                "type": "NOT_FOUND",
                "path": ["node"],
                "locations": [{ "line": 2, "column": 3 }],
                "message": "Could not resolve to a node with the global id of 'I_1'"
            }]
        }))
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");
    let app = Router::new().route("/graphql", post(handler));
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let temp = TempDir::new().expect("tempdir");
    let wal = Arc::new(RedbWalProvider::new(temp.path()));
    wal.register(
        "src",
        DurabilityConfig {
            enabled: true,
            max_events: 32,
            capacity_policy: CapacityPolicy::RejectIncoming,
        }
        .to_wal_config(),
    )
    .await
    .expect("register wal");
    let state_store = Arc::new(MemoryStateStoreProvider::new());
    let base = test_source_base("src");
    let mut receiver = base
        .create_streaming_receiver()
        .await
        .expect("create event receiver");
    let params = HydratorParams {
        source_id: "src".to_string(),
        base,
        wal: wal.clone(),
        state_store: state_store.clone(),
        api_client: Arc::new(
            GitHubGraphQLClient::new(format!("http://{addr}/graphql"), "pat".to_string())
                .expect("client"),
        ),
        projects: vec![ProjectSpec {
            owner: "acme".to_string(),
            number: 1,
        }],
        effective_repos: Arc::new(RwLock::new(HashSet::from(["acme/repo".to_string()]))),
        notify: Arc::new(tokio::sync::Notify::new()),
        health: Arc::new(RwLock::new(HydratorHealth::default())),
        processing_gate: Arc::new(tokio::sync::Mutex::new(())),
        shutdown: tokio::sync::watch::channel(false).1,
    };

    let previous_snapshot_key = "root-snapshot:I_1".to_string();
    let (_, previous_snapshot) = map_root_diff(
        "src",
        &FetchedRoot::Issue(sample_issue("existing")),
        None,
        1_000,
    )
    .expect("snapshot");
    save_root_snapshot(
        state_store.as_ref(),
        "src",
        &previous_snapshot_key,
        &previous_snapshot,
    )
    .await
    .expect("save previous");
    let mut reconcile_index = previous_snapshot.elements.clone();
    reconcile_index.insert(
        "PVTI_1".to_string(),
        SnapshotElement {
            element_type: "node".to_string(),
            id: "PVTI_1".to_string(),
            labels: vec!["GitHubProjectItem".to_string()],
            properties: json!({}),
            in_node_id: None,
            out_node_id: None,
        },
    );
    reconcile_index.insert(
        "TRACKS:PVTI_1:I_1".to_string(),
        SnapshotElement {
            element_type: "relation".to_string(),
            id: "TRACKS:PVTI_1:I_1".to_string(),
            labels: vec!["TRACKS".to_string()],
            properties: json!({}),
            in_node_id: Some("I_1".to_string()),
            out_node_id: Some("PVTI_1".to_string()),
        },
    );
    crate::hydrator::save_reconcile_index(state_store.as_ref(), "src", &reconcile_index)
        .await
        .expect("seed reconcile index");

    let locator = WebhookLocator {
        event_type: "issues".to_string(),
        action: "deleted".to_string(),
        node_id: Some("I_1".to_string()),
        repository_full_name: Some("acme/repo".to_string()),
        parent_issue_id: None,
        parent_pull_request_id: None,
        project_id: None,
        project_owner: None,
        project_number: None,
    };
    let admission = encode_admission_change("src", "delivery-delete", &locator).expect("encode");
    let sequence = wal.append("src", &admission).await.expect("append");
    process_admission(&params, sequence, &admission)
        .await
        .expect("delete path");

    assert!(wal.oldest_sequence("src").await.expect("oldest").is_none());
    let tombstone = load_root_snapshot(state_store.as_ref(), "src", &previous_snapshot_key)
        .await
        .expect("load snapshot")
        .expect("snapshot exists");
    assert!(tombstone.elements.is_empty());
    assert_eq!(
        tombstone.committed_delivery_id.as_deref(),
        Some("delivery-delete")
    );
    let updated_index = crate::hydrator::load_reconcile_index(state_store.as_ref(), "src")
        .await
        .expect("load updated reconcile index");
    assert!(!updated_index.contains_key("I_1"));
    assert!(!updated_index.contains_key("TRACKS:PVTI_1:I_1"));
    assert!(updated_index.contains_key("PVTI_1"));

    let mut delete_counts = HashMap::new();
    while let Ok(Ok(event)) = tokio::time::timeout(Duration::from_millis(20), receiver.recv()).await
    {
        if let drasi_lib::channels::SourceEvent::Change(SourceChange::Delete { metadata }) =
            &event.event
        {
            *delete_counts
                .entry(metadata.reference.element_id.as_ref().to_string())
                .or_insert(0usize) += 1;
        }
    }
    assert_eq!(delete_counts.get("I_1"), Some(&1));
    assert_eq!(delete_counts.get("TRACKS:PVTI_1:I_1"), Some(&1));
    assert!(!delete_counts.contains_key("PVTI_1"));
    assert!(delete_counts.values().all(|count| *count == 1));
    server.abort();
}

#[tokio::test]
async fn hydrator_delete_uses_reconcile_index_when_root_snapshot_missing() {
    async fn handler(Json(_payload): Json<serde_json::Value>) -> Json<serde_json::Value> {
        Json(json!({ "data": { "node": null } }))
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");
    let app = Router::new().route("/graphql", post(handler));
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let temp = TempDir::new().expect("tempdir");
    let wal = Arc::new(RedbWalProvider::new(temp.path()));
    wal.register(
        "src",
        DurabilityConfig {
            enabled: true,
            max_events: 32,
            capacity_policy: CapacityPolicy::RejectIncoming,
        }
        .to_wal_config(),
    )
    .await
    .expect("register wal");
    let state_store = Arc::new(MemoryStateStoreProvider::new());
    let base = test_source_base("src");
    let params = HydratorParams {
        source_id: "src".to_string(),
        base,
        wal: wal.clone(),
        state_store: state_store.clone(),
        api_client: Arc::new(
            GitHubGraphQLClient::new(format!("http://{addr}/graphql"), "pat".to_string())
                .expect("client"),
        ),
        projects: vec![],
        effective_repos: Arc::new(RwLock::new(HashSet::from(["acme/repo".to_string()]))),
        notify: Arc::new(tokio::sync::Notify::new()),
        health: Arc::new(RwLock::new(HydratorHealth::default())),
        processing_gate: Arc::new(tokio::sync::Mutex::new(())),
        shutdown: tokio::sync::watch::channel(false).1,
    };

    let issue = sample_issue("existing");
    let mut reconcile_snapshot = ReconcileSnapshot::default();
    reconcile_snapshot.repositories.insert(
        issue.repository.id.clone(),
        RepositoryData {
            id: issue.repository.id.clone(),
            name: "repo".to_string(),
            name_with_owner: issue.repository.name_with_owner.clone(),
            owner: OwnerRef {
                login: "acme".to_string(),
            },
            description: None,
            url: "https://github.com/acme/repo".to_string(),
            is_archived: false,
            is_private: false,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            default_branch_ref: None,
        },
    );
    reconcile_snapshot
        .issues
        .insert(issue.id.clone(), issue.clone());
    for comment in &issue.comments.nodes {
        reconcile_snapshot
            .issue_comments
            .insert(comment.id.clone(), comment.clone());
    }
    let (_, mut reconcile_index) =
        map_reconcile_snapshot("src", &reconcile_snapshot, &HashMap::new(), 1_000);
    reconcile_index.insert(
        "PVTI_1".to_string(),
        SnapshotElement {
            element_type: "node".to_string(),
            id: "PVTI_1".to_string(),
            labels: vec!["GitHubProjectItem".to_string()],
            properties: json!({}),
            in_node_id: None,
            out_node_id: None,
        },
    );
    reconcile_index.insert(
        "TRACKS:PVTI_1:I_1".to_string(),
        SnapshotElement {
            element_type: "relation".to_string(),
            id: "TRACKS:PVTI_1:I_1".to_string(),
            labels: vec!["TRACKS".to_string()],
            properties: json!({}),
            in_node_id: Some("I_1".to_string()),
            out_node_id: Some("PVTI_1".to_string()),
        },
    );
    crate::hydrator::save_reconcile_index(state_store.as_ref(), "src", &reconcile_index)
        .await
        .expect("save reconcile index");

    let locator = WebhookLocator {
        event_type: "issues".to_string(),
        action: "deleted".to_string(),
        node_id: Some("I_1".to_string()),
        repository_full_name: Some("acme/repo".to_string()),
        parent_issue_id: None,
        parent_pull_request_id: None,
        project_id: None,
        project_owner: None,
        project_number: None,
    };
    let admission =
        encode_admission_change("src", "delivery-reconcile-delete", &locator).expect("encode");
    let sequence = wal.append("src", &admission).await.expect("append");
    process_admission(&params, sequence, &admission)
        .await
        .expect("delete should succeed from reconcile index");

    let key = snapshot_key_for_locator(&locator, None);
    let tombstone = load_root_snapshot(state_store.as_ref(), "src", &key)
        .await
        .expect("load root snapshot")
        .expect("tombstone exists");
    assert!(tombstone.elements.is_empty());

    let updated_index = crate::hydrator::load_reconcile_index(state_store.as_ref(), "src")
        .await
        .expect("load updated index");
    assert!(!updated_index.contains_key("I_1"));
    assert!(!updated_index.contains_key("IC_1"));
    assert!(!updated_index.contains_key("COMMENT_ON:IC_1:I_1"));
    assert!(!updated_index.contains_key("IN_REPOSITORY:I_1:R_1"));
    assert!(!updated_index.contains_key("TRACKS:PVTI_1:I_1"));
    assert!(updated_index.contains_key("PVTI_1"));
    assert!(updated_index.contains_key("R_1"));
    assert!(wal.oldest_sequence("src").await.expect("oldest").is_none());
    server.abort();
}

#[tokio::test]
async fn archived_project_item_deletes_from_durable_adjacency_without_hydration() {
    async fn hung_handler(State(calls): State<Arc<AtomicUsize>>) -> Json<serde_json::Value> {
        calls.fetch_add(1, Ordering::SeqCst);
        std::future::pending::<Json<serde_json::Value>>().await
    }

    let graphql_calls = Arc::new(AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind hung GraphQL endpoint");
    let addr = listener.local_addr().expect("local addr");
    let app = Router::new()
        .route("/graphql", post(hung_handler))
        .with_state(graphql_calls.clone());
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let temp = TempDir::new().expect("tempdir");
    let wal = Arc::new(RedbWalProvider::new(temp.path()));
    wal.register(
        "src",
        DurabilityConfig {
            enabled: true,
            max_events: 32,
            capacity_policy: CapacityPolicy::RejectIncoming,
        }
        .to_wal_config(),
    )
    .await
    .expect("register wal");
    let state_store = Arc::new(MemoryStateStoreProvider::new());
    let index = HashMap::from([
        (
            "PVTI_1".to_string(),
            SnapshotElement {
                element_type: "node".to_string(),
                id: "PVTI_1".to_string(),
                labels: vec!["GitHubProjectItem".to_string()],
                properties: json!({}),
                in_node_id: None,
                out_node_id: None,
            },
        ),
        (
            "I_1".to_string(),
            SnapshotElement {
                element_type: "node".to_string(),
                id: "I_1".to_string(),
                labels: vec!["GitHubIssue".to_string()],
                properties: json!({}),
                in_node_id: None,
                out_node_id: None,
            },
        ),
        (
            "IN_PROJECT:PVTI_1:PVT_1".to_string(),
            SnapshotElement {
                element_type: "relation".to_string(),
                id: "IN_PROJECT:PVTI_1:PVT_1".to_string(),
                labels: vec!["IN_PROJECT".to_string()],
                properties: json!({}),
                in_node_id: Some("PVT_1".to_string()),
                out_node_id: Some("PVTI_1".to_string()),
            },
        ),
        (
            "TRACKS:PVTI_1:I_1".to_string(),
            SnapshotElement {
                element_type: "relation".to_string(),
                id: "TRACKS:PVTI_1:I_1".to_string(),
                labels: vec!["TRACKS".to_string()],
                properties: json!({}),
                in_node_id: Some("I_1".to_string()),
                out_node_id: Some("PVTI_1".to_string()),
            },
        ),
    ]);
    assert!(!index.contains_key("PVT_1"));
    crate::hydrator::save_reconcile_index(state_store.as_ref(), "src", &index)
        .await
        .expect("seed index");
    let locator = parse_locator(
        "projects_v2_item",
        br#"{"action":"archived","projects_v2_item":{"node_id":"PVTI_1","project_node_id":"PVT_1"}}"#,
    )
    .expect("parse archived project item webhook locator");
    let (_, item_snapshot) = map_root_diff(
        "src",
        &FetchedRoot::ProjectItem(sample_project_item()),
        None,
        1_000,
    )
    .expect("map standalone project item snapshot");
    save_root_snapshot(
        state_store.as_ref(),
        "src",
        &snapshot_key_for_locator(&locator, None),
        &item_snapshot,
    )
    .await
    .expect("save standalone project item snapshot");
    let base = test_source_base("src");
    let mut receiver = base
        .create_streaming_receiver()
        .await
        .expect("create event receiver");
    let params = HydratorParams {
        source_id: "src".to_string(),
        base,
        wal: wal.clone(),
        state_store: state_store.clone(),
        api_client: Arc::new(
            GitHubGraphQLClient::new(format!("http://{addr}/graphql"), "pat".to_string())
                .expect("client"),
        ),
        projects: vec![ProjectSpec {
            owner: "acme".to_string(),
            number: 1,
        }],
        effective_repos: Arc::new(RwLock::new(HashSet::new())),
        notify: Arc::new(Notify::new()),
        health: Arc::new(RwLock::new(HydratorHealth::default())),
        processing_gate: Arc::new(tokio::sync::Mutex::new(())),
        shutdown: tokio::sync::watch::channel(false).1,
    };
    let admission =
        encode_admission_change("src", "delivery-project-archived", &locator).expect("encode");
    let sequence = wal.append("src", &admission).await.expect("append");

    tokio::time::timeout(
        Duration::from_millis(250),
        process_admission(&params, sequence, &admission),
    )
    .await
    .expect("archived item deletion must not wait on GraphQL")
    .expect("archived item is an immediate authoritative removal");

    let updated = crate::hydrator::load_reconcile_index(state_store.as_ref(), "src")
        .await
        .expect("load index");
    assert!(!updated.contains_key("PVTI_1"));
    assert!(!updated.contains_key("IN_PROJECT:PVTI_1:PVT_1"));
    assert!(!updated.contains_key("TRACKS:PVTI_1:I_1"));
    assert!(updated.contains_key("I_1"));
    assert_eq!(graphql_calls.load(Ordering::SeqCst), 0);
    assert!(wal.oldest_sequence("src").await.expect("oldest").is_none());

    let mut delete_counts = HashMap::new();
    while let Ok(Ok(event)) = tokio::time::timeout(Duration::from_millis(20), receiver.recv()).await
    {
        if let drasi_lib::channels::SourceEvent::Change(SourceChange::Delete { metadata }) =
            &event.event
        {
            *delete_counts
                .entry(metadata.reference.element_id.as_ref().to_string())
                .or_insert(0usize) += 1;
        }
    }
    assert_eq!(delete_counts.get("PVTI_1"), Some(&1));
    assert_eq!(delete_counts.get("IN_PROJECT:PVTI_1:PVT_1"), Some(&1));
    assert_eq!(delete_counts.get("TRACKS:PVTI_1:I_1"), Some(&1));
    assert!(!delete_counts.contains_key("PVT_1"));
    assert!(!delete_counts.contains_key("I_1"));
    assert!(delete_counts.values().all(|count| *count == 1));
    server.abort();
}

#[tokio::test]
async fn hydrator_project_scope_resolution_uses_authoritative_project_identity() {
    async fn handler(Json(payload): Json<serde_json::Value>) -> Json<serde_json::Value> {
        let id = payload
            .get("variables")
            .and_then(|v| v.get("id"))
            .and_then(|v| v.as_str())
            .unwrap_or_default();

        let response = if id == "PVTI_1" {
            json!({
                "data": {
                    "node": {
                        "__typename": "ProjectV2Item",
                        "id": "PVTI_1",
                        "type": "ISSUE",
                        "createdAt": "2026-01-01T00:00:00Z",
                        "updatedAt": "2026-01-01T00:00:00Z",
                        "project": {
                            "id": "PVT_1",
                            "number": 1,
                            "owner": { "login": "acme" }
                        },
                        "content": null,
                        "fieldValues": {
                            "pageInfo": { "hasNextPage": false, "endCursor": null },
                            "nodes": []
                        }
                    }
                }
            })
        } else {
            json!({
                "data": {
                    "node": {
                        "__typename": "ProjectV2Item",
                        "id": "PVTI_999",
                        "type": "ISSUE",
                        "createdAt": "2026-01-01T00:00:00Z",
                        "updatedAt": "2026-01-01T00:00:00Z",
                        "project": {
                            "id": "PVT_999",
                            "number": 999,
                            "owner": { "login": "other-org" }
                        },
                        "content": null,
                        "fieldValues": {
                            "pageInfo": { "hasNextPage": false, "endCursor": null },
                            "nodes": []
                        }
                    }
                }
            })
        };
        Json(response)
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");
    let app = Router::new().route("/graphql", post(handler));
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let temp = TempDir::new().expect("tempdir");
    let wal = Arc::new(RedbWalProvider::new(temp.path()));
    wal.register(
        "src",
        DurabilityConfig {
            enabled: true,
            max_events: 32,
            capacity_policy: CapacityPolicy::RejectIncoming,
        }
        .to_wal_config(),
    )
    .await
    .expect("register wal");
    let state_store = Arc::new(MemoryStateStoreProvider::new());

    let params = HydratorParams {
        source_id: "src".to_string(),
        base: test_source_base("src"),
        wal: wal.clone(),
        state_store: state_store.clone(),
        api_client: Arc::new(
            GitHubGraphQLClient::new(format!("http://{addr}/graphql"), "pat".to_string())
                .expect("client"),
        ),
        projects: vec![ProjectSpec {
            owner: "acme".to_string(),
            number: 1,
        }],
        effective_repos: Arc::new(RwLock::new(HashSet::new())),
        notify: Arc::new(tokio::sync::Notify::new()),
        health: Arc::new(RwLock::new(HydratorHealth::default())),
        processing_gate: Arc::new(tokio::sync::Mutex::new(())),
        shutdown: tokio::sync::watch::channel(false).1,
    };

    let configured_locator = WebhookLocator {
        event_type: "projects_v2_item".to_string(),
        action: "edited".to_string(),
        node_id: Some("PVTI_1".to_string()),
        repository_full_name: None,
        parent_issue_id: None,
        parent_pull_request_id: None,
        project_id: Some("PVT_1".to_string()),
        project_owner: None,
        project_number: None,
    };
    let configured_admission =
        encode_admission_change("src", "delivery-project-configured", &configured_locator)
            .expect("encode");
    let configured_sequence = wal
        .append("src", &configured_admission)
        .await
        .expect("append");
    process_admission(&params, configured_sequence, &configured_admission)
        .await
        .expect("configured project should pass");

    let configured_key = snapshot_key_for_locator(&configured_locator, None);
    let configured_snapshot = load_root_snapshot(state_store.as_ref(), "src", &configured_key)
        .await
        .unwrap();
    assert!(configured_snapshot.is_some());

    let unconfigured_locator = WebhookLocator {
        event_type: "projects_v2_item".to_string(),
        action: "edited".to_string(),
        node_id: Some("PVTI_999".to_string()),
        repository_full_name: None,
        parent_issue_id: None,
        parent_pull_request_id: None,
        project_id: Some("PVT_999".to_string()),
        project_owner: None,
        project_number: None,
    };
    let unconfigured_admission = encode_admission_change(
        "src",
        "delivery-project-unconfigured",
        &unconfigured_locator,
    )
    .expect("encode");
    let unconfigured_sequence = wal
        .append("src", &unconfigured_admission)
        .await
        .expect("append");
    process_admission(&params, unconfigured_sequence, &unconfigured_admission)
        .await
        .expect("skip unconfigured project");

    assert!(wal.oldest_sequence("src").await.expect("oldest").is_none());
    server.abort();
}

#[tokio::test]
async fn hydrator_skips_unsupported_event_type() {
    let temp = TempDir::new().expect("tempdir");
    let wal = Arc::new(RedbWalProvider::new(temp.path()));
    wal.register(
        "src",
        DurabilityConfig {
            enabled: true,
            max_events: 32,
            capacity_policy: CapacityPolicy::RejectIncoming,
        }
        .to_wal_config(),
    )
    .await
    .expect("register wal");

    let params = HydratorParams {
        source_id: "src".to_string(),
        base: test_source_base("src"),
        wal: wal.clone(),
        state_store: Arc::new(MemoryStateStoreProvider::new()),
        api_client: Arc::new(
            GitHubGraphQLClient::new("http://127.0.0.1:9/graphql".to_string(), "pat".to_string())
                .expect("client"),
        ),
        projects: vec![],
        effective_repos: Arc::new(RwLock::new(HashSet::new())),
        notify: Arc::new(tokio::sync::Notify::new()),
        health: Arc::new(RwLock::new(HydratorHealth::default())),
        processing_gate: Arc::new(tokio::sync::Mutex::new(())),
        shutdown: tokio::sync::watch::channel(false).1,
    };

    let locator = WebhookLocator {
        event_type: "member".to_string(),
        action: "added".to_string(),
        node_id: None,
        repository_full_name: Some("acme/repo".to_string()),
        parent_issue_id: None,
        parent_pull_request_id: None,
        project_id: None,
        project_owner: None,
        project_number: None,
    };
    let admission =
        encode_admission_change("src", "delivery-unsupported", &locator).expect("encode");
    let sequence = wal.append("src", &admission).await.expect("append");
    process_admission(&params, sequence, &admission)
        .await
        .expect("unsupported event should be skipped");
    assert!(wal.oldest_sequence("src").await.expect("oldest").is_none());
}

#[tokio::test]
async fn hydrator_replay_unpruned_delivery_uses_committed_marker() {
    let temp = TempDir::new().expect("tempdir");
    let wal = Arc::new(RedbWalProvider::new(temp.path()));
    wal.register(
        "src",
        DurabilityConfig {
            enabled: true,
            max_events: 32,
            capacity_policy: CapacityPolicy::RejectIncoming,
        }
        .to_wal_config(),
    )
    .await
    .expect("register wal");
    let state_store = Arc::new(MemoryStateStoreProvider::new());
    let params = HydratorParams {
        source_id: "src".to_string(),
        base: test_source_base("src"),
        wal: wal.clone(),
        state_store: state_store.clone(),
        api_client: Arc::new(
            GitHubGraphQLClient::new("http://127.0.0.1:9/graphql".to_string(), "pat".to_string())
                .expect("client"),
        ),
        projects: vec![],
        effective_repos: Arc::new(RwLock::new(HashSet::from(["acme/repo".to_string()]))),
        notify: Arc::new(tokio::sync::Notify::new()),
        health: Arc::new(RwLock::new(HydratorHealth::default())),
        processing_gate: Arc::new(tokio::sync::Mutex::new(())),
        shutdown: tokio::sync::watch::channel(false).1,
    };

    let locator = WebhookLocator {
        event_type: "issues".to_string(),
        action: "edited".to_string(),
        node_id: Some("I_1".to_string()),
        repository_full_name: Some("acme/repo".to_string()),
        parent_issue_id: None,
        parent_pull_request_id: None,
        project_id: None,
        project_owner: None,
        project_number: None,
    };
    let admission = encode_admission_change("src", "delivery-replay", &locator).expect("encode");
    let sequence = wal.append("src", &admission).await.expect("append");

    let (changes, mut snapshot) = map_root_diff(
        "src",
        &FetchedRoot::Issue(sample_issue("existing")),
        None,
        1_000,
    )
    .expect("map");
    assert!(!changes.is_empty());
    snapshot.committed_delivery_id = Some("delivery-replay".to_string());
    snapshot.committed_sequence = Some(sequence);
    let key = snapshot_key_for_locator(&locator, None);
    save_root_snapshot(state_store.as_ref(), "src", &key, &snapshot)
        .await
        .expect("save committed snapshot");

    process_admission(&params, sequence, &admission)
        .await
        .expect("replay should prune");
    assert!(wal.oldest_sequence("src").await.expect("oldest").is_none());
}

#[tokio::test]
async fn hydrator_replay_without_marker_still_converges_to_latest_state() {
    #[derive(Clone)]
    struct ServerState {
        title: Arc<RwLock<String>>,
    }

    async fn handler(
        State(state): State<ServerState>,
        Json(_payload): Json<serde_json::Value>,
    ) -> Json<serde_json::Value> {
        let title = state.title.read().await.clone();
        Json(json!({
            "data": {
                "node": {
                    "__typename": "Issue",
                    "id": "I_1",
                    "number": 1,
                    "title": title,
                    "body": "body",
                    "state": "OPEN",
                    "createdAt": "2026-01-01T00:00:00Z",
                    "updatedAt": "2026-01-01T00:00:00Z",
                    "closedAt": null,
                    "url": "https://github.com/acme/repo/issues/1",
                    "author": { "login": "octocat" },
                    "repository": { "id": "R_1", "nameWithOwner": "acme/repo" },
                    "assignees": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] },
                    "labels": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] },
                    "comments": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] }
                }
            }
        }))
    }

    let server_state = ServerState {
        title: Arc::new(RwLock::new("initial".to_string())),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");
    let app = Router::new()
        .route("/graphql", post(handler))
        .with_state(server_state.clone());
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let temp = TempDir::new().expect("tempdir");
    let wal = Arc::new(RedbWalProvider::new(temp.path()));
    wal.register(
        "src",
        DurabilityConfig {
            enabled: true,
            max_events: 32,
            capacity_policy: CapacityPolicy::RejectIncoming,
        }
        .to_wal_config(),
    )
    .await
    .expect("register wal");
    let state_store = Arc::new(MemoryStateStoreProvider::new());
    let params = HydratorParams {
        source_id: "src".to_string(),
        base: test_source_base("src"),
        wal: wal.clone(),
        state_store: state_store.clone(),
        api_client: Arc::new(
            GitHubGraphQLClient::new(format!("http://{addr}/graphql"), "pat".to_string())
                .expect("client"),
        ),
        projects: vec![],
        effective_repos: Arc::new(RwLock::new(HashSet::from(["acme/repo".to_string()]))),
        notify: Arc::new(tokio::sync::Notify::new()),
        health: Arc::new(RwLock::new(HydratorHealth::default())),
        processing_gate: Arc::new(tokio::sync::Mutex::new(())),
        shutdown: tokio::sync::watch::channel(false).1,
    };

    let locator = WebhookLocator {
        event_type: "issues".to_string(),
        action: "edited".to_string(),
        node_id: Some("I_1".to_string()),
        repository_full_name: Some("acme/repo".to_string()),
        parent_issue_id: None,
        parent_pull_request_id: None,
        project_id: None,
        project_owner: None,
        project_number: None,
    };

    let admission_1 = encode_admission_change("src", "delivery-converge-1", &locator).unwrap();
    let seq_1 = wal.append("src", &admission_1).await.unwrap();
    process_admission(&params, seq_1, &admission_1)
        .await
        .unwrap();

    let key = snapshot_key_for_locator(&locator, None);
    state_store.delete("src", &key).await.unwrap();

    let admission_2 = encode_admission_change("src", "delivery-converge-2", &locator).unwrap();
    let seq_2 = wal.append("src", &admission_2).await.unwrap();
    process_admission(&params, seq_2, &admission_2)
        .await
        .unwrap();

    *server_state.title.write().await = "updated".to_string();
    let admission_3 = encode_admission_change("src", "delivery-converge-3", &locator).unwrap();
    let seq_3 = wal.append("src", &admission_3).await.unwrap();
    process_admission(&params, seq_3, &admission_3)
        .await
        .unwrap();

    let final_snapshot = load_root_snapshot(state_store.as_ref(), "src", &key)
        .await
        .unwrap()
        .unwrap();
    let issue_props = &final_snapshot.elements["I_1"].properties;
    assert_eq!(issue_props["title"], json!("updated"));
    assert!(wal.oldest_sequence("src").await.unwrap().is_none());
    server.abort();
}

#[tokio::test]
async fn webhook_only_create_updates_index_and_empty_reconcile_emits_delete() {
    async fn handler(Json(payload): Json<serde_json::Value>) -> Json<serde_json::Value> {
        let query = payload["query"].as_str().unwrap_or_default();
        if query.contains("query($id: ID!)") && query.contains("... on Issue") {
            return Json(json!({
                "data": {
                    "node": {
                        "__typename": "Issue",
                        "id": "I_webhook",
                        "number": 7,
                        "title": "Webhook only",
                        "body": null,
                        "state": "OPEN",
                        "createdAt": "2026-01-01T00:00:00Z",
                        "updatedAt": "2026-01-01T00:00:00Z",
                        "closedAt": null,
                        "url": "https://github.com/acme/repo/issues/7",
                        "author": null,
                        "repository": { "id": "R_1", "nameWithOwner": "acme/repo" },
                        "assignees": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] },
                        "labels": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] },
                        "comments": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] }
                    }
                }
            }));
        }
        if query.contains("defaultBranchRef") {
            return Json(json!({
                "data": {
                    "repository": {
                        "id": "R_1",
                        "name": "repo",
                        "nameWithOwner": "acme/repo",
                        "owner": { "login": "acme" },
                        "description": null,
                        "url": "https://github.com/acme/repo",
                        "isArchived": false,
                        "isPrivate": false,
                        "createdAt": "2026-01-01T00:00:00Z",
                        "updatedAt": "2026-01-01T00:00:00Z",
                        "defaultBranchRef": null
                    }
                }
            }));
        }
        if query.contains("issues(first: 100, after: $cursor") {
            return Json(json!({
                "data": { "repository": { "issues": {
                    "pageInfo": { "hasNextPage": false, "endCursor": null },
                    "nodes": []
                }}}
            }));
        }
        if query.contains("pullRequests(first: 100, after: $cursor") {
            return Json(json!({
                "data": { "repository": { "pullRequests": {
                    "pageInfo": { "hasNextPage": false, "endCursor": null },
                    "nodes": []
                }}}
            }));
        }
        Json(json!({ "data": {} }))
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");
    let app = Router::new().route("/graphql", post(handler));
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let temp = TempDir::new().expect("tempdir");
    let wal = Arc::new(RedbWalProvider::new(temp.path()));
    wal.register(
        "src",
        DurabilityConfig {
            enabled: true,
            max_events: 32,
            capacity_policy: CapacityPolicy::RejectIncoming,
        }
        .to_wal_config(),
    )
    .await
    .expect("register wal");
    let state_store = Arc::new(MemoryStateStoreProvider::new());
    let hydration_base = test_source_base("src");
    let reconcile_base = test_source_base("src");
    let mut receiver = reconcile_base
        .create_streaming_receiver()
        .await
        .expect("create event receiver");
    let api_client = Arc::new(
        GitHubGraphQLClient::new(format!("http://{addr}/graphql"), "pat".to_string())
            .expect("client"),
    );
    let effective_repos = Arc::new(RwLock::new(HashSet::from(["acme/repo".to_string()])));
    let processing_gate = Arc::new(tokio::sync::Mutex::new(()));
    let params = HydratorParams {
        source_id: "src".to_string(),
        base: hydration_base,
        wal: wal.clone(),
        state_store: state_store.clone(),
        api_client: api_client.clone(),
        projects: vec![],
        effective_repos: effective_repos.clone(),
        notify: Arc::new(Notify::new()),
        health: Arc::new(RwLock::new(HydratorHealth::default())),
        processing_gate: processing_gate.clone(),
        shutdown: tokio::sync::watch::channel(false).1,
    };
    let locator = WebhookLocator {
        event_type: "issues".to_string(),
        action: "opened".to_string(),
        node_id: Some("I_webhook".to_string()),
        repository_full_name: Some("acme/repo".to_string()),
        parent_issue_id: None,
        parent_pull_request_id: None,
        project_id: None,
        project_owner: None,
        project_number: None,
    };
    let admission =
        encode_admission_change("src", "delivery-webhook-only", &locator).expect("encode");
    let sequence = wal.append("src", &admission).await.expect("append");
    process_admission(&params, sequence, &admission)
        .await
        .expect("hydrate webhook create");

    let index = crate::hydrator::load_reconcile_index(state_store.as_ref(), "src")
        .await
        .expect("load webhook-updated index");
    assert!(index.contains_key("I_webhook"));

    let reconcile_params = ReconcilerParams {
        source_id: "src".to_string(),
        base: reconcile_base,
        state_store: state_store.clone(),
        api_client,
        projects: vec![],
        static_repos: HashSet::from(["acme/repo".to_string()]),
        effective_repos,
        interval_secs: 60,
        run_initial_pass: false,
        processing_gate,
        shutdown: tokio::sync::watch::channel(false).1,
    };
    crate::reconciler::reconcile_once(&reconcile_params)
        .await
        .expect("empty reconcile");

    let mut saw_issue_delete = false;
    while let Ok(Ok(event)) = tokio::time::timeout(Duration::from_millis(20), receiver.recv()).await
    {
        if let drasi_lib::channels::SourceEvent::Change(SourceChange::Delete { metadata }) =
            &event.event
        {
            saw_issue_delete |= metadata.reference.element_id.as_ref() == "I_webhook";
        }
    }
    assert!(
        saw_issue_delete,
        "empty reconcile must emit the missed delete"
    );
    let index = crate::hydrator::load_reconcile_index(state_store.as_ref(), "src")
        .await
        .expect("load reconciled index");
    assert!(!index.contains_key("I_webhook"));
    server.abort();
}

#[tokio::test]
async fn reconcile_index_commit_failure_keeps_webhook_admission_in_wal() {
    async fn handler(Json(_payload): Json<serde_json::Value>) -> Json<serde_json::Value> {
        Json(json!({
            "data": {
                "node": {
                    "__typename": "Issue",
                    "id": "I_1",
                    "number": 1,
                    "title": "Issue",
                    "body": null,
                    "state": "OPEN",
                    "createdAt": "2026-01-01T00:00:00Z",
                    "updatedAt": "2026-01-01T00:00:00Z",
                    "closedAt": null,
                    "url": "https://github.com/acme/repo/issues/1",
                    "author": null,
                    "repository": { "id": "R_1", "nameWithOwner": "acme/repo" },
                    "assignees": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] },
                    "labels": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] },
                    "comments": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] }
                }
            }
        }))
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");
    let app = Router::new().route("/graphql", post(handler));
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let temp = TempDir::new().expect("tempdir");
    let wal = Arc::new(RedbWalProvider::new(temp.path()));
    wal.register(
        "src",
        DurabilityConfig {
            enabled: true,
            max_events: 32,
            capacity_policy: CapacityPolicy::RejectIncoming,
        }
        .to_wal_config(),
    )
    .await
    .expect("register wal");
    let inner: Arc<dyn StateStoreProvider> = Arc::new(MemoryStateStoreProvider::new());
    let state_store: Arc<dyn StateStoreProvider> = Arc::new(FaultyStateStoreProvider {
        inner,
        fail_store: "src".to_string(),
        fail_key: "reconcile-index".to_string(),
        fail_get: false,
        fail_set: true,
    });
    let params = HydratorParams {
        source_id: "src".to_string(),
        base: test_source_base("src"),
        wal: wal.clone(),
        state_store,
        api_client: Arc::new(
            GitHubGraphQLClient::new(format!("http://{addr}/graphql"), "pat".to_string())
                .expect("client"),
        ),
        projects: vec![],
        effective_repos: Arc::new(RwLock::new(HashSet::from(["acme/repo".to_string()]))),
        notify: Arc::new(Notify::new()),
        health: Arc::new(RwLock::new(HydratorHealth::default())),
        processing_gate: Arc::new(tokio::sync::Mutex::new(())),
        shutdown: tokio::sync::watch::channel(false).1,
    };
    let locator = WebhookLocator {
        event_type: "issues".to_string(),
        action: "opened".to_string(),
        node_id: Some("I_1".to_string()),
        repository_full_name: Some("acme/repo".to_string()),
        parent_issue_id: None,
        parent_pull_request_id: None,
        project_id: None,
        project_owner: None,
        project_number: None,
    };
    let admission =
        encode_admission_change("src", "delivery-index-failure", &locator).expect("encode");
    let sequence = wal.append("src", &admission).await.expect("append");
    let err = process_admission(&params, sequence, &admission)
        .await
        .expect_err("index commit must fail admission");
    assert!(format!("{err:#}").contains("reconcile-index"));
    assert_eq!(
        wal.oldest_sequence("src").await.expect("oldest"),
        Some(sequence)
    );
    server.abort();
}

#[tokio::test]
async fn stale_update_before_durable_delete_prunes_only_stale_head() {
    async fn handler(Json(_payload): Json<serde_json::Value>) -> Json<serde_json::Value> {
        Json(json!({
            "data": { "node": null },
            "errors": [{
                "type": "NOT_FOUND",
                "path": ["node"],
                "message": "Could not resolve to a node with the global id of 'I_1'"
            }]
        }))
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");
    let app = Router::new().route("/graphql", post(handler));
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let temp = TempDir::new().expect("tempdir");
    let wal = Arc::new(RedbWalProvider::new(temp.path()));
    wal.register(
        "src",
        DurabilityConfig {
            enabled: true,
            max_events: 32,
            capacity_policy: CapacityPolicy::RejectIncoming,
        }
        .to_wal_config(),
    )
    .await
    .expect("register wal");
    let params = HydratorParams {
        source_id: "src".to_string(),
        base: test_source_base("src"),
        wal: wal.clone(),
        state_store: Arc::new(MemoryStateStoreProvider::new()),
        api_client: Arc::new(
            GitHubGraphQLClient::new(format!("http://{addr}/graphql"), "pat".to_string())
                .expect("client"),
        ),
        projects: vec![],
        effective_repos: Arc::new(RwLock::new(HashSet::from(["acme/repo".to_string()]))),
        notify: Arc::new(Notify::new()),
        health: Arc::new(RwLock::new(HydratorHealth::default())),
        processing_gate: Arc::new(tokio::sync::Mutex::new(())),
        shutdown: tokio::sync::watch::channel(false).1,
    };
    let mut locator = WebhookLocator {
        event_type: "issues".to_string(),
        action: "edited".to_string(),
        node_id: Some("I_1".to_string()),
        repository_full_name: Some("acme/repo".to_string()),
        parent_issue_id: None,
        parent_pull_request_id: None,
        project_id: None,
        project_owner: None,
        project_number: None,
    };
    let stale = encode_admission_change("src", "delivery-stale", &locator).expect("encode stale");
    let stale_sequence = wal.append("src", &stale).await.expect("append stale");
    locator.action = "deleted".to_string();
    let delete =
        encode_admission_change("src", "delivery-delete", &locator).expect("encode delete");
    let delete_sequence = wal.append("src", &delete).await.expect("append delete");

    process_admission(&params, stale_sequence, &stale)
        .await
        .expect("stale head converges to queued delete");
    assert_eq!(
        wal.oldest_sequence("src").await.expect("oldest"),
        Some(delete_sequence),
        "queued delete must remain durable"
    );
    process_admission(&params, delete_sequence, &delete)
        .await
        .expect("authoritative delete");
    assert!(wal.oldest_sequence("src").await.expect("oldest").is_none());
    server.abort();
}

#[tokio::test]
async fn repository_scope_is_rechecked_after_waiting_for_processing_gate() {
    #[derive(Clone, Default)]
    struct ServerState {
        calls: Arc<AtomicUsize>,
    }
    async fn handler(
        State(state): State<ServerState>,
        Json(_payload): Json<serde_json::Value>,
    ) -> Json<serde_json::Value> {
        state.calls.fetch_add(1, Ordering::SeqCst);
        Json(json!({ "data": { "node": null } }))
    }

    let server_state = ServerState::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");
    let app = Router::new()
        .route("/graphql", post(handler))
        .with_state(server_state.clone());
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let temp = TempDir::new().expect("tempdir");
    let wal = Arc::new(RedbWalProvider::new(temp.path()));
    wal.register(
        "src",
        DurabilityConfig {
            enabled: true,
            max_events: 32,
            capacity_policy: CapacityPolicy::RejectIncoming,
        }
        .to_wal_config(),
    )
    .await
    .expect("register wal");
    let processing_gate = Arc::new(tokio::sync::Mutex::new(()));
    let gate_guard = processing_gate.lock().await;
    let effective_repos = Arc::new(RwLock::new(HashSet::from(["acme/repo".to_string()])));
    let params = HydratorParams {
        source_id: "src".to_string(),
        base: test_source_base("src"),
        wal: wal.clone(),
        state_store: Arc::new(MemoryStateStoreProvider::new()),
        api_client: Arc::new(
            GitHubGraphQLClient::new(format!("http://{addr}/graphql"), "pat".to_string())
                .expect("client"),
        ),
        projects: vec![],
        effective_repos: effective_repos.clone(),
        notify: Arc::new(Notify::new()),
        health: Arc::new(RwLock::new(HydratorHealth::default())),
        processing_gate: processing_gate.clone(),
        shutdown: tokio::sync::watch::channel(false).1,
    };
    let locator = WebhookLocator {
        event_type: "issues".to_string(),
        action: "edited".to_string(),
        node_id: Some("I_1".to_string()),
        repository_full_name: Some("acme/repo".to_string()),
        parent_issue_id: None,
        parent_pull_request_id: None,
        project_id: None,
        project_owner: None,
        project_number: None,
    };
    let admission = encode_admission_change("src", "delivery-gate-race", &locator).expect("encode");
    let sequence = wal.append("src", &admission).await.expect("append");
    let task = tokio::spawn(async move { process_admission(&params, sequence, &admission).await });
    tokio::task::yield_now().await;
    effective_repos.write().await.clear();
    drop(gate_guard);

    task.await
        .expect("join hydrator")
        .expect("out-of-scope delivery is skipped");
    assert_eq!(server_state.calls.load(Ordering::SeqCst), 0);
    assert!(wal.oldest_sequence("src").await.expect("oldest").is_none());
    server.abort();
}

#[tokio::test]
async fn fetched_authoritative_repository_mismatch_is_skipped() {
    async fn handler(Json(_payload): Json<serde_json::Value>) -> Json<serde_json::Value> {
        Json(json!({
            "data": {
                "node": {
                    "__typename": "Issue",
                    "id": "I_1",
                    "number": 1,
                    "title": "Transferred",
                    "body": null,
                    "state": "OPEN",
                    "createdAt": "2026-01-01T00:00:00Z",
                    "updatedAt": "2026-01-01T00:00:00Z",
                    "closedAt": null,
                    "url": "https://github.com/other/repo/issues/1",
                    "author": null,
                    "repository": { "id": "R_2", "nameWithOwner": "other/repo" },
                    "assignees": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] },
                    "labels": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] },
                    "comments": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] }
                }
            }
        }))
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");
    let app = Router::new().route("/graphql", post(handler));
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let temp = TempDir::new().expect("tempdir");
    let wal = Arc::new(RedbWalProvider::new(temp.path()));
    wal.register(
        "src",
        DurabilityConfig {
            enabled: true,
            max_events: 32,
            capacity_policy: CapacityPolicy::RejectIncoming,
        }
        .to_wal_config(),
    )
    .await
    .expect("register wal");
    let state_store = Arc::new(MemoryStateStoreProvider::new());
    let params = HydratorParams {
        source_id: "src".to_string(),
        base: test_source_base("src"),
        wal: wal.clone(),
        state_store: state_store.clone(),
        api_client: Arc::new(
            GitHubGraphQLClient::new(format!("http://{addr}/graphql"), "pat".to_string())
                .expect("client"),
        ),
        projects: vec![],
        effective_repos: Arc::new(RwLock::new(HashSet::from(["acme/repo".to_string()]))),
        notify: Arc::new(Notify::new()),
        health: Arc::new(RwLock::new(HydratorHealth::default())),
        processing_gate: Arc::new(tokio::sync::Mutex::new(())),
        shutdown: tokio::sync::watch::channel(false).1,
    };
    let locator = WebhookLocator {
        event_type: "issues".to_string(),
        action: "transferred".to_string(),
        node_id: Some("I_1".to_string()),
        repository_full_name: Some("acme/repo".to_string()),
        parent_issue_id: None,
        parent_pull_request_id: None,
        project_id: None,
        project_owner: None,
        project_number: None,
    };
    let admission =
        encode_admission_change("src", "delivery-transferred", &locator).expect("encode");
    let sequence = wal.append("src", &admission).await.expect("append");
    process_admission(&params, sequence, &admission)
        .await
        .expect("authoritative out-of-scope object is skipped");

    assert!(wal.oldest_sequence("src").await.expect("oldest").is_none());
    assert!(
        crate::hydrator::load_reconcile_index(state_store.as_ref(), "src")
            .await
            .expect("load index")
            .is_empty()
    );
    assert!(load_root_snapshot(
        state_store.as_ref(),
        "src",
        &snapshot_key_for_locator(&locator, None)
    )
    .await
    .expect("load root snapshot")
    .is_none());
    server.abort();
}
