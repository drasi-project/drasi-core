pub mod config;
pub mod stream;
pub mod conversion;

use anyhow::Result;
use async_trait::async_trait;
use log::{error, info};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::task::JoinHandle;

use drasi_lib::channels::{ComponentEvent, ComponentStatus, ComponentType, SubscriptionResponse};
use drasi_lib::sources::base::{SourceBase, SourceBaseParams};
use drasi_lib::Source;

use crate::config::MongoSourceConfig;
use crate::stream::ReplicationStream;

pub struct MongoSource {
    pub base: SourceBase,
    config: MongoSourceConfig,
}

impl MongoSource {
    pub fn new(id: impl Into<String>, config: MongoSourceConfig) -> Result<Self> {
        let params = SourceBaseParams::new(id.into());
        Ok(Self {
            base: SourceBase::new(params)?,
            config,
        })
    }

    pub fn builder(id: impl Into<String>) -> MongoSourceBuilder {
        MongoSourceBuilder::new(id)
    }
}

#[async_trait]
impl Source for MongoSource {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "mongodb"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        let mut props = HashMap::new();
        props.insert("connection_string".to_string(), serde_json::Value::String(self.config.connection_string.clone()));
        props.insert("database".to_string(), serde_json::Value::String(self.config.database.clone()));
        props.insert("collection".to_string(), serde_json::Value::String(self.config.collection.clone()));
        props
    }

    async fn start(&self) -> Result<()> {
        if self.base.get_status().await == ComponentStatus::Running {
            return Ok(());
        }

        self.base.set_status(ComponentStatus::Starting).await;
        info!("Starting MongoDB source: {}", self.base.id);
        
        // validate config before starting
        if let Err(e) = self.config.validate() {
            let msg = format!("Invalid configuration: {}", e);
            error!("{}", msg);
            self.base.set_status_with_event(ComponentStatus::Error, Some(msg)).await?;
            return Err(e);
        }

        let config = self.config.clone();
        let source_id = self.base.id.clone();
        let dispatchers = self.base.dispatchers.clone();
        let status_tx = self.base.status_tx();
        let status_clone = self.base.status.clone();

        let task = tokio::spawn(async move {
            let mut stream = ReplicationStream::new(
                config,
                source_id.clone(),
                dispatchers,
                status_tx.clone(),
                status_clone.clone(),
            );
            
            if let Err(e) = stream.run().await {
                 error!("MongoDB stream task failed for {source_id}: {e}");
                *status_clone.write().await = ComponentStatus::Error;
                
                if let Some(ref tx) = *status_tx.read().await {
                    let _ = tx
                        .send(ComponentEvent {
                            component_id: source_id,
                            component_type: ComponentType::Source,
                            status: ComponentStatus::Error,
                            timestamp: chrono::Utc::now(),
                            message: Some(format!("Stream failed: {e}")),
                        })
                        .await;
                }
            }
        });

        self.base.set_task_handle(task).await;
        self.base.set_status(ComponentStatus::Running).await;
        
        self.base.send_component_event(
            ComponentStatus::Running,
            Some("MongoDB source started".to_string())
        ).await?;

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.base.stop_common().await
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn subscribe(
        &self,
        settings: drasi_lib::config::SourceSubscriptionSettings,
    ) -> Result<SubscriptionResponse> {
        self.base.subscribe_with_bootstrap(&settings, "MongoDB").await
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn initialize(&self, context: drasi_lib::context::SourceRuntimeContext) {
        self.base.initialize(context).await;
    }
}

pub struct MongoSourceBuilder {
    id: String,
    config: MongoSourceConfig,
}

impl MongoSourceBuilder {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            config: MongoSourceConfig {
                connection_string: String::new(),
                database: String::new(),
                collection: String::new(),
                pipeline: None,
            },
        }
    }

    pub fn with_connection_string(mut self, cs: impl Into<String>) -> Self {
        self.config.connection_string = cs.into();
        self
    }

    pub fn with_database(mut self, db: impl Into<String>) -> Self {
        self.config.database = db.into();
        self
    }

    pub fn with_collection(mut self, col: impl Into<String>) -> Self {
        self.config.collection = col.into();
        self
    }
    
    pub fn with_pipeline(mut self, pipeline: Vec<mongodb::bson::Document>) -> Self {
        self.config.pipeline = Some(pipeline);
        self
    }

    pub fn build(self) -> Result<MongoSource> {
        self.config.validate()?;
        MongoSource::new(self.id, self.config)
    }
}
