use anyhow::Result;
use drasi_core::models::{SourceChange, ElementValue};
use drasi_lib::channels::{ChangeDispatcher, ChangeReceiver, SourceEventWrapper, SourceEvent};
use drasi_source_mongodb::config::MongoSourceConfig;
use drasi_source_mongodb::MongoSource;
use drasi_lib::Source;
use mongodb::bson::{doc, Document};
use mongodb::Client;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use testcontainers::{core::ContainerPort, runners::AsyncRunner, GenericImage};

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
    // Start MongoDB container
    let container = GenericImage::new("mongo", "5.0")
        .with_exposed_port(ContainerPort::Tcp(27017))
        .start()
        .await?;
    
    let port = container.get_host_port_ipv4(27017).await?;
    let connection_string = format!("mongodb://127.0.0.1:{}", port);

    // Setup MongoDB data
    let client = Client::with_uri_str(&connection_string).await?;
    let db = client.database("test_db");
    let collection = db.collection::<Document>("test_collection");

    // Configure Source
    let config = MongoSourceConfig {
        connection_string: connection_string.clone(),
        database: "test_db".to_string(),
        collection: "test_collection".to_string(),
        pipeline: None,
    };

    let source = MongoSource::new("test-source", config)?;
    
    // Inject mock dispatcher
    let mock_dispatcher = MockDispatcher::new();
    source.base.dispatchers.write().await.push(Box::new(mock_dispatcher.clone()));

    // Start source
    source.start().await?;

    // Wait a bit for connection
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Insert document
    let doc = doc! { "name": "test-item", "value": 42 };
    collection.insert_one(doc, None).await?;

    // Wait for event
    let mut found = false;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let events = mock_dispatcher.events.read().await;
        if !events.is_empty() {
            found = true;
            let change = &events[0];
            if let SourceChange::Insert { element } = change {
                if let drasi_core::models::Element::Node { properties, .. } = element {
                    if let Some(ElementValue::String(name)) = properties.get("name") {
                        assert_eq!(name.as_ref(), "test-item");
                    } else {
                        panic!("Property 'name' mismatch");
                    }
                } else {
                    panic!("Expected Element::Node");
                }
            } else {
                panic!("Expected Insert event");
            }
            break;
        }
    }

    assert!(found, "Did not receive change event");

    source.stop().await?;
    Ok(())
}
