// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_core::interface::{CheckpointStore, CreatedIndexes, IndexBackendPlugin, IndexError};
use drasi_lib::schema::{PropertyType, SourceSchema};
use std::sync::Weak;

const JOINED: &str = "joined";
const ACCOUNTS: &str = "accounts";
const CONTACTS: &str = "contacts";
const ACCOUNTS_QUERY: &str = "MATCH (a:Account) RETURN a.Id AS id, a.Name AS name";
const CONTACTS_QUERY: &str = "MATCH (c:Contact) RETURN c.Id AS id, c.Name AS name";
const TIMEOUT: Duration = Duration::from_secs(10);

struct Fixture {
    rest: RestHarness,
    native: PubSubHarness,
    instance_url: String,
    pubsub_endpoint: String,
    servers: Vec<tokio::task::JoinHandle<()>>,
    scan_floor: Mutex<[usize; 2]>,
}

impl Fixture {
    async fn new() -> Result<Self> {
        let rest = RestHarness::default();
        let app = Router::new()
            .route("/oauth/token", post(oauth_token))
            .route(
                "/services/data/v68.0/sobjects/:object/describe",
                get(describe_object),
            )
            .route("/services/data/v68.0/query", get(query_records))
            .route(
                "/services/data/v68.0/query/failed-continuation",
                get(failed_continuation),
            )
            .with_state(rest.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let instance_url = format!("http://{}", listener.local_addr()?);
        let rest_server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("fixture REST server");
        });

        let native = PubSubHarness::new();
        let service = FakePubSub {
            harness: native.clone(),
            header_gate: None,
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let pubsub_endpoint = format!("http://{}", listener.local_addr()?);
        let grpc_server = tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(proto::pub_sub_server::PubSubServer::new(service))
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await
                .expect("fixture Pub/Sub server");
        });
        Ok(Self {
            rest,
            native,
            instance_url,
            pubsub_endpoint,
            servers: vec![rest_server, grpc_server],
            scan_floor: Mutex::new([0, 0]),
        })
    }

    fn source(&self, root: &Path, scan_seconds: u64) -> Result<SalesforceSource> {
        let config = serde_json::from_value(json!({
            "connection": {
                "instanceUrl": self.instance_url,
                "expectedOrgId": ORG_ID,
                "apiVersion": "68.0",
                "auth": {
                    "type": "clientCredentials",
                    "tokenUrl": format!("{}/oauth/token", self.instance_url),
                    "clientId": "fixture",
                    "clientSecret": "fixture"
                }
            },
            "objects": ["Account", "Contact"],
            "spoolRoot": root.join("source"),
            "pubsubEndpoint": self.pubsub_endpoint,
            "fullScanIntervalSeconds": scan_seconds,
            "reconcileIntervalSeconds": 1
        }))?;
        SalesforceSource::builder(SOURCE_ID)
            .with_config(config)
            .build()
    }

    async fn runtime(
        &self,
        root: &Path,
        queries: &[(&str, &str)],
        scan_seconds: u64,
    ) -> Result<DrasiLib> {
        *self.scan_floor.lock().await = [
            self.rest.scans("Account").await,
            self.rest.scans("Contact").await,
        ];
        let mut builder = DrasiLib::builder()
            .with_id("salesforce-metadata-integration")
            .with_source(self.source(root, scan_seconds)?)
            .with_index_provider(
                "persistent",
                Arc::new(RocksDbIndexProvider::new(
                    root.join("queries"),
                    false,
                    false,
                )),
            );
        for (id, query) in queries {
            builder = builder.with_query(persistent_query(id, query));
        }
        let drasi = builder.build().await?;
        if let Err(error) = drasi.start().await {
            tokio::task::yield_now().await;
            let events = futures_util::StreamExt::collect::<Vec<_>>(
                drasi.get_source_events(SOURCE_ID).await?,
            )
            .await;
            drasi.shutdown().await?;
            return Err(anyhow!(error).context(format!("source startup events: {events:?}")));
        }
        Ok(drasi)
    }

    async fn ready(&self) -> Result<()> {
        let floor = *self.scan_floor.lock().await;
        tokio::time::timeout(TIMEOUT, async {
            loop {
                if ["Account", "Contact"]
                    .iter()
                    .all(|object| self.native.receiver_count(object) > 0)
                    && self.rest.scans("Account").await >= floor[0] + 2
                    && self.rest.scans("Contact").await >= floor[1] + 2
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .context("both object observers must subscribe and finish initial scan requests")?;
        Ok(())
    }

    async fn close(mut self) {
        for server in self.servers.drain(..) {
            server.abort();
            let result = server.await;
            assert!(result.is_ok() || result.is_err_and(|error| error.is_cancelled()));
        }
    }

    async fn wait_describes(&self, minimum: usize) -> Result<()> {
        tokio::time::timeout(TIMEOUT, async {
            while self.rest.describe_calls.load(Ordering::Relaxed) < minimum {
                tokio::task::yield_now().await;
            }
        })
        .await
        .context("Describe requests did not arrive")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for server in &self.servers {
            server.abort();
        }
    }
}

fn changed_contact_describe() -> Value {
    json!({
        "name": "Contact",
        "queryable": true,
        "keyPrefix": "003",
        "fields": [
            {"name": "Id", "type": "id", "nillable": false},
            {"name": "Name", "type": "int", "nillable": true},
            {"name": "Enabled__c", "type": "boolean", "nillable": true},
            {"name": "AccountId", "type": "reference", "nillable": true,
             "relationshipName": "LinkedAccount", "referenceTo": ["Account"]},
            {"name": "RelatedId", "type": "reference", "nillable": true,
             "relationshipName": "Related", "referenceTo": ["Account", "Contact", "User"]}
        ]
    })
}

async fn wait_source_schema(drasi: &DrasiLib, previous: &SourceSchema) -> Result<SourceSchema> {
    tokio::time::timeout(TIMEOUT, async {
        loop {
            let schema = drasi
                .get_source_schema(SOURCE_ID)
                .await?
                .context("schema")?;
            if &schema != previous {
                return Ok(schema);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?
}

#[tokio::test(flavor = "current_thread")]
async fn public_schema_inspection_tracks_complete_metadata_refreshes_without_io() -> Result<()> {
    let temp = TempDir::new()?;
    let fixture = Fixture::new().await?;
    let drasi = fixture.runtime(temp.path(), &[], 3600).await?;
    let outcome = async {
        fixture.ready().await?;
        let before = drasi
            .get_source_schema(SOURCE_ID)
            .await?
            .context("source schema")?;
        assert_eq!(before.nodes.len(), 2);
        let relation = before
            .relations
            .iter()
            .find(|r| r.label == "REL_Account")
            .unwrap();
        assert_eq!(relation.from.as_deref(), Some("Contact"));
        assert_eq!(relation.to.as_deref(), Some("Account"));
        assert!(!before.relations.iter().any(|r| r.label == "REL_Owner"));
        let calls = fixture.rest.describe_calls.load(Ordering::Relaxed);
        for _ in 0..20 {
            let mut owned = drasi.get_source_schema(SOURCE_ID).await?.unwrap();
            assert_eq!(owned, before);
            owned.nodes.clear();
            let graph = drasi.get_graph_schema().await?;
            assert!(!graph.sources_without_schema.contains(SOURCE_ID));
            assert_eq!(
                graph.nodes["Contact"]
                    .properties
                    .iter()
                    .find(|property| property.name == "Id")
                    .unwrap()
                    .data_type,
                Some(PropertyType::String)
            );
        }
        assert_eq!(fixture.rest.describe_calls.load(Ordering::Relaxed), calls);

        fixture
            .rest
            .describe_overrides
            .write()
            .await
            .insert("Contact".into(), changed_contact_describe());
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        *fixture.rest.describe_gate.write().await = Some(gate.clone());
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(301)).await;
        tokio::time::resume();
        fixture.wait_describes(calls + 2).await?;
        assert_eq!(
            drasi.get_source_schema(SOURCE_ID).await?,
            Some(before.clone())
        );
        assert_eq!(
            fixture.rest.describe_calls.load(Ordering::Relaxed),
            calls + 2
        );
        gate.add_permits(2);
        let after = wait_source_schema(&drasi, &before).await?;
        *fixture.rest.describe_gate.write().await = None;
        let contact = after.nodes.iter().find(|n| n.label == "Contact").unwrap();
        assert_eq!(
            contact
                .properties
                .iter()
                .find(|p| p.name == "Name")
                .unwrap()
                .data_type,
            Some(PropertyType::Integer)
        );
        assert_eq!(
            contact
                .properties
                .iter()
                .find(|p| p.name == "Enabled__c")
                .unwrap()
                .data_type,
            Some(PropertyType::Boolean)
        );
        assert!(!contact.properties.iter().any(|p| p.name == "Email"));
        assert!(!after.relations.iter().any(|r| r.label == "REL_Account"));
        let graph = drasi.get_graph_schema().await?;
        assert_eq!(
            graph.relations["REL_LinkedAccount"].to.as_deref(),
            Some("Account")
        );
        assert_eq!(
            graph.relations["REL_Related"].from.as_deref(),
            Some("Contact")
        );
        assert_eq!(graph.relations["REL_Related"].to, None);

        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(301)).await;
        tokio::time::resume();
        fixture.wait_describes(calls + 4).await?;
        assert_eq!(drasi.get_source_schema(SOURCE_ID).await?, Some(after));
        Ok::<_, anyhow::Error>(())
    }
    .await;
    drasi.shutdown().await?;
    fixture.close().await;
    outcome
}

#[tokio::test(flavor = "current_thread")]
async fn schema_publication_survives_cancelled_start_without_claiming_running() -> Result<()> {
    let temp = TempDir::new()?;
    let fixture = Fixture::new().await?;
    let source = Arc::new(fixture.source(temp.path(), 3600)?);
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    *fixture.rest.describe_gate.write().await = Some(gate.clone());
    let starting_source = source.clone();
    let start = tokio::spawn(async move { starting_source.start().await });
    fixture.wait_describes(2).await?;
    start.abort();
    assert!(start.await.unwrap_err().is_cancelled());
    assert_eq!(source.describe_schema(), None);
    gate.add_permits(2);
    tokio::time::timeout(TIMEOUT, async {
        while source.status().await != ComponentStatus::Error {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    let published = source
        .describe_schema()
        .context("completed metadata preparation")?;
    assert_eq!(published.nodes.len(), 2);
    source.stop().await?;
    assert_eq!(source.describe_schema(), Some(published));
    source.deprovision().await?;
    assert_eq!(source.describe_schema(), None);
    fixture.close().await;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn schema_snapshot_retains_last_publication_after_report_and_cleanup_failures() -> Result<()>
{
    let temp = TempDir::new()?;
    let fixture = Fixture::new().await?;
    let source = fixture.source(temp.path(), 3600)?;
    source.start().await?;
    source.stop().await?;
    let before = source.describe_schema().context("initial schema")?;
    let fingerprint = source.schema().await?.schema.fingerprint;
    let report_path = temp.path().join("source/schema.json");
    std::fs::remove_file(&report_path)?;
    std::fs::create_dir(&report_path)?;
    fixture
        .rest
        .describe_overrides
        .write()
        .await
        .insert("Contact".into(), changed_contact_describe());
    assert!(source.start().await.is_err());
    assert_eq!(source.describe_schema(), Some(before.clone()));
    assert_ne!(source.schema().await?.schema.fingerprint, fingerprint);
    source.stop().await?;
    std::fs::remove_dir(&report_path)?;
    source.start().await?;
    source.stop().await?;
    let after = source.describe_schema().context("recovered schema")?;
    assert_ne!(after, before);

    let ownership_path = temp.path().join("source/ownership.json");
    let ownership = std::fs::read(&ownership_path)?;
    let mut mismatched: Value = serde_json::from_slice(&ownership)?;
    mismatched["source_id"] = json!("different-owner");
    std::fs::write(&ownership_path, serde_json::to_vec(&mismatched)?)?;
    let error = source.deprovision().await.unwrap_err();
    assert!(format!("{error:#}").contains("not owned"));
    assert_eq!(source.describe_schema(), Some(after));
    std::fs::write(&ownership_path, ownership)?;
    source.deprovision().await?;
    assert_eq!(source.describe_schema(), None);
    fixture.close().await;
    Ok(())
}

struct ObservedRocksIndexes {
    inner: RocksDbIndexProvider,
    checkpoint: Mutex<Option<Weak<dyn CheckpointStore>>>,
}

#[async_trait::async_trait]
impl IndexBackendPlugin for ObservedRocksIndexes {
    async fn create_indexes(
        &self,
        query_id: &str,
    ) -> std::result::Result<CreatedIndexes, IndexError> {
        let indexes = self.inner.create_indexes(query_id).await?;
        if query_id == JOINED {
            *self.checkpoint.lock().await = indexes.checkpoint_store.as_ref().map(Arc::downgrade);
        }
        Ok(indexes)
    }

    fn is_volatile(&self) -> bool {
        self.inner.is_volatile()
    }
}

fn persistent_query(id: &str, query: &str) -> drasi_lib::config::QueryConfig {
    Query::cypher(id)
        .query(query)
        .from_source(SOURCE_ID)
        .enable_bootstrap(true)
        .with_storage_backend(StorageBackendRef::Named("persistent".into()))
        .build()
}

fn sorted(mut rows: Vec<Value>) -> Vec<Value> {
    rows.sort_by_cached_key(Value::to_string);
    rows
}

fn joined(id: &str, contact: &str, account: &str) -> Value {
    json!({"id": id, "contact": contact, "account": account})
}

async fn wait_rows(drasi: &DrasiLib, query: &str, expected: Vec<Value>) -> Result<()> {
    let expected = sorted(expected);
    let mut actual = Vec::new();
    tokio::time::timeout(TIMEOUT, async {
        loop {
            ensure!(
                drasi.get_source_status(SOURCE_ID).await? == ComponentStatus::Running,
                "source failed while awaiting {query}"
            );
            match drasi.get_query_status(query).await? {
                ComponentStatus::Running => {
                    actual = sorted(drasi.get_query_results(query).await?);
                    if actual == expected {
                        return Ok(());
                    }
                }
                ComponentStatus::Starting => {}
                status => anyhow::bail!("query {query} entered {status:?}"),
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .with_context(|| format!("{query}: expected {expected:?}, last snapshot {actual:?}"))?
}

async fn sequence(drasi: &DrasiLib, id: &str) -> Result<u64> {
    let query = drasi
        .query_manager()
        .get_query_instance(id)
        .await
        .map_err(anyhow::Error::msg)?;
    Ok(query.fetch_snapshot().await?.as_of_sequence)
}

#[tokio::test]
async fn regression_native_delete_of_unseen_record_does_not_interrupt_ingestion() -> Result<()> {
    let temp = TempDir::new()?;
    let fixture = Fixture::new().await?;
    let drasi = fixture
        .runtime(temp.path(), &[(ACCOUNTS, ACCOUNTS_QUERY)], 3600)
        .await?;
    let outcome = async {
        fixture.ready().await?;
        fixture.native.emit("Account", "DELETE", &[ID_2]);
        fixture.rest.upsert(ID_1, "Account A").await;
        fixture.native.emit("Account", "CREATE", &[ID_1]);
        wait_rows(
            &drasi,
            ACCOUNTS,
            vec![json!({"id": ID_1, "name": "Account A"})],
        )
        .await?;
        fixture.rest.upsert(ID_1, "Updated").await;
        fixture.native.emit("Account", "UPDATE", &[ID_1]);
        wait_rows(
            &drasi,
            ACCOUNTS,
            vec![json!({"id": ID_1, "name": "Updated"})],
        )
        .await?;
        assert!(fixture
            .rest
            .queries
            .read()
            .await
            .iter()
            .any(|query| parse_ids_from_soql(query)
                .is_some_and(|ids| ids.iter().any(|id| id == ID_2))));
        Ok(())
    }
    .await;
    drasi.shutdown().await?;
    fixture.close().await;
    outcome
}

#[tokio::test]
async fn metadata_graph_target_first_reparent_null_and_owner_delete() -> Result<()> {
    let temp = TempDir::new()?;
    let fixture = Fixture::new().await?;
    let drasi = fixture
        .runtime(
            temp.path(),
            &[
                (JOINED, JOIN_QUERY),
                (ACCOUNTS, ACCOUNTS_QUERY),
                (CONTACTS, CONTACTS_QUERY),
            ],
            3600,
        )
        .await?;
    let outcome =
        async {
            fixture.ready().await?;
            fixture.rest.upsert(ID_1, "Account A").await;
            fixture.rest.upsert(ID_2, "Account B").await;
            fixture.native.emit("Account", "CREATE", &[ID_1, ID_2]);
            wait_rows(
                &drasi,
                ACCOUNTS,
                vec![
                    json!({"id": ID_1, "name": "Account A"}),
                    json!({"id": ID_2, "name": "Account B"}),
                ],
            )
            .await?;

            fixture.rest.contact(CONTACT_1, "Ada", Some(ID_1)).await;
            fixture.native.emit("Contact", "CREATE", &[CONTACT_1]);
            wait_rows(&drasi, JOINED, vec![joined(CONTACT_1, "Ada", "Account A")]).await?;

            fixture.rest.contact(CONTACT_1, "Ada", Some(ID_2)).await;
            fixture.native.emit("Contact", "UPDATE", &[CONTACT_1]);
            wait_rows(&drasi, JOINED, vec![joined(CONTACT_1, "Ada", "Account B")]).await?;

            fixture.rest.contact(CONTACT_1, "Ada", None).await;
            fixture.native.emit("Contact", "UPDATE", &[CONTACT_1]);
            wait_rows(&drasi, JOINED, vec![]).await?;
            wait_rows(
                &drasi,
                CONTACTS,
                vec![json!({"id": CONTACT_1, "name": "Ada"})],
            )
            .await?;

            fixture.rest.contact(CONTACT_1, "Ada", Some(ID_1)).await;
            fixture.native.emit("Contact", "UPDATE", &[CONTACT_1]);
            wait_rows(&drasi, JOINED, vec![joined(CONTACT_1, "Ada", "Account A")]).await?;
            fixture.rest.remove_object("Contact", CONTACT_1).await;
            fixture.native.emit("Contact", "DELETE", &[CONTACT_1]);
            wait_rows(&drasi, JOINED, vec![]).await?;
            wait_rows(&drasi, CONTACTS, vec![]).await?;

            // An owner returning without an FK must not resurrect its previous edge.
            fixture.rest.contact(CONTACT_1, "Ada returned", None).await;
            fixture.native.emit("Contact", "UNDELETE", &[CONTACT_1]);
            wait_rows(
                &drasi,
                CONTACTS,
                vec![json!({"id": CONTACT_1, "name": "Ada returned"})],
            )
            .await?;
            wait_rows(&drasi, JOINED, vec![]).await?;
            let queries = fixture.rest.queries.read().await;
            ensure!(
                queries.iter().any(|query| query_object(query) == "Contact"
                    && parse_ids_from_soql(query).is_some()),
                "Contact changes must be hydrated from native invalidations, not periodic scans"
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
    drasi.shutdown().await?;
    fixture.close().await;
    outcome
}

#[tokio::test]
async fn metadata_graph_owner_first_and_target_deletion_preserve_the_owned_edge() -> Result<()> {
    let temp = TempDir::new()?;
    let fixture = Fixture::new().await?;
    let drasi = fixture
        .runtime(
            temp.path(),
            &[
                (JOINED, JOIN_QUERY),
                (ACCOUNTS, ACCOUNTS_QUERY),
                (CONTACTS, CONTACTS_QUERY),
            ],
            3600,
        )
        .await?;
    let outcome = async {
        fixture.ready().await?;
        fixture.rest.contact(CONTACT_1, "Ada", Some(ID_1)).await;
        fixture.native.emit("Contact", "CREATE", &[CONTACT_1]);
        wait_rows(
            &drasi,
            CONTACTS,
            vec![json!({"id": CONTACT_1, "name": "Ada"})],
        )
        .await?;
        wait_rows(&drasi, JOINED, vec![]).await?;

        fixture.rest.upsert(ID_1, "Late Account").await;
        fixture.native.emit("Account", "CREATE", &[ID_1]);
        wait_rows(
            &drasi,
            JOINED,
            vec![joined(CONTACT_1, "Ada", "Late Account")],
        )
        .await?;
        let contact_requests = fixture
            .rest
            .queries
            .read()
            .await
            .iter()
            .filter(|query| query_object(query) == "Contact")
            .count();

        fixture.rest.remove(ID_1).await;
        fixture.native.emit("Account", "DELETE", &[ID_1]);
        let account_deletion = wait_rows(&drasi, ACCOUNTS, vec![]).await;
        wait_rows(&drasi, JOINED, vec![]).await?;
        wait_rows(
            &drasi,
            CONTACTS,
            vec![json!({"id": CONTACT_1, "name": "Ada"})],
        )
        .await?;

        fixture.rest.upsert(ID_1, "Restored Account").await;
        fixture.native.emit("Account", "UNDELETE", &[ID_1]);
        wait_rows(
            &drasi,
            JOINED,
            vec![joined(CONTACT_1, "Ada", "Restored Account")],
        )
        .await?;
        assert_eq!(
            fixture
                .rest
                .queries
                .read()
                .await
                .iter()
                .filter(|query| query_object(query) == "Contact")
                .count(),
            contact_requests,
            "target restoration must not require a Contact refetch"
        );
        account_deletion?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    drasi.shutdown().await?;
    fixture.close().await;
    outcome
}

#[tokio::test]
async fn metadata_graph_failed_and_partial_scans_never_sweep_committed_records() -> Result<()> {
    let temp = TempDir::new()?;
    let fixture = Fixture::new().await?;
    fixture.rest.upsert(ID_1, "Account A").await;
    fixture.rest.upsert(ID_2, "Account B").await;
    fixture.rest.contact(CONTACT_1, "Ada", Some(ID_1)).await;
    fixture.rest.contact(CONTACT_2, "Grace", Some(ID_2)).await;
    let drasi = fixture
        .runtime(
            temp.path(),
            &[(JOINED, JOIN_QUERY), (ACCOUNTS, ACCOUNTS_QUERY)],
            3600,
        )
        .await?;
    let outcome = async {
        fixture.ready().await?;
        wait_rows(
            &drasi,
            JOINED,
            vec![
                joined(CONTACT_1, "Ada", "Account A"),
                joined(CONTACT_2, "Grace", "Account B"),
            ],
        )
        .await?;
        let account_scans = fixture.rest.scans("Account").await;
        for (failure, name) in [
            (ScanFailure::FirstPage, "After failed first page"),
            (ScanFailure::Continuation, "After failed continuation"),
        ] {
            fixture
                .rest
                .scan_failures
                .write()
                .await
                .insert("Contact".into(), failure);
            let failed_before = fixture.rest.failed_pages.load(Ordering::Relaxed);
            fixture.native.emit("Contact", "GAP_OVERFLOW", &[]);
            tokio::time::timeout(TIMEOUT, async {
                while fixture.rest.failed_pages.load(Ordering::Relaxed) == failed_before {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .context("the requested Contact scan must actually fail")?;

            // This later same-object update is a processing barrier after the failed scan.
            fixture.rest.contact(CONTACT_1, name, Some(ID_1)).await;
            fixture.native.emit("Contact", "UPDATE", &[CONTACT_1]);
            wait_rows(
                &drasi,
                JOINED,
                vec![
                    joined(CONTACT_1, name, "Account A"),
                    joined(CONTACT_2, "Grace", "Account B"),
                ],
            )
            .await?;
        }
        fixture.rest.scan_failures.write().await.clear();
        fixture.rest.remove_object("Contact", CONTACT_2).await;
        fixture.native.emit("Contact", "GAP_OVERFLOW", &[]);
        wait_rows(
            &drasi,
            JOINED,
            vec![joined(CONTACT_1, "After failed continuation", "Account A")],
        )
        .await?;
        wait_rows(
            &drasi,
            ACCOUNTS,
            vec![
                json!({"id": ID_1, "name": "Account A"}),
                json!({"id": ID_2, "name": "Account B"}),
            ],
        )
        .await?;
        assert_eq!(
            fixture.rest.scans("Account").await,
            account_scans,
            "the successful deletion repair must be a Contact-only scan"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    drasi.shutdown().await?;
    fixture.close().await;
    outcome
}

#[tokio::test]
async fn metadata_graph_bootstrap_matches_live_for_labelled_unlabelled_and_mixed_patterns(
) -> Result<()> {
    let temp = TempDir::new()?;
    let fixture = Fixture::new().await?;
    let patterns = [
        JOIN_QUERY,
        "MATCH (c)-[r]->(a) RETURN c.Id AS id, c.Name AS contact, a.Name AS account",
        "MATCH (c:Contact)-[r]->(a) RETURN c.Id AS id, c.Name AS contact, a.Name AS account",
        "MATCH (c)-[:REL_Account]->(a:Account) RETURN c.Id AS id, c.Name AS contact, a.Name AS account",
    ];
    let live_ids = [
        "live-labelled",
        "live-unlabelled",
        "live-mixed-owner",
        "live-mixed-target",
    ];
    let queries = live_ids.into_iter().zip(patterns).collect::<Vec<_>>();
    let drasi = fixture.runtime(temp.path(), &queries, 3600).await?;
    let outcome = async {
        fixture.ready().await?;
        for id in live_ids {
            wait_rows(&drasi, id, vec![]).await?;
        }
        fixture.rest.upsert(ID_1, "Account A").await;
        fixture.rest.contact(CONTACT_1, "Ada", Some(ID_1)).await;
        fixture.native.emit("Contact", "CREATE", &[CONTACT_1]);
        fixture.native.emit("Account", "CREATE", &[ID_1]);
        let expected = vec![joined(CONTACT_1, "Ada", "Account A")];
        for (index, (id, pattern)) in queries.iter().enumerate() {
            wait_rows(&drasi, id, expected.clone()).await?;
            let snapshot_id = format!("snapshot-{index}");
            drasi
                .add_query(persistent_query(&snapshot_id, pattern))
                .await?;
            wait_rows(&drasi, &snapshot_id, expected.clone()).await?;
            assert_eq!(
                sequence(&drasi, &snapshot_id).await?,
                0,
                "a fresh query must obtain its whole result from the default snapshot provider"
            );
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    drasi.shutdown().await?;
    fixture.close().await;
    outcome
}

#[tokio::test]
async fn metadata_graph_retained_edge_metadata_updates_emit_no_unrelated_result_changes(
) -> Result<()> {
    let temp = TempDir::new()?;
    let fixture = Fixture::new().await?;
    fixture.rest.upsert(ID_1, "Account A").await;
    fixture
        .rest
        .put(
            "Contact",
            json!({
                "Id": CONTACT_1, "Name": "Ada", "AccountId": ID_1,
                "Email": "before@example.test"
            }),
        )
        .await;
    let indexes = Arc::new(ObservedRocksIndexes {
        inner: RocksDbIndexProvider::new(temp.path().join("queries"), false, false),
        checkpoint: Mutex::new(None),
    });
    let drasi = DrasiLib::builder()
        .with_source(fixture.source(temp.path(), 3600)?)
        .with_index_provider("persistent", indexes.clone())
        .with_query(persistent_query(JOINED, JOIN_QUERY))
        .with_query(persistent_query(
            "email",
            "MATCH (c:Contact)-[:REL_Account]->(a:Account) RETURN c.Email AS email, a.Name AS account",
        ))
        .build()
        .await?;
    let outcome = async {
        drasi.start().await?;
        fixture.ready().await?;
        let expected = vec![joined(CONTACT_1, "Ada", "Account A")];
        wait_rows(&drasi, JOINED, expected.clone()).await?;
        wait_rows(
            &drasi,
            "email",
            vec![json!({"email": "before@example.test", "account": "Account A"})],
        )
        .await?;
        let query = drasi
            .query_manager()
            .get_query_instance(JOINED)
            .await
            .map_err(anyhow::Error::msg)?;
        let checkpoint_store = indexes
            .checkpoint
            .lock()
            .await
            .as_ref()
            .and_then(Weak::upgrade)
            .context("persistent query checkpoint store")?;
        let before = checkpoint_store
            .read_checkpoint(SOURCE_ID)
            .await?
            .context("completed bootstrap/source checkpoint")?;
        let position = before.source_position.context("local graph position")?;
        let before = drasi_salesforce_common::LocalPosition::decode(&position)?;
        let result_sequence = query.fetch_snapshot().await?.as_of_sequence;

        fixture
            .rest
            .put(
                "Contact",
                json!({
                    "Id": CONTACT_1, "Name": "Ada", "AccountId": ID_1,
                    "Email": "after@example.test"
                }),
            )
            .await;
        fixture.native.emit("Contact", "UPDATE", &[CONTACT_1]);
        wait_rows(
            &drasi,
            "email",
            vec![json!({"email": "after@example.test", "account": "Account A"})],
        )
        .await?;

        // The unchanged projection must process both the node and retained-edge update.
        tokio::time::timeout(TIMEOUT, async {
            loop {
                if checkpoint_store
                    .read_checkpoint(SOURCE_ID)
                    .await?
                    .is_some_and(|checkpoint| checkpoint.sequence >= before.ordinal + 2)
                {
                    return Ok::<_, anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .context("query did not checkpoint the owner and retained-edge metadata updates")??;
        wait_rows(&drasi, JOINED, expected).await?;
        let outbox = query.fetch_outbox(result_sequence).await?;
        assert_eq!(outbox.latest_sequence, result_sequence);
        assert!(
            outbox.results.is_empty(),
            "metadata-only edge updates must not publish user-visible join changes: {:?}",
            outbox.results
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    drasi.shutdown().await?;
    fixture.close().await;
    outcome
}

#[tokio::test]
async fn metadata_graph_persistent_queries_resume_independently_across_restart_and_offline_changes(
) -> Result<()> {
    let temp = TempDir::new()?;
    let fixture = Fixture::new().await?;
    fixture.rest.upsert(ID_1, "Account A").await;
    fixture.rest.upsert(ID_2, "Account B").await;
    fixture.rest.contact(CONTACT_1, "Ada", Some(ID_1)).await;
    fixture.rest.contact(CONTACT_2, "Grace", Some(ID_2)).await;
    let queries = [("first", JOIN_QUERY), ("second", JOIN_QUERY)];
    let initial = vec![
        joined(CONTACT_1, "Ada", "Account A"),
        joined(CONTACT_2, "Grace", "Account B"),
    ];
    let drasi = fixture.runtime(temp.path(), &queries, 3600).await?;
    let first_run = async {
        fixture.ready().await?;
        for (id, _) in queries {
            wait_rows(&drasi, id, initial.clone()).await?;
        }
        Ok::<_, anyhow::Error>([
            sequence(&drasi, "first").await?,
            sequence(&drasi, "second").await?,
        ])
    }
    .await;
    drasi.shutdown().await?;
    drop(drasi);
    let original_sequences = first_run?;

    let drasi = fixture
        .runtime(temp.path(), &queries, 3600)
        .await
        .context("reopening unchanged Salesforce source and persistent query indexes")?;
    let second_run = async {
        fixture.ready().await?;
        for (index, (id, _)) in queries.iter().enumerate() {
            wait_rows(&drasi, id, initial.clone()).await?;
            assert_eq!(
                sequence(&drasi, id).await?,
                original_sequences[index],
                "unchanged restart must not emit duplicate bootstrap inserts"
            );
        }
        drasi.stop_query("second").await?;
        fixture
            .rest
            .contact(CONTACT_1, "Ada online", Some(ID_1))
            .await;
        fixture.native.emit("Contact", "UPDATE", &[CONTACT_1]);
        wait_rows(
            &drasi,
            "first",
            vec![
                joined(CONTACT_1, "Ada online", "Account A"),
                joined(CONTACT_2, "Grace", "Account B"),
            ],
        )
        .await?;
        ensure!(
            sequence(&drasi, "first").await? > original_sequences[0],
            "first query must advance while second remains stopped"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    drasi.shutdown().await?;
    drop(drasi);
    second_run?;

    fixture
        .rest
        .contact(CONTACT_1, "Ada offline", Some(ID_2))
        .await;
    fixture.rest.remove_object("Contact", CONTACT_2).await;
    fixture.rest.remove(ID_1).await;
    let drasi = fixture.runtime(temp.path(), &queries, 3600).await?;
    let third_run = async {
        fixture.ready().await?;
        let expected = vec![joined(CONTACT_1, "Ada offline", "Account B")];
        for (id, _) in queries {
            wait_rows(&drasi, id, expected.clone()).await?;
        }
        drasi
            .add_query(persistent_query("fresh-snapshot", JOIN_QUERY))
            .await?;
        wait_rows(&drasi, "fresh-snapshot", expected).await?;
        assert_eq!(sequence(&drasi, "fresh-snapshot").await?, 0);
        Ok::<_, anyhow::Error>(())
    }
    .await;
    drasi.shutdown().await?;
    drop(drasi);
    fixture.close().await;
    third_run
}

#[tokio::test]
async fn metadata_graph_schema_discovers_scalars_and_reports_omissions_durably() -> Result<()> {
    let temp = TempDir::new()?;
    let fixture = Fixture::new().await?;
    let source = fixture.source(temp.path(), 3600)?;
    assert_eq!(source.describe_schema(), None);
    source.start().await?;
    let report = async {
        fixture.ready().await?;
        let report = source.schema().await?;
        assert!(!report.schema.fingerprint.is_empty());
        assert_eq!(report.pending_records, 0);
        assert_eq!(report.schema.objects.len(), 2);
        let account = report
            .schema
            .objects
            .iter()
            .find(|object| object.object == "Account")
            .context("Account schema")?;
        assert!(account.fields.iter().any(|field| field.name == "Name"));
        assert!(account
            .fields
            .iter()
            .any(|field| field.name == "BillingCity"));
        assert!(account
            .omissions
            .iter()
            .any(|field| field.field == "BillingAddress"));
        let contact = report
            .schema
            .objects
            .iter()
            .find(|object| object.object == "Contact")
            .context("Contact schema")?;
        for name in [
            "FirstName",
            "LastName",
            "Email",
            "MailingCity",
            "AccountId",
            "OwnerId",
        ] {
            assert!(
                contact.fields.iter().any(|field| field.name == name),
                "missing scalar {name}"
            );
        }
        assert!(contact
            .omissions
            .iter()
            .any(|field| field.field == "MailingAddress"));
        assert!(contact
            .omissions
            .iter()
            .any(|field| field.field == "OwnerId"));
        let account_lookup = contact
            .lookups
            .iter()
            .find(|lookup| lookup.field == "AccountId")
            .context("metadata-derived Account lookup")?;
        assert_eq!(account_lookup.label, "REL_Account");
        assert_eq!(account_lookup.selected_targets, vec!["Account"]);
        let queries = fixture.rest.queries.read().await;
        ensure!(queries
            .iter()
            .filter(|query| query_object(query) == "Account")
            .all(|query| query.contains("BillingCity") && !query.contains("BillingAddress")));
        ensure!(queries
            .iter()
            .filter(|query| query_object(query) == "Contact")
            .all(|query| query.contains("MailingCity") && !query.contains("MailingAddress")));
        Ok::<_, anyhow::Error>(report.schema)
    }
    .await;
    source.stop().await?;
    let published = source.describe_schema().context("published graph schema")?;
    drop(source);
    let schema = report?;
    let source = fixture.source(temp.path(), 3600)?;
    assert_eq!(source.describe_schema(), None);
    source.start().await?;
    let persisted = source.schema().await?;
    source.stop().await?;
    assert_eq!(persisted.schema, schema);
    assert_eq!(source.describe_schema(), Some(published));
    source.deprovision().await?;
    assert_eq!(source.describe_schema(), None);
    fixture.close().await;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn regression_invalid_field_refreshes_metadata_without_stopping_ingestion() -> Result<()> {
    invalid_field_refresh(0).await
}

#[tokio::test(flavor = "current_thread")]
async fn invalid_field_refresh_retries_transient_describe_failure() -> Result<()> {
    invalid_field_refresh(1).await
}

async fn invalid_field_refresh(describe_failures: usize) -> Result<()> {
    let temp = TempDir::new()?;
    let fixture = Fixture::new().await?;
    fixture
        .rest
        .put(
            "Account",
            json!({"Id": ID_1, "Name": "Account A", "Phone": "555"}),
        )
        .await;
    let drasi = fixture
        .runtime(
            temp.path(),
            &[(
                ACCOUNTS,
                "MATCH (a:Account) RETURN a.Id AS id, a.Phone AS phone",
            )],
            3600,
        )
        .await?;
    let outcome = async {
        fixture.ready().await?;
        wait_rows(&drasi, ACCOUNTS, vec![json!({"id": ID_1, "phone": "555"})]).await?;
        let before = drasi
            .get_source_schema(SOURCE_ID)
            .await?
            .context("schema before INVALID_FIELD refresh")?;
        assert!(before.nodes.iter().any(|node| {
            node.label == "Account" && node.properties.iter().any(|field| field.name == "Phone")
        }));
        fixture
            .rest
            .reject_removed_phone
            .store(true, Ordering::Relaxed);
        fixture
            .rest
            .describe_failures
            .store(describe_failures, Ordering::Relaxed);
        fixture
            .rest
            .omit_account_phone
            .store(true, Ordering::Relaxed);
        fixture.native.emit("Account", "UPDATE", &[ID_1]);
        wait_rows(&drasi, ACCOUNTS, vec![json!({"id": ID_1, "phone": null})]).await?;
        let after = drasi
            .get_source_schema(SOURCE_ID)
            .await?
            .context("schema after INVALID_FIELD refresh")?;
        assert!(after.nodes.iter().any(|node| {
            node.label == "Account" && node.properties.iter().all(|field| field.name != "Phone")
        }));
        assert!(fixture.rest.invalid_field_responses.load(Ordering::Relaxed) > 0);
        fixture.rest.upsert(ID_2, "Account B").await;
        fixture.native.emit("Account", "CREATE", &[ID_2]);
        wait_rows(
            &drasi,
            ACCOUNTS,
            vec![
                json!({"id": ID_1, "phone": null}),
                json!({"id": ID_2, "phone": null}),
            ],
        )
        .await?;
        Ok(())
    }
    .await;
    drasi.shutdown().await?;
    fixture.close().await;
    outcome
}

#[tokio::test]
async fn invalid_field_with_unchanged_metadata_is_reported_not_retried_forever() -> Result<()> {
    let temp = TempDir::new()?;
    let fixture = Fixture::new().await?;
    let source = fixture.source(temp.path(), 3600)?;
    source.start().await?;
    let published = source.describe_schema();
    let outcome = async {
        fixture.ready().await?;
        fixture
            .rest
            .reject_removed_phone
            .store(true, Ordering::Relaxed);
        fixture.native.emit("Account", "UPDATE", &[ID_1]);
        tokio::time::timeout(TIMEOUT, async {
            while source.status().await != ComponentStatus::Error {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        assert_eq!(
            fixture.rest.invalid_field_responses.load(Ordering::Relaxed),
            1
        );
        assert_eq!(source.describe_schema(), published);
        Ok(())
    }
    .await;
    source.stop().await?;
    fixture.close().await;
    outcome
}

#[tokio::test(flavor = "current_thread")]
async fn metadata_graph_refresh_clears_removed_field_without_record_changes() -> Result<()> {
    let temp = TempDir::new()?;
    let fixture = Fixture::new().await?;
    fixture
        .rest
        .put(
            "Account",
            json!({"Id": ID_1, "Name": "Account A", "Phone": "+1-212-555-0101"}),
        )
        .await;
    fixture.rest.contact(CONTACT_1, "Ada", Some(ID_1)).await;
    let records_before = fixture.rest.rows.read().await.clone();
    let drasi = fixture
        .runtime(
            temp.path(),
            &[(
                JOINED,
                "MATCH (c:Contact)-[:REL_Account]->(a:Account) RETURN c.Name AS contact, a.Name AS account, a.Phone AS phone",
            )],
            3600,
        )
        .await?;
    let outcome = async {
        fixture.ready().await?;
        wait_rows(
            &drasi,
            JOINED,
            vec![json!({
                "contact": "Ada", "account": "Account A", "phone": "+1-212-555-0101"
            })],
        )
        .await?;
        let source_instance = {
            let graph = drasi.component_graph();
            let graph = graph.read().await;
            graph
                .get_runtime::<Arc<dyn Source>>(SOURCE_ID)
                .context("registered Salesforce source")?
                .clone()
        };
        let source = source_instance
            .as_any()
            .downcast_ref::<SalesforceSource>()
            .context("Salesforce source schema inspection")?;
        let before = source.schema().await?;
        let published_before = drasi.get_source_schema(SOURCE_ID).await?;
        assert_eq!(before.pending_records, 0);
        assert!(before.schema.objects.iter().any(|object| {
            object.object == "Account" && object.fields.iter().any(|field| field.name == "Phone")
        }));
        let scans_before = [
            fixture.rest.scans("Account").await,
            fixture.rest.scans("Contact").await,
        ];

        fixture.rest.omit_account_phone.store(true, Ordering::Relaxed);
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(301)).await;
        tokio::time::resume();

        let expected = vec![json!({"contact": "Ada", "account": "Account A", "phone": null})];
        let mut report = before.clone();
        let mut actual = Vec::new();
        tokio::time::timeout(TIMEOUT, async {
            loop {
                ensure!(
                    drasi.get_source_status(SOURCE_ID).await? == ComponentStatus::Running,
                    "source failed during metadata refresh"
                );
                report = source.schema().await?;
                actual = drasi.get_query_results(JOINED).await?;
                if report.schema.fingerprint != before.schema.fingerprint
                    && report.pending_records == 0
                    && actual == expected
                    && report.schema.objects.iter().any(|object| {
                        object.object == "Account"
                            && object.fields.iter().all(|field| field.name != "Phone")
                    })
                {
                    return Ok::<_, anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .with_context(|| {
            format!(
                "metadata-only refresh did not converge: before fingerprint={}, current fingerprint={}, pending_records={}, rows={actual:?}",
                before.schema.fingerprint, report.schema.fingerprint, report.pending_records,
            )
        })??;
        assert!(report.head_ordinal > before.head_ordinal);
        assert!(fixture.rest.scans("Account").await > scans_before[0]);
        assert!(fixture.rest.scans("Contact").await > scans_before[1]);
        assert_eq!(*fixture.rest.rows.read().await, records_before);
        let published_after = drasi.get_source_schema(SOURCE_ID).await?.unwrap();
        assert_ne!(Some(published_after.clone()), published_before);
        assert!(published_after.nodes.iter().any(|node| {
            node.label == "Account" && node.properties.iter().all(|field| field.name != "Phone")
        }));
        let merged = drasi.get_graph_schema().await?;
        assert!(!merged.nodes["Account"]
            .properties
            .iter()
            .any(|property| property.name == "Phone"));
        assert!(!merged.sources_without_schema.contains(SOURCE_ID));
        Ok::<_, anyhow::Error>(())
    }
    .await;
    drasi.shutdown().await?;
    fixture.close().await;
    outcome
}

#[tokio::test]
async fn metadata_graph_rest_only_contacts_still_join_for_both_unavailable_topic_modes(
) -> Result<()> {
    for mode in [TopicMode::NotFound, TopicMode::NotSubscribable] {
        let temp = TempDir::new()?;
        let fixture = Fixture::new().await?;
        fixture
            .native
            .topic_modes
            .write()
            .await
            .insert("Contact".into(), mode);
        fixture.rest.upsert(ID_1, "Account A").await;
        fixture
            .rest
            .contact(CONTACT_1, "REST-only Ada", Some(ID_1))
            .await;
        let drasi = fixture
            .runtime(temp.path(), &[(JOINED, JOIN_QUERY)], 1)
            .await?;
        let outcome = async {
            wait_rows(
                &drasi,
                JOINED,
                vec![joined(CONTACT_1, "REST-only Ada", "Account A")],
            )
            .await?;
            fixture
                .rest
                .contact(CONTACT_1, "Polling update", Some(ID_1))
                .await;
            wait_rows(
                &drasi,
                JOINED,
                vec![joined(CONTACT_1, "Polling update", "Account A")],
            )
            .await?;
            assert_eq!(fixture.native.receiver_count("Contact"), 0);
            ensure!(fixture.native.topic_calls.load(Ordering::Relaxed) >= 2);
            Ok::<_, anyhow::Error>(())
        }
        .await;
        drasi.shutdown().await?;
        fixture.close().await;
        outcome?;
    }
    Ok(())
}

#[tokio::test]
async fn metadata_graph_native_authentication_failures_are_fatal_not_rest_only() -> Result<()> {
    let temp = TempDir::new()?;
    let fixture = Fixture::new().await?;
    fixture
        .native
        .topic_modes
        .write()
        .await
        .insert("Contact".into(), TopicMode::Unauthenticated);
    let source = fixture.source(temp.path(), 1)?;
    source.start().await?;
    let outcome = tokio::time::timeout(TIMEOUT, async {
        while source.status().await != ComponentStatus::Error {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("native authentication failure must become a source error");
    source.stop().await?;
    fixture.close().await;
    outcome
}
