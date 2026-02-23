use anyhow::Result;
use drasi_core::models::{SourceChange, ElementValue, Element};
use drasi_lib::channels::{ChangeDispatcher, ChangeReceiver, SourceEventWrapper, SourceEvent};
use drasi_source_mongodb::config::MongoSourceConfig;
use drasi_source_mongodb::MongoSource;
use drasi_lib::Source;
use mongodb::bson::{doc, Document};
use mongodb::Client;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use drasi_lib::state_store::StateStoreProvider;
use testcontainers::{core::{ContainerPort, WaitFor}, runners::AsyncRunner, GenericImage, ImageExt};

// Mock Dispatcher to capture events
#[derive(Clone)]
struct MockDispatcher {
    events: Arc<RwLock<Vec<SourceChange>>>,
}

impl MockDispatcher {
    fn new() -> Self {
        Self {
            events: Arc::new(RwLock::new(Vec::new())),
        }
    }
    
    async fn get_events(&self) -> Vec<SourceChange> {
        self.events.read().await.clone()
    }

    async fn clear(&self) {
        self.events.write().await.clear();
    }
}

#[async_trait::async_trait]
impl ChangeDispatcher<SourceEventWrapper> for MockDispatcher {
    async fn dispatch_change(&self, event: Arc<SourceEventWrapper>) -> Result<()> {
        if let SourceEvent::Change(change) = &event.event {
            self.events.write().await.push(change.clone());
        }
        Ok(())
    }

    async fn create_receiver(&self) -> Result<Box<dyn ChangeReceiver<SourceEventWrapper>>> {
        Err(anyhow::anyhow!("Not implemented for mock"))
    }
}

#[tokio::test]
#[ignore]
async fn test_mongo_source_integration() -> Result<()> {
    // Start MongoDB container with Replica Set
    // We need to bind to all interfaces and start with replSet
    let container = GenericImage::new("mongo", "5.0")
        .with_wait_for(WaitFor::message_on_stdout("Waiting for connections"))
        .with_exposed_port(ContainerPort::Tcp(27017))
        .with_cmd(vec!["mongod", "--replSet", "rs0", "--bind_ip_all"])
        .start()
        .await?;
    
    let port = container.get_host_port_ipv4(27017).await?;
    let connection_string = format!("mongodb://127.0.0.1:{}", port);

    // Initialize Replica Set
    let client = Client::with_uri_str(&connection_string).await?;
    
    // Attempt to initiate replica set
    // In a real container we might need to wait a bit or retry
    let _ = client.database("admin").run_command(doc! { "replSetInitiate": {} }, None).await;
    
    // Wait for replica set to be ready (primary)
    let mut ready = false;
    for _ in 0..30 {
        if let Ok(status) = client.database("admin").run_command(doc! { "replSetGetStatus": 1 }, None).await {
            if let Ok(members) = status.get_array("members") {
                if let Some(member) = members.first() {
                    if let Some(state_str) = member.as_document().and_then(|d| d.get_str("stateStr").ok()) {
                        if state_str == "PRIMARY" {
                            ready = true;
                            break;
                        }
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    
    if !ready {
        // Fallback check, maybe simple ping works if we are just testing connectivity?
        // But change streams need Primary.
        println!("Warning: Could not verify Primary state, continuing anyway...");
    }

    let db_name = "test_db";
    let collection_name = "test_collection";
    let db = client.database(db_name);
    let collection = db.collection::<Document>(collection_name);

    // Configure Source
    let config = MongoSourceConfig {
        connection_string: connection_string.clone(),
        database: db_name.to_string(),
        collections: vec![collection_name.to_string()],
        pipeline: None,
        username: None,
        password: None,
    };

    let source = MongoSource::new("test-source", config)?;
    let mock_dispatcher = MockDispatcher::new();
    source.base.dispatchers.write().await.push(Box::new(mock_dispatcher.clone()));

    // Initialize with state store
    let state_store = Arc::new(drasi_lib::state_store::MemoryStateStoreProvider::new());
    let (status_tx, _status_rx) = tokio::sync::mpsc::channel(100);
    
    let context = drasi_lib::context::SourceRuntimeContext::new(
        "test-source",
        status_tx,
        Some(state_store.clone()),
    );
    source.initialize(context).await;

    // 1. Start Source
    println!("Starting source...");
    source.start().await?;
    tokio::time::sleep(Duration::from_secs(3)).await; // Wait for stream connection

    // 2. Test Insert
    println!("Testing Insert...");
    mock_dispatcher.clear().await;
    let doc_insert = doc! { "name": "test-item", "nested": { "val": 10 } };
    let insert_result = collection.insert_one(doc_insert, None).await?;
    let inserted_id = insert_result.inserted_id;

    wait_for_event(&mock_dispatcher, 1).await?;
    let events = mock_dispatcher.get_events().await;
    assert_eq!(events.len(), 1);
    
    if let SourceChange::Insert { element: Element::Node { properties, .. } } = &events[0] {
        if let Some(ElementValue::String(name)) = properties.get("name") {
            assert_eq!(name.as_ref(), "test-item");
        } else {
            panic!("Expected string for 'name'");
        }
        
        // Verify nested
        if let Some(ElementValue::Object(nested)) = properties.get("nested") {
             if let Some(ElementValue::Integer(val)) = nested.get("val") {
                 assert_eq!(*val, 10);
             } else {
                 panic!("Expected integer for 'nested.val'");
             }
        } else {
            panic!("Expected nested object");
        }
    } else {
        panic!("Expected Insert event, got {:?}", events[0]);
    }
    
    // Check state store for resume token
    assert!(state_store.contains_key("test-source", "resume_token").await.unwrap());

    // 3. Test Update (Change nested field)
    println!("Testing Update...");
    mock_dispatcher.clear().await;
    collection.update_one(
        doc! { "_id": inserted_id.clone() },
        doc! { "$set": { "nested.val": 20, "new_field": "test" } },
        None
    ).await?;

    wait_for_event(&mock_dispatcher, 1).await?;
    let events = mock_dispatcher.get_events().await;
    assert_eq!(events.len(), 1);

    if let SourceChange::Update { element: Element::Node { properties, .. } } = &events[0] {
        // Because we look up full document, we should see the merged state
        if let Some(ElementValue::Object(nested)) = properties.get("nested") {
            if let Some(ElementValue::Integer(val)) = nested.get("val") {
                assert_eq!(*val, 20);
            } else {
                 panic!("Expected integer for 'nested.val' after update");
            }
        } else {
            panic!("Expected nested object after update");
        }
        
        if let Some(ElementValue::String(name)) = properties.get("name") {
            assert_eq!(name.as_ref(), "test-item"); // Existing field preserved
        }
        
        if let Some(ElementValue::String(new_field)) = properties.get("new_field") {
            assert_eq!(new_field.as_ref(), "test");
        } else {
            panic!("Expected string for 'new_field'");
        }
    } else {
        panic!("Expected Update event, got {:?}", events[0]);
    }

    // 4. Test Delete
    println!("Testing Delete...");
    mock_dispatcher.clear().await;
    collection.delete_one(doc! { "_id": inserted_id.clone() }, None).await?;

    wait_for_event(&mock_dispatcher, 1).await?;
    let events = mock_dispatcher.get_events().await;
    assert_eq!(events.len(), 1);
    
    if let SourceChange::Delete { metadata } = &events[0] {
        assert!(metadata.reference.element_id.contains(&inserted_id.as_object_id().unwrap().to_hex()));
    } else {
        panic!("Expected Delete event");
    }

    // 5. Test Resume / Shutdown correctness
    println!("Testing Shutdown and Resume...");
    source.stop().await?;
    tokio::time::sleep(Duration::from_secs(1)).await;
    
    // Verify token is still in store
    let token = state_store.get("test-source", "resume_token").await.unwrap();
    assert!(token.is_some());
    
    // Insert while stopped
    let doc_offline = doc! { "name": "offline-item" };
    collection.insert_one(doc_offline, None).await?;
    
    // Clear events (should be empty anyway as source is stopped)
    mock_dispatcher.clear().await;
    
    // Restart source - it should pick up from last token and see the new item
    println!("Restarting source...");
    source.start().await?;
    tokio::time::sleep(Duration::from_secs(2)).await;
    
    wait_for_event(&mock_dispatcher, 1).await?;
    let events = mock_dispatcher.get_events().await;
    assert!(!events.is_empty());
    
    let last_event = events.last().unwrap();
    if let SourceChange::Insert { element: Element::Node { properties, .. } } = last_event {
         if let Some(ElementValue::String(name)) = properties.get("name") {
            assert_eq!(name.as_ref(), "offline-item");
        } else {
            println!("Got properties: {:?}", properties);
            panic!("Expected string for 'name' in offline item");
        }
    } else {
        panic!("Expected Insert event from offline data");
    }

    source.stop().await?;
    println!("Test completed successfully");
    Ok(())
}

async fn wait_for_event(dispatcher: &MockDispatcher, min_count: usize) -> Result<()> {
    for _ in 0..20 {
        if dispatcher.events.read().await.len() >= min_count {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Err(anyhow::anyhow!("Timeout waiting for events"))
}

#[tokio::test]
#[ignore]
async fn test_mongo_source_with_auth() -> Result<()> {
    // Start MongoDB container with Auth and Replica Set
    // We need a keyfile for ReplSet with Auth
    // Build image using GenericImage methods
    let image = GenericImage::new("mongo", "5.0")
        .with_wait_for(WaitFor::message_on_stdout("Waiting for connections"))
        .with_exposed_port(ContainerPort::Tcp(27017))
        .with_entrypoint("bash")
        .with_env_var("MONGO_INITDB_ROOT_USERNAME", "admin")
        .with_env_var("MONGO_INITDB_ROOT_PASSWORD", "password");
        
    // with_cmd returns ContainerRequest, so must be last before start
    let container = image
        .with_cmd(vec!["-c", "echo \"verysecretkey123\" > /tmp/keyfile && chmod 400 /tmp/keyfile && chown mongodb:mongodb /tmp/keyfile && mongod --replSet rs0 --bind_ip_all --auth --keyFile /tmp/keyfile"])
        .start()
        .await?;
    
    let port = container.get_host_port_ipv4(27017).await?;
    let connection_string_no_auth = format!("mongodb://127.0.0.1:{}", port);
    let connection_string_auth = format!("mongodb://admin:password@127.0.0.1:{}", port);

    // Initialize Replica Set (requires auth now?)
    // Localhost exception might allow init without auth if strictly localhost, 
    // but we are connecting from host to container port, so it's not localhost from mongo's perspective.
    // We must use the admin creds.
    
    let client_options = mongodb::options::ClientOptions::parse(&connection_string_auth).await?;
    let client = Client::with_options(client_options)?;
    
    // Initiate replSet
    // We might need to handle "already initialized" or similar if we restart, but this is fresh container.
    match client.database("admin").run_command(doc! { "replSetInitiate": {} }, None).await {
        Ok(_) => println!("Replica set initiated"),
        Err(e) => println!("Replica set initiation check: {}", e),
    }

    // Wait for primary
    let mut ready = false;
    for _ in 0..30 {
        if let Ok(status) = client.database("admin").run_command(doc! { "replSetGetStatus": 1 }, None).await {
            if let Ok(members) = status.get_array("members") {
                if let Some(member) = members.first() {
                    if let Some(state_str) = member.as_document().and_then(|d| d.get_str("stateStr").ok()) {
                        if state_str == "PRIMARY" {
                            ready = true;
                            break;
                        }
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    
    if !ready {
        panic!("Replica set failed to become PRIMARY");
    }

    let db_name = "auth_test_db";
    let collection_name = "auth_test_col";
    let db = client.database(db_name);
    let collection = db.collection::<Document>(collection_name);

    // Create a regular user for the source
    client.database("admin").run_command(doc! {
        "createUser": "drasi",
        "pwd": "drasi_password",
        "roles": [
            { "role": "read", "db": db_name },
            { "role": "read", "db": "local" } // Maybe needed for oplog? usually needs more perms for change stream
        ]
    }, None).await?;
    // Actually for change streams, user needs permission to read collection and read oplog.
    // Let's give root for simplicity in test, or correct roles.
    // "read" on db is enough for simple watch? No, need to read oplog or be clusterMonitor?
    // Let's grant "readAnyDatabase" and "clusterMonitor" or just "root" to simplify test setup correctness.
    client.database("admin").run_command(doc! {
        "grantRolesToUser": "drasi",
        "roles": [ "root" ]
    }, None).await?;

    // 1. Test failure without valid creds
    // We configure source with connection_string_no_auth and NO overrides.
    let config_fail = MongoSourceConfig {
        connection_string: connection_string_no_auth.clone(),
        database: db_name.to_string(),
        collections: vec![collection_name.to_string()],
        pipeline: None,
        username: None,
        password: None,
    };
    
    let source_fail = MongoSource::new("fail-source", config_fail)?;
    let mock_dispatcher = MockDispatcher::new();
    source_fail.base.dispatchers.write().await.push(Box::new(mock_dispatcher.clone()));
    
    source_fail.start().await?;
    tokio::time::sleep(Duration::from_secs(2)).await;
    
    // Check status - should be Error
    let status = source_fail.base.get_status().await;
    assert_eq!(status, drasi_lib::channels::ComponentStatus::Error);
    source_fail.stop().await?;

    // 2. Test success with credential overrides
    let config_success = MongoSourceConfig {
        connection_string: connection_string_no_auth.clone(),
        database: db_name.to_string(),
        collections: vec![collection_name.to_string()],
        pipeline: None,
        username: Some("drasi".to_string()),
        password: Some("drasi_password".to_string()),
    };
    
    let source_success = MongoSource::new("success-source", config_success)?;
    let mock_dispatcher_success = MockDispatcher::new();
    source_success.base.dispatchers.write().await.push(Box::new(mock_dispatcher_success.clone()));
    
    source_success.start().await?;
    tokio::time::sleep(Duration::from_secs(3)).await; // Connect
    
    // Check status
    assert_eq!(source_success.base.get_status().await, drasi_lib::channels::ComponentStatus::Running);
    
    // Perform Insert
    collection.insert_one(doc! { "msg": "authenticated" }, None).await?;
    
    wait_for_event(&mock_dispatcher_success, 1).await?;
    let events = mock_dispatcher_success.get_events().await;
    assert_eq!(events.len(), 1);
    
    if let SourceChange::Insert { element: Element::Node { properties, .. } } = &events[0] {
        if let Some(ElementValue::String(msg)) = properties.get("msg") {
            assert_eq!(msg.as_ref(), "authenticated");
        } else {
            panic!("Expected msg property");
        }
    }
    
    source_success.stop().await?;
    Ok(())
}

#[tokio::test]
#[ignore]
async fn test_mongo_source_multi_collection() -> Result<()> {
    // Start MongoDB container
    let container = GenericImage::new("mongo", "5.0")
        .with_wait_for(WaitFor::message_on_stdout("Waiting for connections"))
        .with_exposed_port(ContainerPort::Tcp(27017))
        .start()
        .await?;
    
    let port = container.get_host_port_ipv4(27017).await?;
    let connection_string = format!("mongodb://127.0.0.1:{}", port);
    
    // Initialize Replica Set
    let client_options = mongodb::options::ClientOptions::parse(&connection_string).await?;
    let client = Client::with_options(client_options)?;
    
    match client.database("admin").run_command(doc! { "replSetInitiate": {} }, None).await {
        Ok(_) => println!("Replica set initiated"),
        Err(e) => println!("Replica set initiation check: {}", e),
    }

    // Wait for primary
    let mut ready = false;
    for _ in 0..30 {
        if let Ok(status) = client.database("admin").run_command(doc! { "replSetGetStatus": 1 }, None).await {
            if let Ok(members) = status.get_array("members") {
                if let Some(member) = members.first() {
                    if let Some(state_str) = member.as_document().and_then(|d| d.get_str("stateStr").ok()) {
                        if state_str == "PRIMARY" {
                            ready = true;
                            break;
                        }
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    
    if !ready {
        panic!("Replica set failed to become PRIMARY");
    }

    let db_name = "multi_col_test_db";
    let db = client.database(db_name);
    let col1 = db.collection::<Document>("col1");
    let col2 = db.collection::<Document>("col2");
    let col3 = db.collection::<Document>("col3");

    // Configure Source with multiple collections
    let config = MongoSourceConfig {
        connection_string: connection_string.clone(),
        database: db_name.to_string(),
        collections: vec!["col1".to_string(), "col2".to_string()],
        pipeline: None,
        username: None,
        password: None,
    };

    let source = MongoSource::new("multi-col-source", config)?;
    let mock_dispatcher = MockDispatcher::new();
    source.base.dispatchers.write().await.push(Box::new(mock_dispatcher.clone()));
    
    // Start Source
    println!("Starting source...");
    source.start().await?;
    tokio::time::sleep(Duration::from_secs(3)).await; // Wait for stream connection

    // Test Insert col1
    col1.insert_one(doc! { "msg": "from col1" }, None).await?;
    
    // Test Insert col2
    col2.insert_one(doc! { "msg": "from col2" }, None).await?;
    
    // Test Insert col3 (should be ignored)
    col3.insert_one(doc! { "msg": "from col3" }, None).await?;
    
    // Verify events
    wait_for_event(&mock_dispatcher, 2).await?;
    let events = mock_dispatcher.get_events().await;
    assert_eq!(events.len(), 2);
    
    let mut found_col1 = false;
    let mut found_col2 = false;
    
    for event in events {
        if let SourceChange::Insert { element: Element::Node { metadata, properties } } = event {
            let id = metadata.reference.element_id;
            println!("Event ID: {}", id);
            
            if id.starts_with("col1:") {
                found_col1 = true;
                if let Some(ElementValue::String(msg)) = properties.get("msg") {
                    assert_eq!(msg.as_ref(), "from col1");
                }
            } else if id.starts_with("col2:") {
                found_col2 = true;
                if let Some(ElementValue::String(msg)) = properties.get("msg") {
                     assert_eq!(msg.as_ref(), "from col2");
                }
            } else {
                panic!("Unexpected event source: {}", id);
            }
        }
    }
    
    assert!(found_col1, "Did not receive event from col1");
    assert!(found_col2, "Did not receive event from col2");
    
    source.stop().await?;
    Ok(())
}
