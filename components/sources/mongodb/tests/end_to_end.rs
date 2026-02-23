use anyhow::Result;
use async_trait::async_trait;
use drasi_lib::channels::{ComponentStatus, ResultDiff};
use drasi_lib::context::ReactionRuntimeContext;
use drasi_lib::reactions::{ReactionBase, ReactionBaseParams};
use drasi_lib::{DrasiLib, Query, Reaction};
use drasi_source_mongodb::config::MongoSourceConfig;
use drasi_source_mongodb::MongoSource;
use mongodb::bson::{doc, Document};
use mongodb::Client;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use testcontainers::{core::{ContainerPort, WaitFor}, runners::AsyncRunner, GenericImage, ImageExt};

// Shared results container
type ResultsHandle = Arc<RwLock<Vec<ResultDiff>>>;

// TestReaction: A simple reaction for collecting query results in tests
struct TestReaction {
    base: ReactionBase,
    results: ResultsHandle,
}

impl TestReaction {
    fn build(id: impl Into<String>, queries: Vec<String>) -> Result<(Self, ResultsHandle)> {
        let params = ReactionBaseParams::new(id, queries);
        let results = Arc::new(RwLock::new(Vec::new()));
        Ok((
            Self {
                base: ReactionBase::new(params),
                results: results.clone(),
            },
            results,
        ))
    }
}

#[async_trait]
impl Reaction for TestReaction {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "test"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        HashMap::new()
    }

    fn query_ids(&self) -> Vec<String> {
        self.base.queries.clone()
    }

    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
    }

    async fn initialize(&self, context: ReactionRuntimeContext) {
        self.base.initialize(context).await;
    }

    async fn start(&self) -> Result<()> {
        self.base
            .set_status_with_event(ComponentStatus::Starting, Some("Starting test reaction".to_string()))
            .await?;

        self.base.subscribe_to_queries().await?;

        self.base
            .set_status_with_event(ComponentStatus::Running, Some("Test reaction started".to_string()))
            .await?;

        let priority_queue = self.base.priority_queue.clone();
        let results = self.results.clone();
        let mut shutdown_rx = self.base.create_shutdown_channel().await;

        let processing_task = tokio::spawn(async move {
            loop {
                let query_result_arc = tokio::select! {
                    biased;
                    _ = &mut shutdown_rx => break,
                    result = priority_queue.dequeue() => result,
                };

                // Collect all results
                for result_diff in &query_result_arc.results {
                    results.write().await.push(result_diff.clone());
                }
            }
        });

        self.base.set_processing_task(processing_task).await;
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.base.stop_common().await?;
        self.base
            .set_status_with_event(ComponentStatus::Stopped, Some("Test reaction stopped".to_string()))
            .await?;
        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }
}

async fn wait_for_result(results: &ResultsHandle, min_count: usize) -> Result<()> {
    for _ in 0..20 {
        if results.read().await.len() >= min_count {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Err(anyhow::anyhow!("Timeout waiting for query results"))
}

#[tokio::test]
#[ignore]
async fn test_end_to_end_basic_pipeline() -> Result<()> {
    // 1. Setup Mongo
    let container = GenericImage::new("mongo", "5.0")
        .with_wait_for(WaitFor::message_on_stdout("Waiting for connections"))
        .with_exposed_port(ContainerPort::Tcp(27017))
        .with_cmd(vec!["mongod", "--replSet", "rs0", "--bind_ip_all"])
        .start()
        .await?;
    let port = container.get_host_port_ipv4(27017).await?;
    let connection_string = format!("mongodb://127.0.0.1:{}", port);
    
    let client = Client::with_uri_str(&connection_string).await?;
    let _ = client.database("admin").run_command(doc! { "replSetInitiate": {} }, None).await;
    
    // Wait for primary
    for _ in 0..30 {
        if let Ok(status) = client.database("admin").run_command(doc! { "replSetGetStatus": 1 }, None).await {
            if let Ok(members) = status.get_array("members") {
                if let Some(member) = members.first() {
                    if let Some(state_str) = member.as_document().and_then(|d| d.get_str("stateStr").ok()) {
                        if state_str == "PRIMARY" {
                            break;
                        }
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    let db_name = "e2e_db";
    let collection_name = "users";
    let db = client.database(db_name);
    let collection = db.collection::<Document>(collection_name);

    // 2. Create Source
    let config = MongoSourceConfig {
        connection_string: connection_string.clone(),
        database: db_name.to_string(),
        collections: vec![collection_name.to_string()],
        pipeline: None,
        username: None,
        password: None,
    };
    let source = MongoSource::new("e2e-source", config)?;

    // 3. Create Query
    let query = Query::cypher("e2e-query")
        .query("MATCH (u:users) WHERE u.age > 20 RETURN u.name")
        .from_source("e2e-source")
        .auto_start(true)
        .build();

    // 4. Create TestReaction
    let (test_reaction, results_handle) = TestReaction::build("test-reaction", vec!["e2e-query".to_string()])?;

    // 5. Build DrasiLib
    let core = Arc::new(
        DrasiLib::builder()
            .with_id("e2e-test")
            .with_source(source)
            .with_query(query)
            .with_reaction(test_reaction)
            .build()
            .await?
    );

    // 6. Start the pipeline
    println!("Starting DrasiLib...");
    core.start().await?;
    tokio::time::sleep(Duration::from_secs(3)).await;

    // 7. Test Interactions
    
    // A. Insert matching user (Alice, 25) -> Expect Adding
    println!("Inserting Alice (25)...");
    collection.insert_one(doc! { "name": "Alice", "age": 25 }, None).await?;
    
    wait_for_result(&results_handle, 1).await?;
    let results = results_handle.read().await.clone();
    assert_eq!(results.len(), 1);
    match &results[0] {
        ResultDiff::Add { data } => {
            println!("Result: {:?}", data);
            // Verify content
        },
        _ => panic!("Expected Add result, got {:?}", results[0]),
    }
    results_handle.write().await.clear();
    
    // B. Insert non-matching user (Bob, 15) -> Expect No Result
    println!("Inserting Bob (15)...");
    collection.insert_one(doc! { "name": "Bob", "age": 15 }, None).await?;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let results = results_handle.read().await.clone();
    assert_eq!(results.len(), 0);
    
    // C. Update Alice (age 25 -> 30) -> Expect Updating
    println!("Updating Alice (25 -> 30)...");
    collection.update_one(doc! { "name": "Alice" }, doc! { "$set": { "age": 30 } }, None).await?;
    
    wait_for_result(&results_handle, 1).await?;
    let results = results_handle.read().await.clone();
    assert_eq!(results.len(), 1);
    match &results[0] {
        ResultDiff::Update { .. } => {},
        _ => panic!("Expected Update result, got {:?}", results[0]),
    }
    results_handle.write().await.clear();
    
    // D. Update Alice (age 30 -> 10) -> Expect Removing
    println!("Updating Alice (30 -> 10)...");
    collection.update_one(doc! { "name": "Alice" }, doc! { "$set": { "age": 10 } }, None).await?;
    
    wait_for_result(&results_handle, 1).await?;
    let results = results_handle.read().await.clone();
    assert_eq!(results.len(), 1);
    match &results[0] {
        ResultDiff::Delete { .. } => {},
        _ => panic!("Expected Delete result, got {:?}", results[0]),
    }
    
    core.stop().await?;
    Ok(())
}

#[tokio::test]
#[ignore]
async fn test_end_to_end_resume_persistence() -> Result<()> {
    // 1. Setup Mongo
    let container = GenericImage::new("mongo", "5.0")
        .with_wait_for(WaitFor::message_on_stdout("Waiting for connections"))
        .with_exposed_port(ContainerPort::Tcp(27017))
        .with_cmd(vec!["mongod", "--replSet", "rs0", "--bind_ip_all"])
        .start()
        .await?;
    let port = container.get_host_port_ipv4(27017).await?;
    let connection_string = format!("mongodb://127.0.0.1:{}", port);
    
    let client = Client::with_uri_str(&connection_string).await?;
    let _ = client.database("admin").run_command(doc! { "replSetInitiate": {} }, None).await;
    for _ in 0..30 {
        if let Ok(status) = client.database("admin").run_command(doc! { "replSetGetStatus": 1 }, None).await {
            if let Ok(members) = status.get_array("members") {
                if let Some(member) = members.first() {
                    if let Some(state_str) = member.as_document().and_then(|d| d.get_str("stateStr").ok()) {
                        if state_str == "PRIMARY" { break; }
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    let db_name = "resume_db";
    let collection_name = "resume_col";
    let db = client.database(db_name);
    let collection = db.collection::<Document>(collection_name);

    // 2. Create Source
    let config = MongoSourceConfig {
        connection_string: connection_string.clone(),
        database: db_name.to_string(),
        collections: vec![collection_name.to_string()],
        pipeline: None,
        username: None,
        password: None,
    };
    let source = MongoSource::new("resume-source", config)?;

    // 3. Create Query
    let query = Query::cypher("resume-query")
        .query("MATCH (n:resume_col) RETURN n.val")
        .from_source("resume-source")
        .auto_start(true)
        .build();

    // 4. Create TestReaction
    let (test_reaction, results_handle) = TestReaction::build("test-reaction", vec!["resume-query".to_string()])?;

    // 5. Build DrasiLib with state store
    let state_store = Arc::new(drasi_lib::state_store::MemoryStateStoreProvider::new());
    let core = Arc::new(
        DrasiLib::builder()
            .with_id("resume-test")
            .with_state_store_provider(state_store.clone())
            .with_source(source)
            .with_query(query)
            .with_reaction(test_reaction)
            .build()
            .await?
    );

    // 6. Start, insert, stop
    core.start().await?;
    tokio::time::sleep(Duration::from_secs(3)).await;

    collection.insert_one(doc! { "val": 1 }, None).await?;
    wait_for_result(&results_handle, 1).await?;
    results_handle.write().await.clear();

    println!("Stopping DrasiLib...");
    core.stop().await?;
    tokio::time::sleep(Duration::from_secs(1)).await;

    // 7. Insert while stopped
    println!("Inserting item 2 (offline)...");
    collection.insert_one(doc! { "val": 2 }, None).await?;

    // 8. Recreate and restart
    println!("Restarting DrasiLib...");
    let config2 = MongoSourceConfig {
        connection_string: connection_string.clone(),
        database: db_name.to_string(),
        collections: vec![collection_name.to_string()],
        pipeline: None,
        username: None,
        password: None,
    };
    let source2 = MongoSource::new("resume-source", config2)?;
    let query2 = Query::cypher("resume-query")
        .query("MATCH (n:resume_col) RETURN n.val")
        .from_source("resume-source")
        .auto_start(true)
        .build();
    let (test_reaction2, results_handle2) = TestReaction::build("test-reaction", vec!["resume-query".to_string()])?;

    let core2 = Arc::new(
        DrasiLib::builder()
            .with_id("resume-test")
            .with_state_store_provider(state_store)
            .with_source(source2)
            .with_query(query2)
            .with_reaction(test_reaction2)
            .build()
            .await?
    );

    core2.start().await?;
    tokio::time::sleep(Duration::from_secs(3)).await;

    //9. Verify item 2 is picked up
    wait_for_result(&results_handle2, 1).await?;
    let results = results_handle2.read().await.clone();
    assert_eq!(results.len(), 1);
    
    core2.stop().await?;
    Ok(())
}

#[tokio::test]
#[ignore]
async fn test_end_to_end_multi_collection() -> Result<()> {
    // 1. Setup Mongo
    let container = GenericImage::new("mongo", "5.0")
        .with_wait_for(WaitFor::message_on_stdout("Waiting for connections"))
        .with_exposed_port(ContainerPort::Tcp(27017))
        .with_cmd(vec!["mongod", "--replSet", "rs0", "--bind_ip_all"])
        .start()
        .await?;
    let port = container.get_host_port_ipv4(27017).await?;
    let connection_string = format!("mongodb://127.0.0.1:{}", port);
    
    let client = Client::with_uri_str(&connection_string).await?;
    let _ = client.database("admin").run_command(doc! { "replSetInitiate": {} }, None).await;
    for _ in 0..30 {
        if let Ok(status) = client.database("admin").run_command(doc! { "replSetGetStatus": 1 }, None).await {
            if let Ok(members) = status.get_array("members") {
                if let Some(member) = members.first() {
                    if let Some(state_str) = member.as_document().and_then(|d| d.get_str("stateStr").ok()) {
                        if state_str == "PRIMARY" { break; }
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    let db_name = "multi_col_db";
    let db = client.database(db_name);
    let col1 = db.collection::<Document>("col1");
    let col2 = db.collection::<Document>("col2");
    let col3 = db.collection::<Document>("col3"); // Unwatched

    // 2. Create Source
    let config = MongoSourceConfig {
        connection_string: connection_string.clone(),
        database: db_name.to_string(),
        collections: vec!["col1".to_string(), "col2".to_string()],
        pipeline: None,
        username: None,
        password: None,
    };
    let source = MongoSource::new("multi-source", config)?;

    // 3. Create Query
    let query = Query::cypher("multi-query")
        .query("MATCH (n) WHERE n:col1 OR n:col2 RETURN n.val")
        .from_source("multi-source")
        .auto_start(true)
        .build();

    // 4. Create TestReaction
    let (test_reaction, results_handle) = TestReaction::build("test-reaction", vec!["multi-query".to_string()])?;

    // 5. Build DrasiLib
    let core = Arc::new(
        DrasiLib::builder()
            .with_id("multi-test")
            .with_source(source)
            .with_query(query)
            .with_reaction(test_reaction)
            .build()
            .await?
    );

    // 6. Start the pipeline
    core.start().await?;
    tokio::time::sleep(Duration::from_secs(3)).await;

    // 7. Test Interactions
    println!("Inserting into col1...");
    col1.insert_one(doc! { "val": "from_col1" }, None).await?;
    wait_for_result(&results_handle, 1).await?;
    let results = results_handle.read().await.clone();
    assert_eq!(results.len(), 1);
    results_handle.write().await.clear();

    println!("Inserting into col2...");
    col2.insert_one(doc! { "val": "from_col2" }, None).await?;
    wait_for_result(&results_handle, 1).await?;
    let results = results_handle.read().await.clone();
    assert_eq!(results.len(), 1);
    results_handle.write().await.clear();

    println!("Inserting into col3 (ignored)...");
    col3.insert_one(doc! { "val": "from_col3" }, None).await?;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let results = results_handle.read().await.clone();
    assert_eq!(results.len(), 0);

    core.stop().await?;
    Ok(())
}

#[tokio::test]
#[ignore]
async fn test_end_to_end_authentication() -> Result<()> {
    // 1. Setup Mongo with Auth
    let image = GenericImage::new("mongo", "5.0")
        .with_wait_for(WaitFor::message_on_stdout("Waiting for connections"))
        .with_exposed_port(ContainerPort::Tcp(27017))
        .with_entrypoint("bash")
        .with_env_var("MONGO_INITDB_ROOT_USERNAME", "admin")
        .with_env_var("MONGO_INITDB_ROOT_PASSWORD", "password");
        
    let container = image
        .with_cmd(vec!["-c", "echo \"verysecretkey123\" > /tmp/keyfile && chmod 400 /tmp/keyfile && chown mongodb:mongodb /tmp/keyfile && mongod --replSet rs0 --bind_ip_all --auth --keyFile /tmp/keyfile"])
        .start()
        .await?;
    
    let port = container.get_host_port_ipv4(27017).await?;
    let connection_string_no_auth = format!("mongodb://127.0.0.1:{}", port);
    let connection_string_auth = format!("mongodb://admin:password@127.0.0.1:{}", port);
    
    let client_options = mongodb::options::ClientOptions::parse(&connection_string_auth).await?;
    let client = Client::with_options(client_options)?;
    let _ = client.database("admin").run_command(doc! { "replSetInitiate": {} }, None).await;

    // Wait for primary
    for _ in 0..30 {
        if let Ok(status) = client.database("admin").run_command(doc! { "replSetGetStatus": 1 }, None).await {
            if let Ok(members) = status.get_array("members") {
                if let Some(member) = members.first() {
                    if let Some(state_str) = member.as_document().and_then(|d| d.get_str("stateStr").ok()) {
                        if state_str == "PRIMARY" { break; }
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    let db_name = "auth_test_db";
    let collection_name = "auth_col";
    let db = client.database(db_name);
    let collection = db.collection::<Document>(collection_name);

    // Create user
    client.database("admin").run_command(doc! {
        "createUser": "drasi",
        "pwd": "drasi_password",
        "roles": [ "root" ]
    }, None).await?;

    // 2. Create Source with auth
    let config = MongoSourceConfig {
        connection_string: connection_string_no_auth.clone(),
        database: db_name.to_string(),
        collections: vec![collection_name.to_string()],
        pipeline: None,
        username: Some("drasi".to_string()),
        password: Some("drasi_password".to_string()),
    };
    let source = MongoSource::new("auth-source", config)?;

    // 3. Create Query
    let query = Query::cypher("auth-query")
        .query("MATCH (n:auth_col) RETURN n.msg")
        .from_source("auth-source")
        .auto_start(true)
        .build();

    // 4. Create TestReaction
    let (test_reaction, results_handle) = TestReaction::build("test-reaction", vec!["auth-query".to_string()])?;

    // 5. Build DrasiLib
    let core = Arc::new(
        DrasiLib::builder()
            .with_id("auth-test")
            .with_source(source)
            .with_query(query)
            .with_reaction(test_reaction)
            .build()
            .await?
    );

    // 6. Start the pipeline
    core.start().await?;
    tokio::time::sleep(Duration::from_secs(3)).await;

    // 7. Test Interaction
    println!("Inserting authenticated...");
    collection.insert_one(doc! { "msg": "secure_data" }, None).await?;
    wait_for_result(&results_handle, 1).await?;
    let results = results_handle.read().await.clone();
    assert_eq!(results.len(), 1);

    core.stop().await?;
    Ok(())
}
