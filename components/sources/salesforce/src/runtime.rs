use anyhow::{anyhow, ensure, Context, Result};
use chrono::Utc;
use drasi_lib::schema::SourceSchema;
use drasi_salesforce_common::{
    Catalog, NativeDecoder, ObjectPlan, ReplayCursor, SalesforceAuthManager,
    SalesforcePubSubClient, SalesforceRestClient, TopicUnavailable,
};
use futures_util::{stream, StreamExt, TryStreamExt};
use log::{info, warn};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{watch, Mutex, Notify, Semaphore};
use tokio::task::JoinSet;

use crate::config::SalesforceSourceConfig;
use crate::store::{DurableMirrorStore, RecordMutation, StaleSchema};

pub struct RuntimeContext {
    pub source_id: String,
    pub config: SalesforceSourceConfig,
    pub catalog: Arc<Catalog>,
    pub auth: SalesforceAuthManager,
    pub rest: SalesforceRestClient,
    pub store: Arc<DurableMirrorStore>,
    pub base: drasi_lib::sources::base::SourceBase,
    pub delivery_gate: Arc<Mutex<()>>,
    pub shutdown: watch::Sender<bool>,
    pub schema_snapshot: watch::Sender<Option<Arc<SourceSchema>>>,
}

pub async fn discover_catalog(
    config: &SalesforceSourceConfig,
    auth: &SalesforceAuthManager,
    rest: &SalesforceRestClient,
) -> Result<Catalog> {
    let mut refreshed = false;
    loop {
        let session = auth.session_token().await?;
        let result = stream::iter(config.objects.clone())
            .map(|object| {
                let session = &session;
                async move {
                    let description = rest.describe_object(session, &object).await?;
                    ensure!(
                        description.name == object,
                        "Describe returned '{}' for requested object '{object}'",
                        description.name
                    );
                    Ok(description)
                }
            })
            .buffer_unordered(4)
            .try_collect::<Vec<_>>()
            .await;
        match result {
            Err(error) if !refreshed && drasi_salesforce_common::rest::is_unauthorized(&error) => {
                auth.invalidate_oauth_cache().await;
                refreshed = true;
            }
            descriptions => return Catalog::from_describes(descriptions?),
        }
    }
}

impl RuntimeContext {
    pub async fn run(self, shutdown_rx: watch::Receiver<bool>) -> Result<()> {
        let (catalog_tx, catalog_rx) = watch::channel(self.catalog.clone());
        let rest_budget = Arc::new(Semaphore::new(4));
        let dirty_budget = Arc::new(Semaphore::new(self.config.max_rows));
        let refresh_requested = Arc::new(Notify::new());
        let runtime = Arc::new(self);
        let mut workers = JoinSet::new();
        for object in &runtime.config.objects {
            let observer = ObjectObserver {
                runtime: runtime.clone(),
                object: object.clone(),
                catalog: catalog_rx.clone(),
                rest_budget: rest_budget.clone(),
                dirty_budget: dirty_budget.clone(),
                refresh_requested: refresh_requested.clone(),
            };
            let shutdown = shutdown_rx.clone();
            workers.spawn(async move { observer.run(shutdown).await });
        }
        let metadata_runtime = runtime.clone();
        workers.spawn(async move {
            metadata_runtime
                .refresh_metadata(catalog_tx, shutdown_rx, rest_budget, refresh_requested)
                .await
        });

        let mut outcome = Ok(());
        if let Some(result) = workers.join_next().await {
            outcome = worker_result(result);
            if outcome.is_ok() && !*runtime.shutdown.borrow() {
                outcome = Err(anyhow!(
                    "Salesforce object observer exited without shutdown"
                ));
            }
        }
        let _ = runtime.shutdown.send(true);
        while let Some(result) = workers.join_next().await {
            if let Err(error) = worker_result(result) {
                if outcome.is_ok() {
                    outcome = Err(error);
                } else {
                    warn!(
                        "[{}] Additional Salesforce shutdown error: {error:#}",
                        runtime.source_id
                    );
                }
            }
        }
        outcome
    }

    async fn refresh_metadata(
        &self,
        catalog_tx: watch::Sender<Arc<Catalog>>,
        mut shutdown: watch::Receiver<bool>,
        rest_budget: Arc<Semaphore>,
        refresh_requested: Arc<Notify>,
    ) -> Result<()> {
        let mut interval = tokio::time::interval(Duration::from_secs(300));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await;
        let mut requested = false;
        loop {
            requested |= tokio::select! {
                _ = shutdown.changed() => return Ok(()),
                _ = interval.tick() => false,
                _ = refresh_requested.notified() => true,
            };
            // Reserve all permits so discovery's four requests share the data-fetch budget.
            let _permit = tokio::select! {
                _ = shutdown.changed() => return Ok(()),
                permit = rest_budget.acquire_many(4) => permit?,
            };
            let discovered = tokio::select! {
                _ = shutdown.changed() => return Ok(()),
                catalog = discover_catalog(&self.config, &self.auth, &self.rest) => catalog,
            };
            let catalog = match discovered {
                Ok(catalog) => catalog,
                Err(error) if drasi_salesforce_common::rest::is_retryable(&error) => {
                    warn!(
                        "[{}] Salesforce metadata refresh will retry: {error:#}",
                        self.source_id
                    );
                    interval.reset_after(Duration::from_secs(5));
                    continue;
                }
                Err(error) => return Err(error).context("Salesforce metadata refresh failed"),
            };
            interval.reset_after(Duration::from_secs(300));
            if catalog.schema.fingerprint == catalog_tx.borrow().schema.fingerprint {
                if requested {
                    catalog_tx
                        .send(Arc::new(catalog))
                        .context("Salesforce object observers disappeared")?;
                }
                requested = false;
                continue;
            }
            let schema = Arc::new(catalog.source_schema());
            let _gate = tokio::select! {
                _ = shutdown.changed() => return Ok(()),
                gate = self.delivery_gate.lock() => gate,
            };
            self.store.set_schema(catalog.schema.clone()).await?;
            self.store.publish_schema().await?;
            catalog_tx
                .send(Arc::new(catalog))
                .context("Salesforce object observers disappeared")?;
            self.schema_snapshot.send_replace(Some(schema));
            requested = false;
            info!(
                "[{}] Salesforce schema changed; reprojecting all selected objects",
                self.source_id
            );
        }
    }
}

fn worker_result(result: std::result::Result<Result<()>, tokio::task::JoinError>) -> Result<()> {
    result.context("Salesforce object observer task failed")?
}

struct ObjectObserver {
    runtime: Arc<RuntimeContext>,
    object: String,
    catalog: watch::Receiver<Arc<Catalog>>,
    rest_budget: Arc<Semaphore>,
    dirty_budget: Arc<Semaphore>,
    refresh_requested: Arc<Notify>,
}

impl ObjectObserver {
    fn plan<'a>(&self, catalog: &'a Catalog) -> Result<&'a ObjectPlan> {
        catalog
            .plans
            .iter()
            .find(|plan| plan.name == self.object)
            .with_context(|| {
                format!(
                    "Salesforce catalog is missing selected object '{}'",
                    self.object
                )
            })
    }

    async fn run(mut self, mut shutdown_rx: watch::Receiver<bool>) -> Result<()> {
        let mut full_scan_ticker = tokio::time::interval(Duration::from_secs(
            self.runtime.config.full_scan_interval_seconds,
        ));
        full_scan_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut reconcile_ticker = tokio::time::interval(Duration::from_secs(
            self.runtime.config.reconcile_interval_seconds,
        ));
        reconcile_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut reconnect_delay = Duration::ZERO;
        let mut auth_failures = 0;

        loop {
            if *shutdown_rx.borrow() {
                return Ok(());
            }
            let runtime = self.runtime.clone();
            let catalog = self.catalog.borrow().clone();
            let plan = self.plan(&catalog)?;
            let delay_before_connect = reconnect_delay;
            let connection = async {
                tokio::time::sleep(delay_before_connect).await;
                let session = runtime.auth.session_token().await?;
                let pubsub = SalesforcePubSubClient::connect(&runtime.config.pubsub_endpoint)
                    .await
                    .context("failed to connect Salesforce Pub/Sub stream")?;
                let topic = pubsub
                    .get_topic(&session, plan)
                    .await
                    .context("failed to resolve Salesforce topic metadata")?;
                info!(
                    "[{}:{}] Subscribing to '{}'",
                    runtime.source_id, plan.name, topic.topic_name
                );
                let replay = match runtime.config.initial_cursor_behavior {
                    drasi_salesforce_common::InitialCursorBehavior::Latest => ReplayCursor::Latest,
                    drasi_salesforce_common::InitialCursorBehavior::Earliest => {
                        ReplayCursor::Earliest
                    }
                };
                let feed = pubsub
                    .subscribe(&session, &topic.topic_name, replay)
                    .await
                    .context("failed to subscribe to Salesforce Pub/Sub")?;
                Ok::<_, anyhow::Error>((session, pubsub, feed))
            };
            tokio::pin!(connection);
            let connected = loop {
                tokio::select! {
                    _ = shutdown_rx.changed() => return Ok(()),
                    changed = self.catalog.changed() => {
                        if *shutdown_rx.borrow() {
                            return Ok(());
                        }
                        changed.context("Salesforce metadata supervisor stopped")?;
                        full_scan_ticker.reset_immediately();
                    }
                    _ = full_scan_ticker.tick() => {
                        if let Err(error) = self.reconcile(None, &mut shutdown_rx).await {
                            self.handle_observation_error(error)?;
                        }
                    }
                    connected = &mut connection => break connected,
                }
            };
            let (session, pubsub, mut feed) = match connected {
                Ok(connection) => connection,
                Err(error) => {
                    reconnect_delay = self
                        .prepare_reconnect(error, &mut auth_failures, reconnect_delay)
                        .await?;
                    continue;
                }
            };
            info!(
                "[{}:{}] Native CDC connected",
                self.runtime.source_id, self.object
            );
            if let Err(error) = self.reconcile(None, &mut shutdown_rx).await {
                self.handle_observation_error(error)?;
            }

            let mut decoders: HashMap<String, NativeDecoder> = HashMap::new();
            let mut invalidated = HashMap::new();
            'stream: loop {
                if *shutdown_rx.borrow() {
                    return Ok(());
                }
                tokio::select! {
                    _ = shutdown_rx.changed() => return Ok(()),
                    changed = self.catalog.changed() => {
                        if *shutdown_rx.borrow() {
                            return Ok(());
                        }
                        changed.context("Salesforce metadata supervisor stopped")?;
                        full_scan_ticker.reset_immediately();
                    }
                    _ = full_scan_ticker.tick() => {
                        match self.reconcile(None, &mut shutdown_rx).await {
                            Ok(()) => invalidated.clear(),
                            Err(error) => self.handle_observation_error(error)?,
                        }
                    }
                    _ = reconcile_ticker.tick(), if !invalidated.is_empty() => {
                        let ids = invalidated.keys().cloned().collect::<Vec<_>>();
                        match self.reconcile(Some(&ids), &mut shutdown_rx).await {
                            Ok(()) => invalidated.clear(),
                            Err(error) => self.handle_observation_error(error)?,
                        }
                    }
                    response = feed.next() => {
                        let response = match response {
                            Ok(response) => response,
                            Err(error) => {
                                reconnect_delay = self.prepare_reconnect(error, &mut auth_failures, reconnect_delay).await?;
                                break;
                            }
                        };
                        reconnect_delay = Duration::ZERO;
                        auth_failures = 0;
                        for envelope in response.events {
                            let Some(event) = envelope.event else { continue };
                            ensure!(event.payload.len() <= self.runtime.config.max_scan_bytes,
                                "Salesforce envelope exceeded configured byte limit");
                            if !decoders.contains_key(&event.schema_id) {
                                let schema = tokio::select! {
                                    _ = shutdown_rx.changed() => return Ok(()),
                                    schema = pubsub.get_schema(&session, &event.schema_id) => schema,
                                };
                                let schema = match schema {
                                    Ok(schema) => schema,
                                    Err(error) => {
                                        reconnect_delay = self.prepare_reconnect(error, &mut auth_failures, reconnect_delay).await?;
                                        break 'stream;
                                    }
                                };
                                if decoders.len() == 8 {
                                    if let Some(oldest) = decoders.keys().next().cloned() {
                                        decoders.remove(&oldest);
                                    }
                                }
                                let catalog = self.catalog.borrow().clone();
                                decoders.insert(event.schema_id.clone(),
                                    NativeDecoder::for_invalidations(&schema, self.plan(&catalog)?)
                                        .with_context(|| format!("failed to parse schema {}", event.schema_id))?);
                            }
                            let invalidation = decoders.get(&event.schema_id)
                                .context("schema decoder cache miss")?
                                .decode_invalidation(&event.payload)
                                .with_context(|| format!("failed to decode schema {}", event.schema_id))?;
                            if invalidation.operation == drasi_salesforce_common::Operation::Reconcile {
                                full_scan_ticker.reset_immediately();
                                continue;
                            }
                            for id in invalidation.ids {
                                if invalidated.contains_key(&id) {
                                    continue;
                                }
                                match self.dirty_budget.clone().try_acquire_owned() {
                                    Ok(permit) => { invalidated.insert(id, permit); }
                                    Err(_) => {
                                        warn!("[{}:{}] Dirty-ID budget reached; scheduling a complete scan", self.runtime.source_id, self.object);
                                        invalidated.clear();
                                        full_scan_ticker.reset_immediately();
                                        break;
                                    }
                                }
                            }
                        }
                        let replenished = tokio::select! {
                            _ = shutdown_rx.changed() => return Ok(()),
                            result = feed.replenish(response.pending_num_requested) => result,
                        };
                        if let Err(error) = replenished {
                            reconnect_delay = self.prepare_reconnect(error, &mut auth_failures, reconnect_delay).await?;
                            break;
                        }
                    }
                }
            }
        }
    }

    async fn reconcile(
        &self,
        ids: Option<&[String]>,
        shutdown: &mut watch::Receiver<bool>,
    ) -> Result<()> {
        let permit = tokio::select! {
            _ = shutdown.changed() => return Ok(()),
            permit = self.rest_budget.acquire() => permit?,
        };
        let mut refreshed_catalog = self.catalog.clone();
        let catalog = refreshed_catalog.borrow_and_update().clone();
        let plan = self.plan(&catalog)?;
        let fetched = tokio::select! {
            _ = shutdown.changed() => return Ok(()),
            rows = self.observe(plan, ids) => rows,
        };
        let fetched = match fetched {
            Err(error) if error.is::<drasi_salesforce_common::rest::InvalidField>() => {
                drop(permit);
                warn!(
                    "[{}:{}] Query projection is stale; refreshing Salesforce metadata",
                    self.runtime.source_id, self.object
                );
                self.refresh_requested.notify_one();
                tokio::select! {
                    biased;
                    _ = shutdown.changed() => return Ok(()),
                    changed = refreshed_catalog.changed() => {
                        changed.context("Salesforce metadata supervisor stopped")?;
                    }
                }
                if refreshed_catalog.borrow().schema.fingerprint == catalog.schema.fingerprint {
                    return Err(error).context(
                        "Salesforce projection is still invalid after metadata rediscovery",
                    );
                }
                return Err(StaleSchema.into());
            }
            result => result?,
        };
        let candidates = match ids {
            Some(ids) => ids.to_vec(),
            None => self.runtime.store.record_ids(&self.object).await?,
        };
        let mut seen = HashSet::new();
        let mut upserts = Vec::with_capacity(fetched.len());
        for record in fetched {
            ensure!(
                seen.insert(record.id.clone()),
                "Salesforce returned duplicate record {}",
                record.id
            );
            upserts.push(RecordMutation::Upsert(
                catalog.record(&self.object, record)?,
            ));
        }
        let mut mutations = Vec::with_capacity(upserts.len());
        for id in candidates {
            if !seen.contains(&id) {
                mutations.push(RecordMutation::Delete {
                    object: self.object.clone(),
                    id,
                    observed_at_ms: Utc::now().timestamp_millis() as u64,
                    schema_revision: catalog.schema.fingerprint.clone(),
                });
            }
        }
        mutations.extend(upserts);
        self.commit_and_dispatch(mutations, shutdown).await?;
        if ids.is_none() {
            let _gate = tokio::select! {
                _ = shutdown.changed() => return Ok(()),
                gate = self.runtime.delivery_gate.lock() => gate,
            };
            self.runtime.store.publish_schema().await?;
            info!(
                "[{}:{}] Complete scan reconciled {} records",
                self.runtime.source_id,
                self.object,
                seen.len()
            );
        }
        Ok(())
    }

    async fn commit_and_dispatch(
        &self,
        mutations: Vec<RecordMutation>,
        shutdown: &mut watch::Receiver<bool>,
    ) -> Result<()> {
        let mut pending = mutations.into_iter().peekable();
        while pending.peek().is_some() {
            let batch = pending.by_ref().take(256).collect();
            let _gate = tokio::select! {
                _ = shutdown.changed() => return Ok(()),
                gate = self.runtime.delivery_gate.lock() => gate,
            };
            let events = self.runtime.store.commit_mutations(batch).await?;
            if !events.is_empty() {
                let wrappers = events
                    .into_iter()
                    .map(|event| event.as_ref().clone())
                    .collect();
                tokio::select! {
                    _ = shutdown.changed() => return Ok(()),
                    dispatched = self.runtime.base.dispatch_events_batch(wrappers) => dispatched?,
                }
            }
        }
        Ok(())
    }

    async fn observe(
        &self,
        plan: &ObjectPlan,
        ids: Option<&[String]>,
    ) -> Result<Vec<drasi_salesforce_common::projection::Record>> {
        let mut refreshed = false;
        loop {
            let session = self.runtime.auth.session_token().await?;
            let result = match ids {
                Some(ids) => {
                    self.runtime
                        .rest
                        .fetch_records_by_ids(
                            &session,
                            plan,
                            ids,
                            self.runtime.config.max_scan_bytes,
                        )
                        .await
                }
                None => {
                    self.runtime
                        .rest
                        .full_scan(
                            &session,
                            plan,
                            self.runtime.config.max_rows,
                            self.runtime.config.max_scan_bytes,
                        )
                        .await
                }
            };
            match result {
                Err(error)
                    if !refreshed && drasi_salesforce_common::rest::is_unauthorized(&error) =>
                {
                    self.runtime.auth.invalidate_oauth_cache().await;
                    refreshed = true;
                }
                result => return result,
            }
        }
    }

    fn handle_observation_error(&self, error: anyhow::Error) -> Result<()> {
        if !error.is::<StaleSchema>() && !drasi_salesforce_common::rest::is_retryable(&error) {
            return Err(error).with_context(|| {
                format!(
                    "Salesforce {} reconciliation failed permanently",
                    self.object
                )
            });
        }
        warn!(
            "[{}:{}] Reconciliation will retry: {error:#}",
            self.runtime.source_id, self.object
        );
        Ok(())
    }

    async fn prepare_reconnect(
        &self,
        error: anyhow::Error,
        auth_failures: &mut u8,
        previous: Duration,
    ) -> Result<Duration> {
        if error.is::<TopicUnavailable>()
            || error
                .downcast_ref::<tonic::Status>()
                .is_some_and(|status| status.code() == tonic::Code::NotFound)
        {
            warn!("[{}:{}] Native CDC unavailable; REST-only reconciliation is active (topic retry in 60s): {error:#}", self.runtime.source_id, self.object);
            return Ok(Duration::from_secs(60));
        }
        if is_unauthenticated_error(&error) {
            *auth_failures += 1;
            if *auth_failures > 1 {
                return Err(error).context("Salesforce authentication failed after refresh");
            }
            self.runtime.auth.invalidate_oauth_cache().await;
        } else if !is_transient_stream_error(&error) {
            return Err(error).context("Salesforce stream failed permanently");
        }
        warn!(
            "[{}:{}] Native CDC will reconnect: {error:#}",
            self.runtime.source_id, self.object
        );
        Ok(previous
            .saturating_mul(2)
            .max(Duration::from_secs(1))
            .min(Duration::from_secs(30)))
    }
}

fn is_unauthenticated_error(error: &anyhow::Error) -> bool {
    drasi_salesforce_common::rest::is_unauthorized(error)
        || error
            .downcast_ref::<tonic::Status>()
            .is_some_and(|status| status.code() == tonic::Code::Unauthenticated)
}

fn is_transient_stream_error(error: &anyhow::Error) -> bool {
    drasi_salesforce_common::rest::is_retryable(error)
        || error.downcast_ref::<tonic::transport::Error>().is_some()
        || error.downcast_ref::<tonic::Status>().is_some_and(|status| {
            matches!(
                status.code(),
                tonic::Code::Unavailable
                    | tonic::Code::DeadlineExceeded
                    | tonic::Code::Cancelled
                    | tonic::Code::ResourceExhausted
                    | tonic::Code::Unauthenticated
            )
        })
}
