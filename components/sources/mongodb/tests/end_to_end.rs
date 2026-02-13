use anyhow::Result;
use drasi_core::{
    evaluation::context::QueryPartEvaluationContext,
    query::{ContinuousQuery, QueryBuilder},
};
use drasi_lib::channels::{ChangeDispatcher, ChangeReceiver, SourceEvent, SourceEventWrapper};
use drasi_source_mongodb::config::MongoSourceConfig;
use drasi_source_mongodb::MongoSource;
use drasi_lib::Source;
use mongodb::bson::{doc, Document};
use mongodb::Client;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use testcontainers::{core::{ContainerPort, WaitFor}, runners::AsyncRunner, GenericImage, ImageExt};

// Validates that the source sends events to the query
#[derive(Clone)]
struct QueryDispatcher {
    query: Arc<RwLock<ContinuousQuery>>,
    results: Arc<RwLock<Vec<QueryPartEvaluationContext>>>,
}

impl QueryDispatcher {
    fn new(query: Arc<RwLock<ContinuousQuery>>) -> Self {
        Self {
            query,
            results: Arc::new(RwLock::new(Vec::new())),
        }
    }
    
    async fn get_results(&self) -> Vec<QueryPartEvaluationContext> {
        self.results.read().await.clone()
    }
    
    async fn clear_results(&self) {
        self.results.write().await.clear();
    }
}

#[async_trait::async_trait]
impl ChangeDispatcher<SourceEventWrapper> for QueryDispatcher {
    async fn dispatch_change(&self, event: Arc<SourceEventWrapper>) -> Result<()> {
        if let SourceEvent::Change(change) = &event.event {
            let result = self.query.write().await.process_source_change(change.clone()).await?;
            let mut results = self.results.write().await;
            results.extend(result);
        }
        Ok(())
    }

    async fn create_receiver(&self) -> Result<Box<dyn ChangeReceiver<SourceEventWrapper>>> {
        Err(anyhow::anyhow!("Not implemented for E2E test"))
    }
}

async fn wait_for_result(dispatcher: &QueryDispatcher, min_count: usize) -> Result<()> {
    for _ in 0..20 {
        if dispatcher.get_results().await.len() >= min_count {
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

    // 2. Setup Query
    // Query: MATCH (u:users) WHERE u.age > 20 RETURN u.name
    use drasi_core::evaluation::functions::FunctionRegistry;
    use drasi_query_cypher::CypherParser;
    use drasi_functions_cypher::CypherFunctionSet;

    let query_str = "MATCH (u:users) WHERE u.age > 20 RETURN u.name";
    
    let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
    let parser = Arc::new(CypherParser::new(function_registry.clone()));
    let builder = QueryBuilder::new(query_str, parser)
        .with_function_registry(function_registry);
    
    let query = Arc::new(RwLock::new(builder.build().await));
    let dispatcher = QueryDispatcher::new(query.clone());

    // 3. Setup Source
    let config = MongoSourceConfig {
        connection_string: connection_string.clone(),
        database: Some(db_name.to_string()),
        collection: Some(collection_name.to_string()),
        collections: vec![],
        pipeline: None,
        username: None,
        password: None,
    };

    let source = MongoSource::new("e2e-source", config)?;
    source.base.dispatchers.write().await.push(Box::new(dispatcher.clone()));
    
    let state_store = Arc::new(drasi_lib::state_store::MemoryStateStoreProvider::new());
    let (status_tx, _status_rx) = tokio::sync::mpsc::channel(100);
    let context = drasi_lib::context::SourceRuntimeContext::new(
        "e2e-source",
        status_tx,
        Some(state_store.clone()),
    );
    source.initialize(context).await;

    println!("Starting source...");
    source.start().await?;
    tokio::time::sleep(Duration::from_secs(3)).await;

    // 4. Test Interactions
    
    // A. Insert matching user (Alice, 25) -> Expect Adding
    println!("Inserting Alice (25)...");
    collection.insert_one(doc! { "name": "Alice", "age": 25 }, None).await?;
    
    wait_for_result(&dispatcher, 1).await?;
    let results = dispatcher.get_results().await;
    assert_eq!(results.len(), 1);
    match &results[0] {
        QueryPartEvaluationContext::Adding { after } => {
            println!("Result: {:?}", after);
            // Verify content
        },
        _ => panic!("Expected Adding result, got {:?}", results[0]),
    }
    dispatcher.clear_results().await;
    
    // B. Insert non-matching user (Bob, 15) -> Expect No Result
    println!("Inserting Bob (15)...");
    collection.insert_one(doc! { "name": "Bob", "age": 15 }, None).await?;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let results = dispatcher.get_results().await;
    assert_eq!(results.len(), 0);
    
    // C. Update Alice (age 25 -> 30) -> Expect Updating
    println!("Updating Alice (25 -> 30)...");
    collection.update_one(doc! { "name": "Alice" }, doc! { "$set": { "age": 30 } }, None).await?;
    
    wait_for_result(&dispatcher, 1).await?;
    let results = dispatcher.get_results().await;
    assert_eq!(results.len(), 1);
    match &results[0] {
        QueryPartEvaluationContext::Updating { .. } => {},
        _ => panic!("Expected Updating result, got {:?}", results[0]),
    }
    dispatcher.clear_results().await;
    
    // D. Update Alice (age 30 -> 10) -> Expect Removing
    println!("Updating Alice (30 -> 10)...");
    collection.update_one(doc! { "name": "Alice" }, doc! { "$set": { "age": 10 } }, None).await?;
    
    wait_for_result(&dispatcher, 1).await?;
    let results = dispatcher.get_results().await;
    assert_eq!(results.len(), 1);
    match &results[0] {
        QueryPartEvaluationContext::Removing { .. } => {},
        _ => panic!("Expected Removing result, got {:?}", results[0]),
    }
    dispatcher.clear_results().await;
    
    source.stop().await?;
    Ok(())
}

#[tokio::test]
#[ignore]
async fn test_end_to_end_resume_persistence() -> Result<()> {
    // 1. Setup Mongo & Query (Similar to above)
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

    // Query setup
    use drasi_core::evaluation::functions::FunctionRegistry;
    use drasi_query_cypher::CypherParser;
    use drasi_functions_cypher::CypherFunctionSet;

    let query_str = "MATCH (n:resume_col) RETURN n.val";
    let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
    let parser = Arc::new(CypherParser::new(function_registry.clone()));
    let builder = QueryBuilder::new(query_str, parser).with_function_registry(function_registry);
    let query = Arc::new(RwLock::new(builder.build().await));
    let dispatcher = QueryDispatcher::new(query.clone());

    // Source Setup with State Store
    let config = MongoSourceConfig {
        connection_string: connection_string.clone(),
        database: Some(db_name.to_string()),
        collection: Some(collection_name.to_string()),
        collections: vec![],
        pipeline: None,
        username: None,
        password: None,
    };

    let source = MongoSource::new("resume-source", config)?;
    source.base.dispatchers.write().await.push(Box::new(dispatcher.clone()));
    let state_store = Arc::new(drasi_lib::state_store::MemoryStateStoreProvider::new());
    
    let (status_tx, _status_rx) = tokio::sync::mpsc::channel(100);
    source.initialize(drasi_lib::context::SourceRuntimeContext::new("resume-source", status_tx.clone(), Some(state_store.clone()))).await;

    source.start().await?;
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Insert item 1
    collection.insert_one(doc! { "val": 1 }, None).await?;
    wait_for_result(&dispatcher, 1).await?;
    dispatcher.clear_results().await;

    // Stop Source
    println!("Stopping source...");
    source.stop().await?;
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Insert item 2 while stopped
    println!("Inserting item 2 (offline)...");
    collection.insert_one(doc! { "val": 2 }, None).await?;

    // Restart Source
    println!("Restarting source...");
    // We need to create a new source instance because the old one is consumed/stopped state? 
    // Usually `start` is re-entrant if designed well, but `stream.rs` loop breaks. 
    // We can call `start` again on the same instance? 
    // `MongoSource::start` checks if running. if stopped, it spawns new task.
    source.start().await?;
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Verify item 2 is picked up
    wait_for_result(&dispatcher, 1).await?;
    let results = dispatcher.get_results().await;
    assert_eq!(results.len(), 1);
    // Logic: If resume token worked, we see item 2.
    // If it failed/ignored, we might see it (because it scans form now?), No, change stream "now" misses past events.
    // So seeing it proves resume worked (or at least we started from before).
    
    source.stop().await?;
    Ok(())
}

#[tokio::test]
#[ignore]
async fn test_end_to_end_multi_collection() -> Result<()> {
    // 1. Setup Mongo & Query
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

    // Query: MATCH (n) WHERE n:col1 OR n:col2 RETURN n.val
    use drasi_core::evaluation::functions::FunctionRegistry;
    use drasi_query_cypher::CypherParser;
    use drasi_functions_cypher::CypherFunctionSet;

    let query_str = "MATCH (n) WHERE n:col1 OR n:col2 RETURN n.val";
    let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
    let parser = Arc::new(CypherParser::new(function_registry.clone()));
    let builder = QueryBuilder::new(query_str, parser).with_function_registry(function_registry);
    let query = Arc::new(RwLock::new(builder.build().await));
    let dispatcher = QueryDispatcher::new(query.clone());

    // Source Setup
    let config = MongoSourceConfig {
        connection_string: connection_string.clone(),
        database: Some(db_name.to_string()),
        collection: None,
        collections: vec!["col1".to_string(), "col2".to_string()],
        pipeline: None,
        username: None,
        password: None,
    };

    let source = MongoSource::new("multi-source", config)?;
    source.base.dispatchers.write().await.push(Box::new(dispatcher.clone()));
    
    let state_store = Arc::new(drasi_lib::state_store::MemoryStateStoreProvider::new());
    let (status_tx, _status_rx) = tokio::sync::mpsc::channel(100);
    source.initialize(drasi_lib::context::SourceRuntimeContext::new("multi-source", status_tx, Some(state_store))).await;

    source.start().await?;
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Test Interactions
    println!("Inserting into col1...");
    col1.insert_one(doc! { "val": "from_col1" }, None).await?;
    wait_for_result(&dispatcher, 1).await?;
    let results = dispatcher.get_results().await;
    assert_eq!(results.len(), 1);
    dispatcher.clear_results().await;

    println!("Inserting into col2...");
    col2.insert_one(doc! { "val": "from_col2" }, None).await?;
    wait_for_result(&dispatcher, 1).await?;
    let results = dispatcher.get_results().await;
    assert_eq!(results.len(), 1);
    dispatcher.clear_results().await;

    println!("Inserting into col3 (ignored)...");
    col3.insert_one(doc! { "val": "from_col3" }, None).await?;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let results = dispatcher.get_results().await;
    assert_eq!(results.len(), 0);

    source.stop().await?;
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

    // Query Setup
    use drasi_core::evaluation::functions::FunctionRegistry;
    use drasi_query_cypher::CypherParser;
    use drasi_functions_cypher::CypherFunctionSet;

    let query_str = "MATCH (n:auth_col) RETURN n.msg";
    let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
    let parser = Arc::new(CypherParser::new(function_registry.clone()));
    let builder = QueryBuilder::new(query_str, parser).with_function_registry(function_registry);
    let query = Arc::new(RwLock::new(builder.build().await));
    let dispatcher = QueryDispatcher::new(query.clone());

    // Source Setup - Use Config Overrides
    let config = MongoSourceConfig {
        connection_string: connection_string_no_auth.clone(), // No auth in string
        database: Some(db_name.to_string()),
        collection: Some(collection_name.to_string()),
        collections: vec![],
        pipeline: None,
        username: Some("drasi".to_string()),     // Auth in Config
        password: Some("drasi_password".to_string()), 
    };

    let source = MongoSource::new("auth-source", config)?;
    source.base.dispatchers.write().await.push(Box::new(dispatcher.clone()));
    let state_store = Arc::new(drasi_lib::state_store::MemoryStateStoreProvider::new());
    source.initialize(drasi_lib::context::SourceRuntimeContext::new("auth-source", tokio::sync::mpsc::channel(100).0, Some(state_store))).await;

    source.start().await?;
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Test Interaction
    println!("Inserting authenticated...");
    collection.insert_one(doc! { "msg": "secure_data" }, None).await?;
    wait_for_result(&dispatcher, 1).await?;
    let results = dispatcher.get_results().await;
    assert_eq!(results.len(), 1);

    source.stop().await?;
    Ok(())
}
