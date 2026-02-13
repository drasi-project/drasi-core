use anyhow::Result;
use futures::stream::StreamExt;
use log::{error, info};
use mongodb::{
    bson::Document,
    options::{ChangeStreamOptions, ClientOptions},
    Client,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

use drasi_lib::channels::{
    ChangeDispatcher, ComponentStatus, SourceEvent, SourceEventWrapper,
};
use drasi_lib::sources::base::SourceBase;

use crate::config::MongoSourceConfig;
use crate::conversion::change_stream_event_to_source_change;

use drasi_lib::state_store::StateStoreProvider;

pub struct ReplicationStream {
    config: MongoSourceConfig,
    source_id: String,
    dispatchers: Arc<RwLock<Vec<Box<dyn ChangeDispatcher<SourceEventWrapper> + Send + Sync>>>>,
    status: Arc<RwLock<ComponentStatus>>,
    resume_token: Option<mongodb::change_stream::event::ResumeToken>,
    shutdown_rx: tokio::sync::broadcast::Receiver<()>,
    state_store: Option<Arc<dyn StateStoreProvider>>,
}

impl ReplicationStream {
    pub fn new(
        config: MongoSourceConfig,
        source_id: String,
        dispatchers: Arc<RwLock<Vec<Box<dyn ChangeDispatcher<SourceEventWrapper> + Send + Sync>>>>,
        status: Arc<RwLock<ComponentStatus>>,
        shutdown_rx: tokio::sync::broadcast::Receiver<()>,
        state_store: Option<Arc<dyn StateStoreProvider>>,
    ) -> Self {
        Self {
            config,
            source_id,
            dispatchers,
            status,
            resume_token: None,
            shutdown_rx,
            state_store,
        }
    }

    pub async fn run(&mut self) -> Result<()> {
        info!("Starting MongoDB replication stream for source {}", self.source_id);

        // Try to load resume token
        if let Some(store) = &self.state_store {
            match store.get(&self.source_id, "resume_token").await {
                Ok(Some(bytes)) => {
                    match mongodb::bson::from_slice(&bytes) {
                        Ok(token) => {
                            info!("Loaded resume token from state store");
                            self.resume_token = Some(token);
                        }
                        Err(e) => error!("Failed to deserialize resume token: {e}"),
                    }
                }
                Ok(None) => info!("No resume token found in state store"),
                Err(e) => error!("Failed to load resume token: {e}"),
            }
        }

        loop {
            // Check for stop signal
            {
                let status = self.status.read().await;
                if *status == ComponentStatus::Stopping || *status == ComponentStatus::Stopped {
                    info!("Received stop signal, shutting down MongoDB stream");
                    break;
                }
            }

            if let Err(e) = self.stream_loop().await {
                // If the error is due to shutdown, we should exit
                if self.shutdown_rx.try_recv().is_ok() {
                    break;
                }
                
                error!("Error in MongoDB stream loop: {e}");
                // Simple backoff strategy
                tokio::time::sleep(Duration::from_secs(5)).await;
                // Check shutdown during sleep
                if self.shutdown_rx.try_recv().is_ok() {
                    break;
                }
            } else {
                 // graceful exit
                 break;
            }
        }
        
        Ok(())
    }

    async fn stream_loop(&mut self) -> Result<()> {
        let mut client_options = ClientOptions::parse(&self.config.connection_string).await?;

        // Handle credentials override
        let username = self.config.username.clone()
            .or_else(|| std::env::var("MONGODB_USERNAME").ok());
        let password = self.config.password.clone()
            .or_else(|| std::env::var("MONGODB_PASSWORD").ok());

        if let (Some(u), Some(p)) = (username, password) {
            let credential = mongodb::options::Credential::builder()
                .username(u)
                .password(p)
                .build();
            client_options.credential = Some(credential);
        }

        let client = Client::with_options(client_options)?;
        
        let db_name = self.config.database.as_ref().ok_or_else(|| anyhow::anyhow!("Database not configured"))?;
        let db = client.database(db_name);

        let collections = self.config.get_collections();
        if collections.is_empty() {
            return Err(anyhow::anyhow!("No collections configured"));
        }

        let mut options = ChangeStreamOptions::builder()
            .full_document(Some(mongodb::options::FullDocumentType::UpdateLookup))
            .build();
        
        if let Some(token) = &self.resume_token {
            info!("Resuming change stream from token");
            options.resume_after = Some(token.clone());
        }

        info!("Starting change stream on database: {}, collections: {:?}", db_name, collections);

        let mut stream = if collections.len() == 1 {
            let col = db.collection::<Document>(&collections[0]);
            col.watch(self.config.pipeline.clone().into_iter().flatten(), Some(options)).await?
        } else {
            let mut pipeline = self.config.pipeline.clone().unwrap_or_default();
            
            // Filter by collections
            let filter = mongodb::bson::doc! {
                "$match": {
                    "ns.coll": { "$in": &collections }
                }
            };
            pipeline.insert(0, filter);
            
            db.watch(pipeline, Some(options)).await?
        };

        info!("Connected to MongoDB Change Stream");

        loop {
            tokio::select! {
                _ = self.shutdown_rx.recv() => {
                    info!("Received shutdown signal");
                    return Ok(());
                }
                event_result = stream.next() => {
                    match event_result {
                        Some(Ok(event)) => {
                             // Check stop signal in inner loop - redundancy for safety
                            {
                                let status = self.status.read().await;
                                if *status == ComponentStatus::Stopping || *status == ComponentStatus::Stopped {
                                    break;
                                }
                            }

                            // Update resume token
                            self.resume_token = Some(event.id.clone());
                            
                            // Persist resume token
                            if let Some(store) = &self.state_store {
                                match mongodb::bson::to_vec(&event.id) {
                                    Ok(bytes) => {
                                        if let Err(e) = store.set(&self.source_id, "resume_token", bytes).await {
                                             error!("Failed to persist resume token: {e}");
                                        }
                                    }
                                    Err(e) => error!("Failed to serialize resume token: {e}"),
                                }
                            }

                            // Extract collection name from event
                            // Invalidate events might lack ns, we skip them
                            let collection_name = match &event.ns {
                                Some(ns) => match &ns.coll {
                                    Some(c) => c.clone(),
                                    None => continue,
                                },
                                None => continue,
                            };

                            match change_stream_event_to_source_change(event, &self.source_id, &collection_name) {
                                Ok(Some(change)) => {
                                    // Create profiling metadata
                                    let mut profiling = drasi_lib::profiling::ProfilingMetadata::new();
                                    profiling.source_send_ns = Some(drasi_lib::profiling::timestamp_ns());

                                    let wrapper = SourceEventWrapper::with_profiling(
                                        self.source_id.clone(),
                                        SourceEvent::Change(change),
                                        chrono::Utc::now(),
                                        profiling,
                                    );

                                    // Dispatch event
                                    if let Err(e) = SourceBase::dispatch_from_task(
                                        self.dispatchers.clone(),
                                        wrapper,
                                        &self.source_id,
                                    )
                                    .await
                                    {
                                        log::debug!("[{}] Failed to dispatch change: {}", self.source_id, e);
                                    }
                                }
                                Ok(None) => {
                                    // Event ignored
                                }
                                Err(e) => {
                                    error!("Failed to convert change stream event: {e}");
                                }
                            }
                        }
                        Some(Err(e)) => {
                             return Err(anyhow::anyhow!("Change stream error: {e}"));
                        }
                        None => {
                            return Err(anyhow::anyhow!("Change stream ended unexpectedly"));
                        }
                    }
                }
            }
        }

        Ok(())
    }
}
