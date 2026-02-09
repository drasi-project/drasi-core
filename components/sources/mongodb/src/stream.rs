use anyhow::{anyhow, Result};
use futures::stream::StreamExt;
use log::{error, info};
use mongodb::{
    bson::Document,
    options::{ChangeStreamOptions, ClientOptions, FullDocumentType},
    Client,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::time::sleep;

use drasi_lib::channels::{
    ChangeDispatcher, ComponentEventSender, ComponentStatus, SourceEvent, SourceEventWrapper,
};
use drasi_lib::sources::base::SourceBase;

use crate::config::MongoSourceConfig;
use crate::conversion::change_stream_event_to_source_change;

pub struct ReplicationStream {
    config: MongoSourceConfig,
    source_id: String,
    dispatchers: Arc<RwLock<Vec<Box<dyn ChangeDispatcher<SourceEventWrapper> + Send + Sync>>>>,
    status_tx: Arc<RwLock<Option<ComponentEventSender>>>,
    status: Arc<RwLock<ComponentStatus>>,
    resume_token: Option<mongodb::change_stream::event::ResumeToken>,
}

impl ReplicationStream {
    pub fn new(
        config: MongoSourceConfig,
        source_id: String,
        dispatchers: Arc<RwLock<Vec<Box<dyn ChangeDispatcher<SourceEventWrapper> + Send + Sync>>>>,
        status_tx: Arc<RwLock<Option<ComponentEventSender>>>,
        status: Arc<RwLock<ComponentStatus>>,
    ) -> Self {
        Self {
            config,
            source_id,
            dispatchers,
            status_tx,
            status,
            resume_token: None,
        }
    }

    pub async fn run(&mut self) -> Result<()> {
        info!("Starting MongoDB replication stream for source {}", self.source_id);

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
                error!("Error in MongoDB stream loop: {e}");
                // Simple backoff strategy
                sleep(Duration::from_secs(5)).await;
            }
        }
        
        Ok(())
    }

    async fn stream_loop(&mut self) -> Result<()> {
        let client_options = ClientOptions::parse(&self.config.connection_string).await?;
        let client = Client::with_options(client_options)?;
        let db = client.database(&self.config.database);
        let collection = db.collection::<Document>(&self.config.collection);

        let mut options = ChangeStreamOptions::builder()
            .full_document(Some(FullDocumentType::UpdateLookup))
            .build();
        
        if let Some(token) = &self.resume_token {
            info!("Resuming change stream from token");
            options.resume_after = Some(token.clone());
        }

        let pipeline = self.config.pipeline.clone().unwrap_or_default();
        let mut stream = collection.watch(pipeline, Some(options)).await?;

        info!("Connected to MongoDB Change Stream");

        while let Some(event_result) = stream.next().await {
            // Check stop signal in inner loop
             {
                let status = self.status.read().await;
                if *status == ComponentStatus::Stopping || *status == ComponentStatus::Stopped {
                    break;
                }
            }

            match event_result {
                Ok(event) => {
                    // Update resume token
                    self.resume_token = Some(event.id.clone());

                    match change_stream_event_to_source_change(event, &self.source_id, &self.config.collection) {
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
                                // Log but continue - this just means no one is listening
                                log::debug!("[{}] Failed to dispatch change: {}", self.source_id, e);
                            }
                        }
                        Ok(None) => {
                            // Event ignored (e.g. invalidate or filtered out)
                        }
                        Err(e) => {
                            error!("Failed to convert change stream event: {e}");
                        }
                    }
                }
                Err(e) => {
                    return Err(anyhow::anyhow!("Change stream error: {e}"));
                }
            }
        }

        Ok(())
    }
}
