// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, ensure, Context, Result};
use clap::{ArgGroup, Parser};
use drasi_lib::identity::{ApplicationIdentityProvider, Credentials};
use drasi_lib::{ComponentStatus, DrasiLib};
use drasi_salesforce_common::auth::validate_access_token;
use drasi_salesforce_common::{
    sha256_hex, Catalog, SalesforceAuthManager, SalesforcePubSubClient, SalesforceRestClient,
    TopicUnavailable,
};
use drasi_source_salesforce::SalesforceSourceConfig;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{ACCOUNTS_QUERY, CONTACTS_QUERY, COUNT_QUERY, JOIN_QUERY, SOURCE};

pub mod offline;

#[cfg(test)]
mod live_schema;

#[derive(Parser)]
#[command(about = "Watch Salesforce Accounts and Contacts change through Drasi queries")]
#[command(group(ArgGroup::new("mode").required(true).args(["org", "local"])))]
pub struct Args {
    /// Salesforce CLI alias or username. This mode never writes Salesforce data.
    #[arg(long)]
    pub org: Option<String>,
    /// State root; object selections and query definitions get separate v2 namespaces.
    #[arg(long)]
    pub state_dir: Option<PathBuf>,
    /// Periodic REST repair interval; native CDC is processed between scans.
    #[arg(long, default_value_t = 15, value_parser = clap::value_parser!(u64).range(1..))]
    pub full_scan_seconds: u64,
    /// Objects to discover and ingest (default: Account Contact).
    #[arg(long, num_args = 1..)]
    pub objects: Vec<String>,
    /// Print the discovered schema as JSON without ingesting data or creating local state.
    #[arg(long, requires = "org")]
    pub schema: bool,
    /// A Cypher query to run instead of the defaults; repeat for multiple queries.
    #[arg(long, requires = "org", conflicts_with_all = ["schema", "diagnose"])]
    pub query: Vec<String>,
    /// Check CLI authentication, object REST access and native topic access, then exit.
    #[arg(long)]
    pub diagnose: bool,
    /// Run the automated offline protocol demonstration instead of connecting to an org.
    #[arg(long, conflicts_with_all = ["state_dir", "full_scan_seconds", "objects", "schema", "query"])]
    pub local: bool,
}

#[derive(Clone, Serialize)]
pub struct DemoQuery {
    pub id: String,
    pub text: String,
}

pub fn demo_queries(objects: &[String], custom: &[String]) -> Result<Vec<DemoQuery>> {
    if !custom.is_empty() {
        return custom
            .iter()
            .enumerate()
            .map(|(index, text)| {
                ensure!(!text.trim().is_empty(), "Custom query must not be empty");
                Ok(DemoQuery {
                    id: format!("custom-{}", index + 1),
                    text: text.clone(),
                })
            })
            .collect();
    }
    let mut queries = Vec::new();
    let accounts = objects.iter().any(|object| object == "Account");
    let contacts = objects.iter().any(|object| object == "Contact");
    if accounts {
        queries.extend([
            DemoQuery {
                id: "accounts".into(),
                text: ACCOUNTS_QUERY.into(),
            },
            DemoQuery {
                id: "account-count".into(),
                text: COUNT_QUERY.into(),
            },
        ]);
    }
    if contacts {
        queries.push(DemoQuery {
            id: "contacts".into(),
            text: CONTACTS_QUERY.into(),
        });
    }
    if accounts && contacts {
        queries.push(DemoQuery {
            id: "contact-accounts".into(),
            text: JOIN_QUERY.into(),
        });
    }
    let mut other_objects = objects
        .iter()
        .filter(|object| !matches!(object.as_str(), "Account" | "Contact"))
        .collect::<Vec<_>>();
    other_objects.sort();
    for object in other_objects {
        queries.push(DemoQuery {
            id: format!("records-{object}"),
            text: format!("MATCH (n:{object}) RETURN n.Id AS id, n AS record"),
        });
    }
    Ok(queries)
}

#[derive(Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct OrgDetails {
    id: String,
    instance_url: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AccessToken {
    access_token: String,
}

fn decode_cli_response<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    #[derive(Deserialize)]
    struct Envelope<T> {
        result: T,
    }
    let envelope: Envelope<T> = serde_json::from_slice(bytes).map_err(|_| {
        anyhow::anyhow!("Invalid Salesforce CLI response; no credential output logged")
    })?;
    Ok(envelope.result)
}

async fn cli<T: DeserializeOwned>(alias: &str, args: &[&str]) -> Result<T> {
    let output = tokio::time::timeout(
        Duration::from_secs(45),
        tokio::process::Command::new(std::env::var_os("SF_BIN").unwrap_or_else(|| "sf".into()))
            .args(args)
            .args(["--target-org", alias, "--json"])
            .env("SF_DISABLE_TELEMETRY", "true")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("Salesforce CLI timed out")?
    .context("Cannot run Salesforce CLI. Install sf or set SF_BIN to its executable path.")?;
    ensure!(
        output.status.success(),
        "Salesforce CLI authentication failed. Run sf org login web --alias {alias} and retry."
    );
    decode_cli_response(&output.stdout)
}

pub struct Connection {
    pub config: SalesforceSourceConfig,
    pub identity: Arc<ApplicationIdentityProvider>,
    pub query_state: PathBuf,
    pub queries: Vec<DemoQuery>,
    state: PathBuf,
}

pub async fn connect(args: &Args) -> Result<Connection> {
    let alias = args
        .org
        .clone()
        .context("A Salesforce CLI alias is required")?;
    eprintln!("Resolving Salesforce CLI login for {alias} (read-only)...");
    let objects = if args.objects.is_empty() {
        vec!["Account".into(), "Contact".into()]
    } else {
        args.objects.clone()
    };
    let queries = demo_queries(&objects, &args.query)?;
    let org: OrgDetails = cli(&alias, &["org", "display"]).await?;
    ensure!(
        org.instance_url
            .trim_end_matches('/')
            .strip_prefix("https://")
            .is_some_and(|host| host.ends_with(".salesforce.com")),
        "The CLI login must point to an HTTPS Salesforce org"
    );
    let state = args.state_dir.clone().unwrap_or_else(|| {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(".state")
            .join(&org.id)
            .join("graph-v2")
    });
    let mut config: SalesforceSourceConfig = serde_json::from_value(json!({
        "connection": {
            "instanceUrl": org.instance_url,
            "expectedOrgId": org.id,
            "apiVersion": "68.0",
            "auth": {
                "type": "clientCredentials",
                "tokenUrl": format!("{}/services/oauth2/token", org.instance_url.trim_end_matches('/')),
                "clientId": "unused-runtime-identity",
                "clientSecret": "unused-runtime-identity"
            }
        },
        "objects": objects,
        "spoolRoot": state.join("source-v2"),
        "initialCursorBehavior": "earliest",
        "reconcileIntervalSeconds": 1,
        "fullScanIntervalSeconds": args.full_scan_seconds
    }))?;
    config.validate()?;
    let identity_hash = config.source_identity();
    config.spool_root = state
        .join(format!("source-v2-{}", &identity_hash[..16]))
        .to_string_lossy()
        .into_owned();
    let query_hash = sha256_hex(&serde_json::to_vec(&queries)?);
    let query_state = state.join(format!(
        "queries-v2-{}-{}",
        &identity_hash[..16],
        &query_hash[..16]
    ));
    let bound_org = org.clone();
    let identity = Arc::new(ApplicationIdentityProvider::new(move |_| {
        let alias = alias.clone();
        let expected = bound_org.clone();
        async move {
            let current: OrgDetails = cli(&alias, &["org", "display"]).await?;
            ensure!(
                current == expected,
                "Salesforce CLI alias changed org during this run"
            );
            let token: AccessToken = cli(&alias, &["org", "auth", "show-access-token"]).await?;
            validate_access_token(&token.access_token)?;
            ensure!(
                !matches!(token.access_token.as_str(), "[REDACTED]" | "REDACTED"),
                "Salesforce CLI returned a redacted token"
            );
            Ok(Credentials::Token {
                username: "salesforce-example".into(),
                token: token.access_token,
            })
        }
    }));

    eprintln!("Org: {} ({})", org.instance_url, org.id);
    Ok(Connection {
        config,
        identity,
        query_state,
        queries,
        state,
    })
}

pub async fn inspect(connection: &Connection, schema_only: bool) -> Result<()> {
    let config = &connection.config;
    let auth =
        SalesforceAuthManager::new(config.connection.clone(), Some(connection.identity.clone()))?;
    let session = auth.session_token().await?;
    let rest = SalesforceRestClient::new(config.connection.clone())?;
    let mut descriptions = Vec::new();
    for object in &config.objects {
        descriptions.push(rest.describe_object(&session, object).await?);
    }
    let catalog = Catalog::from_describes(descriptions)?;
    if schema_only {
        println!("{}", serde_json::to_string_pretty(&catalog.schema)?);
        return Ok(());
    }
    println!("Discovered query schema (Salesforce field types; --schema prints full JSON):");
    for object in &catalog.schema.objects {
        let fields = object
            .fields
            .iter()
            .map(|field| format!("{}:{}", field.name, field.field_type))
            .collect::<Vec<_>>()
            .join(", ");
        println!("  {}: {fields}", object.object);
        for lookup in &object.lookups {
            println!("  lookup {}", serde_json::to_string(lookup)?);
        }
        for omission in &object.omissions {
            println!("  omitted {}: {}", omission.field, omission.reason);
        }
    }
    let pubsub = SalesforcePubSubClient::connect(&config.pubsub_endpoint).await?;
    for plan in &catalog.plans {
        match pubsub.get_topic(&session, plan).await {
            Ok(topic) => println!(
                "PASS REST {} access and native {}",
                plan.name, topic.topic_name
            ),
            Err(error)
                if error.is::<TopicUnavailable>()
                    || error
                        .downcast_ref::<tonic::Status>()
                        .is_some_and(|status| status.code() == tonic::Code::NotFound) =>
            {
                println!("REST_ONLY {}: {error:#}", plan.name)
            }
            Err(error) => return Err(error).context("Native Salesforce topic check failed"),
        }
    }
    println!("DIAGNOSE_OK (real org; no records or local query state changed)");
    Ok(())
}

pub fn prepare_run(connection: &Connection) -> Result<()> {
    let mut directories = std::fs::DirBuilder::new();
    directories.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        directories.mode(0o700);
    }
    directories.create(&connection.state)?;
    let config = &connection.config;
    println!(
        "Watching all visible records of {}. No value filters or manual joins.",
        config.objects.join(", ")
    );
    println!("Salesforce UI: create an Account and a Contact, choose the Contact's Account, then change or clear that lookup.");
    for query in &connection.queries {
        println!("Query {}: {}", query.id, query.text);
    }
    println!("Source state: {}", config.spool_root);
    println!("Query state: {}", connection.query_state.display());
    println!(
        "Full repair scans run every {}s. Use --schema to see all available fields.",
        config.full_scan_interval_seconds
    );
    println!("Initial sync continues in the background. Ctrl+C stops cleanly; rerun with the same state to resume.");
    Ok(())
}

pub async fn wait_until_ready(drasi: &DrasiLib, queries: &[DemoQuery]) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(60), async {
        while !queries_ready(drasi, queries).await? {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("Queries did not become ready within 60 seconds")?
}

pub async fn queries_ready(drasi: &DrasiLib, queries: &[DemoQuery]) -> Result<bool> {
    ensure!(
        drasi.get_source_status(SOURCE).await? == ComponentStatus::Running,
        "Salesforce source is not running; check the source error above"
    );
    let mut ready = true;
    for query in queries {
        match drasi.get_query_status(&query.id).await? {
            ComponentStatus::Running => {}
            ComponentStatus::Starting => ready = false,
            status => bail!("Query {} entered {status:?}", query.id),
        }
    }
    Ok(ready)
}

pub async fn print_state(
    drasi: &DrasiLib,
    queries: &[DemoQuery],
    previous: Option<&Value>,
) -> Result<Value> {
    let mut state = serde_json::Map::new();
    for query in queries {
        let mut rows = drasi.get_query_results(&query.id).await?;
        rows.sort_by_cached_key(Value::to_string);
        let name = if query.id == "account-count" {
            "count"
        } else {
            &query.id
        };
        state.insert(name.into(), json!(rows));
    }
    let state = Value::Object(state);
    if previous != Some(&state) {
        println!("STATE {state}");
    }
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selecting_a_real_org_never_starts_the_local_demo() {
        let args = Args::try_parse_from(["example", "--org", "developer-org"]).unwrap();
        assert_eq!(args.org.as_deref(), Some("developer-org"));
        assert!(!args.local);
        assert_eq!(args.full_scan_seconds, 15);
    }

    #[test]
    fn mode_must_be_explicit_and_exclusive() {
        assert!(Args::try_parse_from(["example"]).is_err());
        assert!(Args::try_parse_from(["example", "--local", "--org", "dev"]).is_err());
        assert!(Args::try_parse_from(["example", "--local"]).unwrap().local);
        assert!(
            Args::try_parse_from(["example", "--local", "--diagnose"])
                .unwrap()
                .diagnose
        );
        assert!(
            Args::try_parse_from(["example", "--org", "dev", "--full-scan-seconds", "0"]).is_err()
        );
    }

    #[test]
    fn objects_and_schema_are_explicit_real_org_options() {
        let args = Args::try_parse_from([
            "example",
            "--org",
            "dev",
            "--objects",
            "Account",
            "Contact",
            "--schema",
        ])
        .unwrap();
        assert_eq!(args.objects, ["Account", "Contact"]);
        assert!(args.schema);
        assert!(Args::try_parse_from(["example", "--local", "--schema"]).is_err());
        assert!(Args::try_parse_from(["example", "--local", "--objects", "Contact"]).is_err());
    }

    #[test]
    fn malformed_cli_output_never_leaks_credentials() {
        let error = decode_cli_response::<AccessToken>(
            br#"{"result":{"access_token":"private-token-do-not-print"}}"#,
        )
        .err()
        .expect("camelCase accessToken is required");
        assert!(!format!("{error:#}").contains("private-token-do-not-print"));
    }

    #[test]
    fn cli_access_token_is_read_from_its_dedicated_response() {
        let token: AccessToken =
            decode_cli_response(br#"{"result":{"accessToken":"test-session-token"}}"#).unwrap();
        assert_eq!(token.access_token, "test-session-token");
    }

    #[test]
    fn selected_objects_get_real_graph_queries_without_value_filters() {
        let queries = demo_queries(&["Account".into(), "Contact".into()], &[]).unwrap();
        assert_eq!(queries.len(), 4);
        assert!(queries.iter().any(|query| query.text == JOIN_QUERY));
        assert!(queries.iter().all(|query| !query.text.contains("WHERE")));
        let other = demo_queries(&["Opportunity".into()], &[]).unwrap();
        assert_eq!(other.len(), 1);
        assert!(!other[0].text.contains("Account"));
    }

    #[test]
    fn default_queries_preserve_saved_state_namespace() {
        let queries = demo_queries(&["Account".into(), "Contact".into()], &[]).unwrap();
        assert_eq!(
            sha256_hex(&serde_json::to_vec(&queries).unwrap()),
            "7cf35a97df3bc9236c35fb74d3dfea2e41906f3d9867642a18fd8b35249e6978"
        );
    }

    #[test]
    fn custom_queries_replace_defaults_and_reject_empty_input() {
        let queries = demo_queries(
            &["Account".into()],
            &["MATCH (a:Account) RETURN a.Name".into()],
        )
        .unwrap();
        assert_eq!(queries.len(), 1);
        assert_eq!(queries[0].id, "custom-1");
        assert!(demo_queries(&[], &[" ".into()]).is_err());
    }
}
