# Detailed Reliability Findings - Code Examples

This document provides detailed code examples and specific locations for all reliability issues identified in the Drasi Server-Core codebase.

---

## CRITICAL ISSUES

### Issue C1: Event Processor Runs in Unmonitored Background Task

**File**: `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/lifecycle.rs`
**Lines**: 156-172
**Severity**: CRITICAL
**Category**: Silent Failures

**Current Code**:
```rust
pub async fn start_event_processors(&mut self) {
    if let Some(receivers) = self.event_receivers.take() {
        // Start component event processor
        let component_rx = receivers.component_event_rx;
        tokio::spawn(async move {
            let mut rx = component_rx;
            while let Some(event) = rx.recv().await {
                info!(
                    "Component Event - {:?} {}: {:?} - {}",
                    event.component_type,
                    event.component_id,
                    event.status,
                    event.message.unwrap_or_default()
                );
            }
        });

        // DataRouter no longer needed - queries subscribe directly to sources
        drop(receivers.control_signal_rx);
        // SubscriptionRouter no longer needed
    }
}
```

**Problems**:
1. Task spawned with no way to track its health
2. If task panics, server becomes unaware
3. If channel closes unexpectedly, task silently stops
4. No way to restart the event processor
5. Server state becomes unobservable

**Impact**:
- Component lifecycle events stop flowing
- Server appears running but system is dead
- Debugging almost impossible without external monitoring

**Recommended Fix**:
```rust
pub async fn start_event_processors(&mut self) {
    if let Some(receivers) = self.event_receivers.take() {
        let component_rx = receivers.component_event_rx;
        let processor_task = tokio::spawn(async move {
            Self::event_processor_loop(component_rx).await
        });

        // Store task handle for health monitoring
        self.event_processor_handle = Some(processor_task);

        drop(receivers.control_signal_rx);
    }
}

async fn event_processor_loop(
    mut rx: broadcast::Receiver<ComponentEvent>,
) {
    loop {
        match rx.recv().await {
            Ok(event) => {
                info!(
                    "Component Event - {:?} {}: {:?} - {}",
                    event.component_type,
                    event.component_id,
                    event.status,
                    event.message.unwrap_or_default()
                );
            }
            Err(broadcast::error::RecvError::Lagged(_)) => {
                warn!("Event processor lagged, skipping messages");
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => {
                error!("Event processor channel closed, shutting down");
                break;
            }
        }
    }
    warn!("Event processor task terminated");
}
```

---

### Issue C2: Task Abort Without Graceful Shutdown

**File**: `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/reactions/base.rs`
**Lines**: 194-222
**Severity**: CRITICAL
**Category**: Resource Cleanup

**Current Code**:
```rust
pub async fn stop_common(&self) -> Result<()> {
    info!("Stopping reaction: {}", self.config.id);

    // Abort all subscription forwarder tasks
    let mut subscription_tasks = self.subscription_tasks.write().await;
    for task in subscription_tasks.drain(..) {
        task.abort();
    }
    drop(subscription_tasks);

    // Abort the processing task
    let mut processing_task = self.processing_task.write().await;
    if let Some(task) = processing_task.take() {
        task.abort();
    }
    drop(processing_task);

    // Drain the priority queue
    let drained_events = self.priority_queue.drain().await;
    if !drained_events.is_empty() {
        info!(
            "[{}] Drained {} pending events from priority queue",
            self.config.id,
            drained_events.len()
        );
    }

    Ok(())
}
```

**Problems**:
1. Tasks aborted immediately without cleanup opportunity
2. In-flight messages lost without persistence
3. Cleanup handlers may panic on aborted tasks
4. Subscribers left in inconsistent state
5. Subscribers may be trying to write to dropped channels

**Impact**:
- Data loss in between query results and reaction processing
- Potential panic cascade on shutdown
- State corruption if tasks held locks

**Recommended Fix**:
```rust
pub async fn stop_common(&self) -> Result<()> {
    info!("Stopping reaction: {}", self.config.id);

    // First, try graceful shutdown with timeout
    const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

    // Signal processing task to finish
    if let Some(task) = self.processing_task.write().await.take() {
        // Graceful shutdown: wait for task to finish naturally
        select! {
            _ = (&mut *self.processing_task.write().await.as_mut().unwrap()) => {
                debug!("[{}] Processing task finished gracefully", self.config.id);
            }
            _ = sleep(SHUTDOWN_TIMEOUT) => {
                warn!("[{}] Processing task did not finish within timeout", self.config.id);
                task.abort();
            }
        }
    }

    // Abort forwarder tasks after allowing them to finish priority queue
    sleep(Duration::from_millis(100)).await;  // Brief grace period

    let mut subscription_tasks = self.subscription_tasks.write().await;
    for task in subscription_tasks.drain(..) {
        task.abort();
    }
    drop(subscription_tasks);

    // Drain the priority queue
    let drained_events = self.priority_queue.drain().await;
    if !drained_events.is_empty() {
        warn!(
            "[{}] Drained {} unprocessed events from priority queue",
            self.config.id,
            drained_events.len()
        );
        // Consider logging these events for debugging/recovery
    }

    Ok(())
}
```

---

### Issue C3: PostgreSQL Connection Recovery No Backoff

**File**: `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/sources/postgres/stream.rs`
**Lines**: 91-150
**Severity**: CRITICAL
**Category**: Missing Recovery

**Current Code**:
```rust
pub async fn run(&mut self) -> Result<()> {
    info!("Starting replication stream for source {}", self.source_id);

    // Connect and setup replication - SINGLE ATTEMPT
    self.connect_and_setup().await?;

    // Start streaming loop
    let mut keepalive_interval = interval(Duration::from_secs(10));

    loop {
        // Check if we should stop
        {
            let status = self.status.read().await;
            if *status == ComponentStatus::Stopping || *status == ComponentStatus::Stopped {
                info!("Received stop signal, shutting down replication");
                break;
            }
        }

        tokio::select! {
            // Check for replication messages
            result = self.read_next_message() => {
                match result {
                    Ok(Some(msg)) => {
                        if let Err(e) = self.handle_message(msg).await {
                            error!("Error handling message: {}", e);
                            // Attempt recovery - BUT NO BACKOFF
                            if let Err(e) = self.recover_connection().await {
                                error!("Failed to recover connection: {}", e);
                                return Err(e);  // IMMEDIATE FAILURE
                            }
                        }
                    }
                    Ok(None) => {
                        // No message available
                    }
                    Err(e) => {
                        error!("Error reading message: {}", e);
                        if let Err(e) = self.recover_connection().await {
                            error!("Failed to recover connection: {}", e);
                            return Err(e);  // IMMEDIATE FAILURE
                        }
                    }
                }
            }

            // Send periodic keepalives
            _ = keepalive_interval.tick() => {
                if let Err(e) = self.send_feedback(false).await {
                    warn!("Failed to send keepalive: {}", e);
                    // WHAT HAPPENS HERE? KEEPS GOING? NO STATE CHANGE?
                }
            }
        }
    }

    // Clean shutdown
    self.shutdown().await?;
    Ok(())
}
```

**Problems**:
1. Initial `connect_and_setup()` has no retry
2. Connection recovery has no exponential backoff
3. Single transient network error causes permanent source failure
4. Keepalive failure logged but not tracked or acted upon
5. No state transition to indicate connection issues
6. No metrics on connection health

**Impact**:
- Brief network partition causes data loss
- No ability to recover from transient network issues
- PostgreSQL replication slot can be held open unnecessarily
- WAL files accumulate on database server

**Recommended Fix**:
```rust
pub async fn run(&mut self) -> Result<()> {
    info!("Starting replication stream for source {}", self.source_id);

    const MAX_INITIAL_RETRIES: u32 = 5;
    const RECOVERY_RETRY_BUDGET: u32 = 3;

    // Connect with retry logic
    let mut retry_delay = Duration::from_millis(100);
    for attempt in 1..=MAX_INITIAL_RETRIES {
        match self.connect_and_setup().await {
            Ok(_) => {
                info!("[{}] Connected on attempt {}", self.source_id, attempt);
                break;
            }
            Err(e) if attempt < MAX_INITIAL_RETRIES => {
                warn!(
                    "[{}] Connection attempt {}/{} failed: {}. Retrying in {:?}",
                    self.source_id, attempt, MAX_INITIAL_RETRIES, e, retry_delay
                );
                sleep(retry_delay).await;
                retry_delay = (retry_delay * 2).min(Duration::from_secs(30));
            }
            Err(e) => {
                error!(
                    "[{}] Failed to connect after {} attempts: {}",
                    self.source_id, MAX_INITIAL_RETRIES, e
                );
                return Err(e);
            }
        }
    }

    let mut keepalive_interval = interval(Duration::from_secs(10));
    let mut recovery_attempts = 0;
    let mut last_keepalive_success = Instant::now();

    loop {
        let status = self.status.read().await;
        if *status == ComponentStatus::Stopping || *status == ComponentStatus::Stopped {
            info!("[{}] Received stop signal", self.source_id);
            break;
        }
        drop(status);

        tokio::select! {
            result = self.read_next_message() => {
                match result {
                    Ok(Some(msg)) => {
                        recovery_attempts = 0;  // Reset on success
                        if let Err(e) = self.handle_message(msg).await {
                            error!("[{}] Error handling message: {}", self.source_id, e);
                            if let Err(e) = self.attempt_recovery(recovery_attempts).await {
                                error!("[{}] Recovery failed: {}", self.source_id, e);
                                return Err(e);
                            }
                            recovery_attempts += 1;
                        }
                    }
                    Ok(None) => { /* No message */ }
                    Err(e) => {
                        error!("[{}] Error reading message: {}", self.source_id, e);
                        recovery_attempts += 1;
                        if recovery_attempts > RECOVERY_RETRY_BUDGET {
                            error!("[{}] Recovery budget exhausted", self.source_id);
                            return Err(anyhow!("Connection recovery failed after {} attempts", recovery_attempts));
                        }
                        self.attempt_recovery(recovery_attempts).await?;
                    }
                }
            }

            _ = keepalive_interval.tick() => {
                match self.send_feedback(false).await {
                    Ok(_) => {
                        last_keepalive_success = Instant::now();
                        recovery_attempts = 0;
                    }
                    Err(e) => {
                        warn!(
                            "[{}] Failed to send keepalive: {}. Last success: {:?} ago",
                            self.source_id,
                            e,
                            last_keepalive_success.elapsed()
                        );
                        if last_keepalive_success.elapsed() > Duration::from_secs(60) {
                            error!("[{}] No keepalive success in 60 seconds, reconnecting", self.source_id);
                            recovery_attempts += 1;
                            self.attempt_recovery(recovery_attempts).await?;
                        }
                    }
                }
            }
        }
    }

    self.shutdown().await?;
    Ok(())
}

async fn attempt_recovery(&mut self, attempt: u32) -> Result<()> {
    let delay = Duration::from_millis(100 * 2u64.pow(attempt.min(5)));
    warn!(
        "[{}] Connection recovery attempt {}, waiting {:?}",
        self.source_id, attempt, delay
    );

    sleep(delay).await;

    self.recover_connection().await
}
```

---

## HIGH SEVERITY ISSUES

### Issue H1: Task Handle Overwritten Without Cleanup

**File**: `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/queries/manager.rs`
**Lines**: 348 (in context around spawn)
**Severity**: HIGH
**Category**: Resource Leak

**Problem**: When restarting a query component, the task handle is overwritten without checking if a previous task exists.

**Current Pattern**:
```rust
let task = tokio::spawn(async move { /* task work */ });
*self.base.task_handle.write().await = Some(task);  // ← Previous handle lost!
```

**Consequences**:
- Previous task continues running in background
- Memory leak from task handle
- Data consistency corruption if both instances write
- Can cascade to multiple instances of same component

**Recommended Pattern**:
```rust
// Before spawning new task, clean up old one
if let Some(old_task) = self.base.task_handle.write().await.take() {
    old_task.abort();
    // Wait briefly for abort to take effect
    tokio::time::sleep(Duration::from_millis(10)).await;
}

let task = tokio::spawn(async move { /* task work */ });
*self.base.task_handle.write().await = Some(task);
```

---

### Issue H2: HTTP Source Startup May Leak Resources

**File**: `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/sources/http/adaptive.rs`
**Lines**: 279-320+
**Severity**: HIGH
**Category**: Resource Cleanup

**Current Code Structure**:
```rust
async fn start(&self) -> Result<()> {
    info!("[{}] Starting adaptive HTTP source", self.base.config.id);

    self.base.set_status(ComponentStatus::Starting).await;

    let (host, port) = match &self.base.config.config {
        crate::config::SourceSpecificConfig::Http(http_config) => {
            (http_config.host.clone(), http_config.port)
        }
        _ => ("0.0.0.0".to_string(), 8080u16),
    };

    // Create batch channel
    let (batch_tx, batch_rx) = mpsc::channel(1000);

    // HTTP server setup happens next but error handling not shown
    // If binding fails here, what happens?
}
```

**Problems**:
- Axum router setup and binding not shown but likely unhandled
- If port already in use, error returned but batch_tx already created
- Spawned tasks may not be cleaned up on binding error
- Dispatcher registrations not rolled back on error

**Recommended Pattern**:
```rust
async fn start(&self) -> Result<()> {
    info!("[{}] Starting adaptive HTTP source", self.base.config.id);

    self.base.set_status(ComponentStatus::Starting).await;

    let (host, port) = self.extract_http_config()?;
    let (batch_tx, batch_rx) = mpsc::channel(1000);

    // Spawn batcher FIRST
    let dispatchers = self.base.dispatchers.clone();
    let adaptive_config = self.adaptive_config.clone();
    let source_id = self.base.config.id.clone();

    let batcher_task = tokio::spawn(Self::run_adaptive_batcher(
        batch_rx,
        dispatchers,
        adaptive_config,
        source_id,
    ));

    // TRY to bind HTTP server
    match self.create_and_bind_server(host, port, batch_tx).await {
        Ok(server_task) => {
            self.base.task_handle.write().await = Some(server_task);
            self.base.set_status(ComponentStatus::Running).await;
            Ok(())
        }
        Err(e) => {
            // ROLLBACK: Abort batcher task and return error
            batcher_task.abort();
            self.base.set_status(ComponentStatus::Stopped).await;
            Err(e)
        }
    }
}
```

---

### Issue H3: PostgreSQL Replication Shutdown No Cleanup

**File**: `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/sources/postgres/mod.rs`
**Lines**: 156-182
**Severity**: HIGH
**Category**: Resource Cleanup

**Current Code**:
```rust
async fn stop(&self) -> Result<()> {
    if self.base.get_status().await != ComponentStatus::Running {
        return Ok(());
    }

    info!(
        "Stopping PostgreSQL replication source: {}",
        self.base.config.id
    );

    self.base.set_status(ComponentStatus::Stopping).await;

    // Cancel the replication task
    if let Some(task) = self.base.task_handle.write().await.take() {
        task.abort();  // ← No cleanup of connection
    }

    self.base.set_status(ComponentStatus::Stopped).await;
    self.base
        .send_component_event(
            ComponentStatus::Stopped,
            Some("PostgreSQL replication stopped".to_string()),
        )
        .await?;

    Ok(())
}
```

**Problems**:
1. Replication connection not properly closed
2. Replication slot may remain held
3. WAL feedback position not updated
4. Connection may be left in middle of transaction
5. Dangling replication stream may keep consuming WAL

**Impact**:
- PostgreSQL disk space fills with unconsumed WAL
- Replication slot becomes obstacle to maintenance
- Next restart may see inconsistent slot state

**Recommended Fix**:
```rust
async fn stop(&self) -> Result<()> {
    if self.base.get_status().await != ComponentStatus::Running {
        return Ok(());
    }

    info!("Stopping PostgreSQL replication source: {}", self.base.config.id);
    self.base.set_status(ComponentStatus::Stopping).await;

    // Request graceful shutdown via status change
    // (component should periodically check status)

    if let Some(task) = self.base.task_handle.write().await.take() {
        // Wait for graceful shutdown with timeout
        select! {
            _ = sleep(Duration::from_secs(5)) => {
                warn!("PostgreSQL replication did not shut down gracefully, aborting");
                task.abort();
            }
            _ = &task => {
                debug!("PostgreSQL replication shut down gracefully");
            }
        }
    }

    self.base.set_status(ComponentStatus::Stopped).await;
    self.base
        .send_component_event(
            ComponentStatus::Stopped,
            Some("PostgreSQL replication stopped".to_string()),
        )
        .await?;

    Ok(())
}
```

---

## MEDIUM SEVERITY ISSUES

### Issue M1: Missing Timeouts on Bootstrap Operations

**File**: `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/bootstrap/providers/postgres.rs`
**Lines**: 151-200
**Severity**: MEDIUM (but can cause CRITICAL impact)
**Category**: Deadlock Prevention

**Current Pattern**:
```rust
async fn execute(&mut self, request: BootstrapRequest, ...) -> Result<usize> {
    info!("Bootstrap: Connecting to PostgreSQL...");

    // NO TIMEOUT HERE
    let mut client = self.connect().await?;

    // NO TIMEOUT HERE
    self.query_primary_keys(&client).await?;

    // NO TIMEOUT HERE
    let (transaction, lsn) = self.create_snapshot(&mut client).await?;

    // NO TIMEOUT HERE
    let tables = self.map_labels_to_tables(&request, &transaction).await?;

    for (label, table_name) in tables {
        // NO TIMEOUT HERE
        let count = self.bootstrap_table(&transaction, &label, &table_name, ...).await?;
        // ...
    }

    // NO TIMEOUT HERE
    transaction.commit().await?;
    Ok(total_count)
}
```

**Problems**:
1. Can hang indefinitely on network partition
2. Server startup blocked waiting for bootstrap
3. No way to kill hung bootstrap without process kill
4. Transaction resources held open indefinitely
5. PostgreSQL may reach max connection limit

**Recommended Fix**:
```rust
pub struct BootstrapConfig {
    pub connection_timeout: Duration,
    pub query_timeout: Duration,
    pub transaction_timeout: Duration,
    pub bootstrap_timeout: Duration,
}

impl Default for BootstrapConfig {
    fn default() -> Self {
        Self {
            connection_timeout: Duration::from_secs(10),
            query_timeout: Duration::from_secs(30),
            transaction_timeout: Duration::from_secs(60),
            bootstrap_timeout: Duration::from_secs(300),
        }
    }
}

async fn execute(&mut self, request: BootstrapRequest, ...) -> Result<usize> {
    let config = BootstrapConfig::default();

    let entire_bootstrap = async {
        info!("Bootstrap: Connecting to PostgreSQL...");

        // WITH TIMEOUT
        let mut client = timeout(config.connection_timeout, self.connect())
            .await
            .map_err(|_| anyhow!("Connection timeout"))?
            .context("Failed to connect during bootstrap")?;

        // WITH TIMEOUT
        timeout(config.query_timeout, self.query_primary_keys(&client))
            .await
            .map_err(|_| anyhow!("Primary keys query timeout"))?
            .context("Failed to query primary keys")?;

        // WITH TIMEOUT
        let (transaction, lsn) = timeout(
            config.query_timeout,
            self.create_snapshot(&mut client),
        )
        .await
        .map_err(|_| anyhow!("Snapshot creation timeout"))?
        .context("Failed to create snapshot")?;

        // ... rest of bootstrap with timeouts ...

        // WITH TIMEOUT
        timeout(config.query_timeout, transaction.commit())
            .await
            .map_err(|_| anyhow!("Transaction commit timeout"))?
            .context("Failed to commit bootstrap transaction")?;

        Ok(total_count)
    };

    // Wrap entire bootstrap with overall timeout
    timeout(config.bootstrap_timeout, entire_bootstrap)
        .await
        .map_err(|_| anyhow!("Bootstrap operation timed out after {:?}", config.bootstrap_timeout))?
}
```

---

### Issue M2: Forwarder Task Message Loss on Queue Full

**File**: `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/reactions/base.rs`
**Lines**: 148-182
**Severity**: MEDIUM
**Category**: Silent Data Loss

**Current Code**:
```rust
let forwarder_task = tokio::spawn(async move {
    loop {
        match receiver.recv().await {
            Ok(query_result) => {
                // Enqueue to priority queue for timestamp-ordered processing
                if !priority_queue.enqueue(query_result).await {
                    warn!(
                        "[{}] Failed to enqueue result from query '{}' - priority queue at capacity",
                        reaction_id, query_id_clone
                    );
                    // ← MESSAGE IS LOST! No retry, no backpressure
                }
            }
            Err(e) => {
                let error_str = e.to_string();
                if error_str.contains("lagged") {
                    warn!(
                        "[{}] Receiver lagged for query '{}': {}",
                        reaction_id, query_id_clone, error_str
                    );
                    continue;  // ← Just keeps going, missing events
                } else {
                    info!(
                        "[{}] Receiver error for query '{}': {}",
                        reaction_id, query_id_clone, error_str
                    );
                    break;  // ← Exits silently
                }
            }
        }
    }
});
```

**Problems**:
1. When priority queue is full, result is silently dropped
2. Lagging errors cause silent message loss
3. No metrics on message loss
4. Query continues sending but reactions fall behind
5. Detector has no way to know results were lost

**Recommended Fix**:
```rust
let forwarder_task = tokio::spawn(async move {
    let mut dropped_count = 0u64;
    let mut lagged_count = 0u64;
    let mut last_metrics_log = Instant::now();

    loop {
        match receiver.recv().await {
            Ok(query_result) => {
                // Try to enqueue with backpressure
                let timestamp = query_result.timestamp;
                match priority_queue.enqueue_with_backpressure(query_result).await {
                    Ok(_) => {
                        if dropped_count > 0 {
                            info!(
                                "[{}] Queue recovered, {} messages were dropped",
                                reaction_id, dropped_count
                            );
                            dropped_count = 0;
                        }
                    }
                    Err(_queue_full) => {
                        // Queue is full - apply backpressure
                        warn!(
                            "[{}] Priority queue full, applying backpressure to query '{}'",
                            reaction_id, query_id_clone
                        );
                        dropped_count += 1;

                        // Wait and retry
                        tokio::time::sleep(Duration::from_millis(10)).await;

                        // Try once more before giving up
                        if !priority_queue.enqueue(query_result).await {
                            error!(
                                "[{}] Dropping query result from '{}' - queue full for {} messages",
                                reaction_id, query_id_clone, dropped_count
                            );
                        }
                    }
                }
            }
            Err(broadcast::error::RecvError::Lagged(count)) => {
                lagged_count += count;
                warn!(
                    "[{}] Receiver lagged - missed {} messages from '{}'",
                    reaction_id, count, query_id_clone
                );
            }
            Err(broadcast::error::RecvError::Closed) => {
                info!(
                    "[{}] Query '{}' subscription closed. Stats: {} dropped, {} lagged",
                    reaction_id, query_id_clone, dropped_count, lagged_count
                );
                break;
            }
        }

        // Log metrics periodically
        if last_metrics_log.elapsed() > Duration::from_secs(60) {
            info!(
                "[{}] Query '{}' metrics - Dropped: {}, Lagged: {}",
                reaction_id, query_id_clone, dropped_count, lagged_count
            );
            dropped_count = 0;
            lagged_count = 0;
            last_metrics_log = Instant::now();
        }
    }
});
```

---

### Issue M3: Status Check-Then-Act Race Condition

**File**: `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/sources/manager.rs`
**Lines**: 201-216
**Severity**: MEDIUM
**Category**: Race Condition

**Current Code**:
```rust
pub async fn start_source(&self, id: String) -> Result<()> {
    let source = {
        let sources = self.sources.read().await;
        sources.get(&id).cloned()
    };  // ← LOCK RELEASED HERE

    if let Some(source) = source {
        let status = source.status().await;  // ← STATUS MAY HAVE CHANGED
        is_operation_valid(&status, &Operation::Start)
            .map_err(|e| anyhow!(e))?;
        source.start().await?;  // ← ANOTHER THREAD MAY HAVE STARTED THIS
    } else {
        return Err(anyhow!("Source not found: {}", id));
    }

    Ok(())
}
```

**Race Sequence**:
1. Thread A: Gets source, releases lock
2. Thread B: Gets source, checks status=Stopped, calls start()
3. Thread A: Checks status=Stopped (stale!), calls start() on already-running source
4. Result: Source started twice, duplicate subscriptions, data duplication

**Recommended Fix**:
```rust
pub async fn start_source(&self, id: String) -> Result<()> {
    // Keep lock during entire check-and-act sequence
    let sources = self.sources.read().await;
    let source = sources.get(&id).cloned()
        .ok_or_else(|| anyhow!("Source not found: {}", id))?;
    drop(sources);

    // Now check status and start atomically
    let status = source.status().await;
    is_operation_valid(&status, &Operation::Start)
        .map_err(|e| anyhow!(e))?;

    source.start().await
}
```

Or use atomic state machine:

```rust
pub async fn start_source(&self, id: String) -> Result<()> {
    let source = {
        let sources = self.sources.read().await;
        sources.get(&id).cloned()
    }.ok_or_else(|| anyhow!("Source not found: {}", id))?;

    // Use compare-and-swap pattern
    loop {
        let current_status = source.status().await;

        match current_status {
            ComponentStatus::Stopped => {
                // Try to transition to Starting
                if source.try_set_status(ComponentStatus::Starting).await {
                    // We successfully transitioned, now start
                    if let Err(e) = source.start().await {
                        let _ = source.try_set_status(ComponentStatus::Stopped).await;
                        return Err(e);
                    }
                    return Ok(());
                }
                // Status changed, retry
            }
            ComponentStatus::Running => {
                return Err(anyhow!("Source already running"));
            }
            ComponentStatus::Starting => {
                tokio::time::sleep(Duration::from_millis(10)).await;
                // Retry until it's fully running
            }
            _ => {
                return Err(anyhow!("Cannot start source in state: {:?}", current_status));
            }
        }
    }
}
```

---

## SUMMARY TABLE

| Issue ID | Severity | Category | File | Lines | Impact |
|----------|----------|----------|------|-------|--------|
| C1 | CRITICAL | Silent Failures | lifecycle.rs | 156-172 | Server becomes zombie |
| C2 | CRITICAL | Resource Cleanup | reactions/base.rs | 194-222 | Data loss, panic cascade |
| C3 | CRITICAL | Missing Recovery | sources/postgres/stream.rs | 91-150 | Connection loss = data loss |
| H1 | HIGH | Resource Leak | queries/manager.rs | 348 | Duplicate processing |
| H2 | HIGH | Resource Cleanup | sources/http/adaptive.rs | 279+ | Resource leak on error |
| H3 | HIGH | Resource Cleanup | sources/postgres/mod.rs | 156-182 | WAL accumulation |
| M1 | MEDIUM | Deadlock | bootstrap/providers/postgres.rs | 151-200 | Server hang |
| M2 | MEDIUM | Data Loss | reactions/base.rs | 148-182 | Silent message loss |
| M3 | MEDIUM | Race Condition | sources/manager.rs | 201-216 | Duplicate processing |

---

## Testing Strategy

For each issue, create integration tests that:

1. **Issue C1**: Verify event processor task health monitoring
   ```rust
   #[tokio::test]
   async fn test_event_processor_panic_detection() {
       // Arrange: Create server with monitored event processor
       // Act: Inject panic in event processor
       // Assert: Server detects and logs failure
   }
   ```

2. **Issue C2**: Verify graceful task shutdown
   ```rust
   #[tokio::test]
   async fn test_reaction_task_graceful_shutdown() {
       // Arrange: Reaction with pending events
       // Act: Stop reaction
       // Assert: Pending events logged, tasks cleaned up
   }
   ```

3. **Issue C3**: Verify connection recovery with backoff
   ```rust
   #[tokio::test]
   async fn test_postgres_connection_recovery_backoff() {
       // Arrange: Mock network failure
       // Act: Replication stream attempts recovery
       // Assert: Exponential backoff observed
   }
   ```

And so on for all critical issues.

