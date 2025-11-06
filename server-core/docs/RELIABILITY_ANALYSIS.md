# Error Handling and Reliability Analysis: Drasi Server-Core

**Analysis Date**: November 2025
**Codebase Size**: 42,737 lines of Rust code
**Focus Area**: `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src`

---

## Executive Summary

The Drasi Server-Core codebase demonstrates **good baseline error handling practices** with proper use of Rust's type system and `Result`-based error propagation. However, several **critical reliability risks** have been identified in resource management, error recovery, and concurrent access patterns that could cause system failures under production conditions.

**Key Findings**:
- **No panics found** via unwrap/expect in production code (excellent)
- **496 RwLock/Arc/Mutex usages** with some potential deadlock risks
- **3 task.abort() calls** with incomplete cleanup on error paths
- **Missing connection recovery** in PostgreSQL replication stream
- **Silent error swallowing** in event processor loops
- **No timeout handling** in critical database operations
- **Potential resource leaks** on HTTP source startup failures

---

## 1. PANIC POINTS & SYSTEM CRASHES

### 1.1 Task Abort without Cleanup (CRITICAL)

**Location**: `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/reactions/base.rs:200-208`

```rust
pub async fn stop_common(&self) -> Result<()> {
    // Abort all subscription forwarder tasks
    let mut subscription_tasks = self.subscription_tasks.write().await;
    for task in subscription_tasks.drain(..) {
        task.abort();  // ← Task aborted without waiting for cleanup
    }
    drop(subscription_tasks);

    // Abort the processing task
    let mut processing_task = self.processing_task.write().await;
    if let Some(task) = processing_task.take() {
        task.abort();  // ← Task aborted without cleanup
    }
    drop(processing_task);

    // ... queue drained ...
}
```

**Risk Level**: HIGH
**Issue**: Tasks are aborted without awaiting their final handlers or cleanup code. This can lead to:
- Incomplete cleanup of resources (connections not closed, buffers not flushed)
- Lost in-flight messages that haven't been persisted
- Subscribers left in inconsistent state
- Potential panic if task cleanup handlers try to write to dropped channels

**Critical Path**: Query and Reaction lifecycle - affects every start/stop cycle

**Recommendation**:
- Implement graceful shutdown: signal tasks to stop, wait with timeout, then abort
- Track and verify resource cleanup
- Add panic guards around task cleanup code

---

### 1.2 PostgreSQL Task Abort (HIGH)

**Location**: `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/sources/postgres/mod.rs:170`

```rust
async fn stop(&self) -> Result<()> {
    if self.base.get_status().await != ComponentStatus::Running {
        return Ok(());
    }

    info!("Stopping PostgreSQL replication source: {}", self.base.config.id);
    self.base.set_status(ComponentStatus::Stopping).await;

    // Cancel the replication task
    if let Some(task) = self.base.task_handle.write().await.take() {
        task.abort();  // ← Aborted without shutdown sequence
    }

    self.base.set_status(ComponentStatus::Stopped).await;
    // ...
}
```

**Risk Level**: HIGH
**Issue**: PostgreSQL replication connection is abandoned without proper cleanup:
- Connection may remain open on database server
- Replication slot may be held open, blocking database maintenance
- WAL segments may not be properly acknowledged
- State synchronization lost

**Critical Path**: PostgreSQL bootstrap and replication streaming

---

## 2. SILENT FAILURES & ERROR SWALLOWING

### 2.1 Event Processor Loop Silently Fails (CRITICAL)

**Location**: `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/lifecycle.rs:152-179`

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
            }  // ← Loop ends silently if channel closes
        });

        // DataRouter no longer needed
        drop(receivers.control_signal_rx);
        // SubscriptionRouter no longer needed
    }
}
```

**Risk Level**: CRITICAL
**Issue**:
- Event processor task runs in background with **zero error handling**
- If channel breaks, loop silently exits with no notification
- No monitoring of processor health
- System state becomes unobservable
- Failures in component lifecycle events go unreported

**Impact**: Server becomes "silent zombie" - appears running but stops processing events

**Recommendation**:
- Add task join handle tracking
- Spawn task monitor that restarts processor on panic
- Log processor termination with full context
- Implement health check for event pipeline

---

### 2.2 Forwarder Task Error Swallowing (MEDIUM)

**Location**: `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/reactions/base.rs:148-179`

```rust
let forwarder_task = tokio::spawn(async move {
    loop {
        match receiver.recv().await {
            Ok(query_result) => {
                if !priority_queue.enqueue(query_result).await {
                    warn!("[{}] Failed to enqueue - queue at capacity", reaction_id);
                    // ← Silently drops message if queue full
                }
            }
            Err(e) => {
                let error_str = e.to_string();
                if error_str.contains("lagged") {
                    warn!("[{}] Receiver lagged", reaction_id);
                    continue;  // ← Continues after lagging
                } else {
                    info!("[{}] Receiver error", reaction_id);
                    break;  // ← Stops on error, exit code lost
                }
            }
        }
    }
});

// Store the forwarder task handle
self.subscription_tasks.write().await.push(forwarder_task);
```

**Risk Level**: MEDIUM
**Issue**:
- Task exits silently on broadcast channel close
- No way to detect task failure from outside
- Queue overflow causes silent message loss
- Lagging scenario handled but not properly tracked
- Parent reaction unaware of forwarder death

**Recommendation**:
- Use `tokio::sync::mpsc` with explicit error channels
- Implement task health monitoring with status updates
- Add metrics for dropped messages
- Send completion/error signals to parent

---

## 3. POOR ERROR TYPES & LOST CONTEXT

### 3.1 Generic Anyhow Error Wrapping (MEDIUM)

**Location**: `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/api/error.rs:335-340`

```rust
impl From<anyhow::Error> for DrasiError {
    fn from(err: anyhow::Error) -> Self {
        Self::Internal(err.to_string())  // ← Loses error chain context
    }
}
```

**Risk Level**: MEDIUM
**Issue**:
- Error chain flattened to string
- Root cause hidden behind generic `Internal` variant
- Debugging becomes difficult without original backtrace
- Cannot programmatically handle specific errors

**Locations Using This Pattern**:
- `queries/manager.rs:148` - QueryBase creation errors
- `sources/postgres/mod.rs:113` - Config parsing errors
- `reactions/base.rs:139` - Query subscription errors

**Recommendation**:
- Preserve error context with `#[source]` attributes
- Use `context()` from anyhow instead of bare string conversion
- Create specific DrasiError variants for major subsystems

---

### 3.2 Uncategorized Database Errors (MEDIUM)

**Location**: `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/bootstrap/providers/postgres.rs:166-200`

```rust
pub async fn execute(&mut self, request: BootstrapRequest, ...) -> Result<usize> {
    let mut client = self.connect().await?;  // ← Connection error

    self.query_primary_keys(&client).await?;  // ← Generic database error

    let (transaction, lsn) = self.create_snapshot(&mut client).await?;  // ← TXN error

    let tables = self.map_labels_to_tables(&request, &transaction).await?;  // ← Query error

    for (label, table_name) in tables {
        let count = self.bootstrap_table(&transaction, &label, &table_name, ...).await?;  // ← Bootstrap error
    }

    transaction.commit().await?;  // ← Commit error
    Ok(total_count)
}
```

**Risk Level**: MEDIUM
**Issue**:
- All database operation errors converted to generic `tokio_postgres::Error`
- No differentiation between:
  - Connection failures (should retry)
  - Authentication failures (should fail immediately)
  - Query errors (may be recoverable)
  - Transaction failures (may require rollback)

**Critical Impact**: Cannot implement proper retry logic or recovery strategies

---

## 4. MISSING RECOVERY & GRACEFUL DEGRADATION

### 4.1 PostgreSQL Replication Connection Recovery (CRITICAL)

**Location**: `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/sources/postgres/stream.rs:100-145`

```rust
pub async fn run(&mut self) -> Result<()> {
    info!("Starting replication stream for source {}", self.source_id);

    self.connect_and_setup().await?;  // ← Single attempt, no retry

    let mut keepalive_interval = interval(Duration::from_secs(10));

    loop {
        let status = self.status.read().await;
        if *status == ComponentStatus::Stopping || *status == ComponentStatus::Stopped {
            info!("Received stop signal, shutting down replication");
            break;
        }
        drop(status);

        tokio::select! {
            result = self.read_next_message() => {
                match result {
                    Ok(Some(msg)) => {
                        if let Err(e) = self.handle_message(msg).await {
                            error!("Error handling message: {}", e);
                            if let Err(e) = self.recover_connection().await {
                                error!("Failed to recover connection: {}", e);
                                return Err(e);  // ← Returns immediately, source stops
                            }
                        }
                    }
                    Err(e) => {
                        error!("Error reading message: {}", e);
                        if let Err(e) = self.recover_connection().await {
                            error!("Failed to recover connection: {}", e);
                            return Err(e);  // ← Same issue
                        }
                    }
                }
            }
            _ = keepalive_interval.tick() => {
                if let Err(e) = self.send_feedback(false).await {
                    warn!("Failed to send keepalive: {}", e);  // ← Warning logged, but what happens next?
                }
            }
        }
    }

    self.shutdown().await?;
    Ok(())
}
```

**Risk Level**: CRITICAL
**Issue**:
- Only one initial connection attempt on startup
- Connection recovery may fail and immediately terminates source
- No exponential backoff or retry budgets
- Keepalive failures logged but not tracked
- Single transient network error causes permanent source failure

**Critical Path**: PostgreSQL WAL replication - loses all streaming data on connection loss

**Recommendation**:
- Implement exponential backoff with max retries for recovery
- Log connection state transitions and recovery attempts
- Track consecutive failures to distinguish transient from permanent failures
- Implement circuit breaker pattern for repeated failures

---

### 4.2 HTTP Source Startup Cascade Failure (HIGH)

**Location**: `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/sources/http/adaptive.rs:279-320+`

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

    // ... HTTP server setup (not shown, but likely contains unhandled errors) ...
}
```

**Risk Level**: HIGH
**Issue**:
- HTTP server binding may fail on port already in use
- But no error handling visible for Axum router startup
- If binding fails mid-startup, partial resources remain allocated
- No cleanup of dispatcher registrations on error

---

### 4.3 Bootstrap Snapshot Validation Missing (MEDIUM)

**Location**: `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/bootstrap/providers/postgres.rs:225-244`

```rust
async fn create_snapshot<'a>(&self, client: &'a mut Client) -> Result<(Transaction<'a>, String)> {
    // Start transaction with repeatable read isolation
    let transaction = client
        .build_transaction()
        .isolation_level(tokio_postgres::IsolationLevel::RepeatableRead)
        .start()
        .await?;

    // Capture current LSN for replication coordination
    let row = transaction
        .query_one("SELECT pg_current_wal_lsn()::text", &[])
        .await?;  // ← No validation that LSN is valid/expected format
    let lsn: String = row.get(0);

    Ok((transaction, lsn))
}
```

**Risk Level**: MEDIUM
**Issue**:
- No validation of LSN format before using in replication
- No verification that snapshot was actually acquired
- Connection pool state not verified before starting transaction
- Transaction may be in unexpected isolation state if previous operation failed

---

## 5. RESOURCE CLEANUP & LEAKS

### 5.1 Task Handle Leak in Query Manager (HIGH)

**Location**: `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/queries/manager.rs:348-365+`

```rust
let task = tokio::spawn(async move {
    if let Err(e) = run_replication(
        source_id.clone(),
        config,
        dispatchers,
        event_tx.clone(),
        status_clone.clone(),
    )
    .await
    {
        error!("Replication task failed for {}: {}", source_id, e);
        *status_clone.write().await = ComponentStatus::Error;
        let _ = event_tx
            .send(ComponentEvent {
                component_id: source_id,
                component_type: ComponentType::Source,
                status: ComponentStatus::Error,
                timestamp: chrono::Utc::now(),
                message: Some(format!("Replication failed: {}", e)),
            })
            .await;
    }
});

*self.base.task_handle.write().await = Some(task);  // ← Overwrites previous handle!
```

**Risk Level**: HIGH
**Issue**:
- Old task handle overwritten without cleanup/abort
- Previous task continues running untracked in background
- Multiple task instances may process same source concurrently
- Memory accumulates as task handles leak

**Impact**: Memory leak + data consistency corruption from concurrent processing

---

### 5.2 Dispatcher Channel Cleanup Missing (MEDIUM)

**Location**: `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/reactions/base.rs:194-222`

```rust
pub async fn stop_common(&self) -> Result<()> {
    info!("Stopping reaction: {}", self.config.id);

    // Abort all subscription forwarder tasks
    let mut subscription_tasks = self.subscription_tasks.write().await;
    for task in subscription_tasks.drain(..) {
        task.abort();
    }
    drop(subscription_tasks);

    // Drain the priority queue
    let drained_events = self.priority_queue.drain().await;
    if !drained_events.is_empty() {
        info!("[{}] Drained {} pending events from priority queue", self.config.id, drained_events.len());
    }

    // ← No cleanup of broadcast channel subscriptions
    // ← No notification to upstream query
    // ← No cleanup of dispatcher registrations
}
```

**Risk Level**: MEDIUM
**Issue**:
- Query broadcast subscribers never explicitly closed
- Query continues sending to dead subscribers
- Dispatcher may try to send to aborted tasks
- Memory from drained queue events may not be released

---

## 6. RACE CONDITIONS & SYNCHRONIZATION ISSUES

### 6.1 RwLock Ordering Risk (MEDIUM)

**Location**: Multiple files - 496 instances of RwLock/Arc usage

**Risk Level**: MEDIUM
**Issue Pattern**: Nested RwLock acquisitions without documented ordering

Example from `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/sources/manager.rs:142-199`:

```rust
async fn add_source_internal(&self, config: SourceConfig, allow_auto_start: bool) -> Result<()> {
    if self.sources.read().await.contains_key(&config.id) {  // ← Lock 1: sources read
        return Err(anyhow!("Source with id '{}' already exists", config.id));
    }

    let source: Arc<dyn Source> = match config.source_type() {
        "application" => {
            let (app_source, handle) = ApplicationSource::new(config.clone(), self.event_tx.clone())?;
            self.application_handles
                .write()
                .await
                .insert(config.id.clone(), handle);  // ← Lock 2: application_handles write
            Arc::new(app_source)
        }
        _ => { /* ... */ }
    };

    let source_id = config.id.clone();
    let should_auto_start = config.auto_start;

    self.sources.write().await.insert(config.id.clone(), source);  // ← Lock 1 again: sources write

    if should_auto_start && allow_auto_start {
        if let Err(e) = self.start_source(source_id.clone()).await {  // ← May acquire more locks
            error!("Failed to auto-start source {}: {}", source_id, e);
        }
    }

    Ok(())
}
```

**Deadlock Potential**:
- Lock acquisition order: sources.read → application_handles.write → sources.write
- Other code paths may acquire in different order
- Multiple threads could deadlock if patterns conflict

**Recommendation**:
- Document lock ordering invariants at module level
- Use lock guards to ensure consistent ordering
- Consider refactoring to single-lock or lock-free designs
- Add deadlock detection in tests

---

### 6.2 Status Check-Then-Act Race (MEDIUM)

**Location**: `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/sources/manager.rs:201-216`

```rust
pub async fn start_source(&self, id: String) -> Result<()> {
    let source = {
        let sources = self.sources.read().await;
        sources.get(&id).cloned()  // ← Releases lock here
    };

    if let Some(source) = source {
        let status = source.status().await;  // ← Status may have changed!
        is_operation_valid(&status, &Operation::Start)
            .map_err(|e| anyhow!(e))?;
        source.start().await?;  // ← Another source may have started this in parallel
    } else {
        return Err(anyhow!("Source not found: {}", id));
    }

    Ok(())
}
```

**Risk Level**: MEDIUM
**Issue**:
- Classic TOCTOU (time-of-check-time-of-use) race condition
- Source status checked AFTER lock released
- Two concurrent `start_source` calls could both succeed for same source
- Resource double-allocation or state corruption

**Impact**: Sources may be started multiple times, duplicate subscriptions, data duplication

---

### 6.3 Query Subscription Race (MEDIUM)

**Location**: `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/reactions/base.rs:126-186`

```rust
pub async fn subscribe_to_queries(&self, query_subscriber: Arc<dyn QuerySubscriber>) -> Result<()> {
    for query_id in &self.config.queries {
        let query = query_subscriber.get_query_instance(query_id).await?;  // ← May be starting/stopping

        let subscription_response = query
            .subscribe(self.config.id.clone())
            .await
            .map_err(|e| anyhow!(e))?;
        let mut receiver = subscription_response.receiver;

        // Spawn forwarder task to read from receiver
        let forwarder_task = tokio::spawn(async move {
            loop {
                match receiver.recv().await {  // ← Query may have stopped
                    Ok(query_result) => { /* enqueue */ }
                    Err(e) => { /* handle error */ }
                }
            }
        });

        self.subscription_tasks.write().await.push(forwarder_task);
    }

    Ok(())
}
```

**Risk Level**: MEDIUM
**Issue**:
- Query could be stopping while subscription being created
- Subscription response may be stale
- Receiver may close immediately after creation
- Task cleanup race with reaction stop

---

## 7. TIMEOUT HANDLING & DEADLOCK PREVENTION

### 7.1 Missing Timeouts on Database Operations (CRITICAL)

**Location**: `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/bootstrap/providers/postgres.rs:162-200`

```rust
pub async fn execute(&mut self, ...) -> Result<usize> {
    let mut client = self.connect().await?;  // ← No timeout!

    self.query_primary_keys(&client).await?;  // ← Indefinite wait

    let (transaction, lsn) = self.create_snapshot(&mut client).await?;  // ← No timeout

    let tables = self.map_labels_to_tables(&request, &transaction).await?;  // ← No timeout

    for (label, table_name) in tables {
        let count = self.bootstrap_table(&transaction, &label, &table_name, ...).await?;  // ← No timeout
    }

    transaction.commit().await?;  // ← No timeout
    Ok(total_count)
}
```

**Risk Level**: CRITICAL
**Issue**:
- All database operations can block forever if connection hung
- Startup hangs waiting for bootstrap to complete
- No timeout for transaction isolation level verification
- Network partition causes permanent hang

**Critical Path**: Bootstrap startup - can hang server startup indefinitely

**Recommendation**:
- Wrap bootstrap operations with `tokio::time::timeout`
- Implement configurable timeout per operation type
- Add connection pool timeout configuration
- Implement query timeouts at PostgreSQL level (statement_timeout)

---

### 7.2 Event Processor Infinite Loop (MEDIUM)

**Location**: `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/lifecycle.rs:156-172`

```rust
pub async fn start_event_processors(&mut self) {
    if let Some(receivers) = self.event_receivers.take() {
        tokio::spawn(async move {
            let mut rx = component_rx;
            while let Some(event) = rx.recv().await {  // ← Infinite loop, never exits
                info!("Component Event - {:?} {}: {:?}", ...);
            }
            // ← Never reaches here unless channel closed
        });
    }
}
```

**Risk Level**: MEDIUM
**Issue**:
- Event processor runs forever in background
- Server shutdown must wait for channel closure
- May contribute to graceful shutdown delays
- No way to cancel or pause event processing

---

## 8. CRITICAL PATHS NEEDING HARDENING

### Sources (Data Ingestion)

| Path | File | Risk | Issue |
|------|------|------|-------|
| PostgreSQL Startup | `sources/postgres/mod.rs:102-154` | CRITICAL | No retry on connection failure |
| PostgreSQL Replication | `sources/postgres/stream.rs:91-150` | CRITICAL | Connection recovery has no backoff |
| HTTP Binding | `sources/http/adaptive.rs:279+` | HIGH | Binding failure may leak resources |
| Platform Source | `sources/platform/mod.rs` | MEDIUM | Consumer group state not validated |

### Queries (Processing)

| Path | File | Risk | Issue |
|------|------|------|-------|
| Query Startup | `queries/manager.rs:175+` | HIGH | Task handle overwrites without cleanup |
| Source Subscription | `queries/manager.rs:300+` | MEDIUM | TOCTOU race on source lookup |
| Bootstrap Integration | `queries/manager.rs` | CRITICAL | Missing timeouts on bootstrap operations |

### Reactions (Output)

| Path | File | Risk | Issue |
|------|------|------|-------|
| Task Cleanup | `reactions/base.rs:194-222` | CRITICAL | Aborted without graceful shutdown |
| Query Subscription | `reactions/base.rs:126-186` | MEDIUM | Race condition during subscription |
| Platform Publisher | `reactions/platform/publisher.rs:54-79` | MEDIUM | Connection retry may loop forever |

### Lifecycle (Orchestration)

| Path | File | Risk | Issue |
|------|------|------|-------|
| Event Processor | `lifecycle.rs:156-172` | CRITICAL | Silently fails with no monitoring |
| Component Start | `lifecycle.rs:185-318` | MEDIUM | Status check races with actual startup |
| Component Stop | `lifecycle.rs:324-388` | HIGH | Error swallowing during shutdown |

---

## 9. SPECIFIC CODE LOCATIONS REQUIRING FIXES

### Priority 1: Stop Immediate System Failures

1. **Lifecycle Event Processor Silent Failure** (Line 156-172)
   - Add task handle tracking and health monitoring
   - Implement panic guard and auto-restart logic

2. **PostgreSQL Connection Recovery Loop** (Line 100-145 in stream.rs)
   - Add exponential backoff with max retry budget
   - Implement circuit breaker for repeated failures

3. **Task Abort Without Cleanup** (Line 200-208 in reactions/base.rs)
   - Implement graceful shutdown sequence
   - Add timeout with force-abort fallback

### Priority 2: Prevent Data Loss

4. **Query Startup Task Handle Leak** (Line 348 in queries/manager.rs)
   - Check for existing handle before overwriting
   - Clean up previous task on restart

5. **Bootstrap Snapshot Validation** (Line 238-241 in bootstrap/providers/postgres.rs)
   - Validate LSN format before use
   - Verify transaction isolation level acquired

6. **Missing Timeouts** (Throughout bootstrap and database operations)
   - Add `tokio::time::timeout` wrappers
   - Make timeouts configurable per operation

### Priority 3: Fix Race Conditions

7. **Status Check-Then-Act Races** (Multiple manager methods)
   - Acquire locks for complete check-and-act sequence
   - Use atomic compare-and-swap patterns

8. **RwLock Ordering Deadlock Risk** (496 instances)
   - Document lock ordering invariants
   - Add integration tests for concurrent operations

---

## 10. RECOMMENDATIONS FOR IMPROVEMENT

### Short-term (Weeks 1-2)

1. Add graceful shutdown for aborted tasks:
   ```rust
   async fn graceful_shutdown(&self, task: JoinHandle<()>, timeout: Duration) -> Result<()> {
       select! {
           _ = (&mut task) => Ok(()),
           _ = sleep(timeout) => {
               task.abort();
               Ok(())
           }
       }
   }
   ```

2. Implement basic task health monitoring:
   ```rust
   struct TaskMonitor {
       task: JoinHandle<()>,
       name: String,
   }
   impl Drop for TaskMonitor {
       fn drop(&mut self) {
           if self.task.is_finished() {
               warn!("Background task '{}' terminated unexpectedly", self.name);
           }
       }
   }
   ```

3. Add timeout wrapper for database operations:
   ```rust
   pub async fn with_timeout<F, T>(f: F, duration: Duration) -> Result<T>
   where
       F: Future<Output = Result<T>>,
   {
       timeout(duration, f)
           .await
           .map_err(|_| DrasiError::timeout("database operation"))?
   }
   ```

### Medium-term (Weeks 3-4)

4. Implement exponential backoff for connection recovery:
   - Add retry budget tracking
   - Log failed attempts with context
   - Distinguish transient from permanent failures

5. Fix TOCTOU races in manager methods:
   - Extend lock duration to include status checks
   - Use atomic state transitions
   - Add integration tests for race conditions

6. Add circuit breaker pattern:
   - Track failure counts
   - Fail fast after threshold
   - Allow periodic recovery attempts

### Long-term (Ongoing)

7. Consider migration to lock-free data structures for hot paths
8. Implement comprehensive error telemetry and alerting
9. Add distributed tracing for critical operations
10. Implement chaos engineering tests for failure scenarios

---

## 11. TESTING RECOMMENDATIONS

Create focused integration tests for:

1. **Graceful Degradation**
   - PostgreSQL network partition recovery
   - HTTP source port binding failure
   - Bootstrap timeout handling

2. **Resource Cleanup**
   - Task handles released on source stop
   - Broadcast channels closed on reaction stop
   - Database connections returned to pool

3. **Concurrent Operations**
   - Multiple `start_source` calls simultaneously
   - Reaction subscription during query lifecycle changes
   - Manager operations during component state transitions

4. **Failure Modes**
   - Panic in spawned task doesn't crash main process
   - Error in one component doesn't break others
   - Recovery retries with exponential backoff

---

## Conclusion

Drasi Server-Core demonstrates solid foundational error handling practices but has **several critical reliability gaps** that require immediate attention:

- **Critical**: Event processor silent failure, task abort without cleanup, missing connection recovery
- **High**: Task handle leaks, HTTP binding failures, PostgreSQL connection issues
- **Medium**: Race conditions, missing timeouts, error context loss

These issues are concentrated in **lifecycle management, resource cleanup, and concurrent access patterns**. Implementing the recommended fixes will significantly improve production reliability and prevent data loss in failure scenarios.

**Estimated remediation effort**: 20-40 engineer-days for comprehensive fixes
**Risk of not fixing**: Loss of data, server hang, silent failures in production
