# Quick Reference: Critical Reliability Issues

**Generated**: November 2025
**Scope**: Drasi Server-Core (`server-core/src/`)

---

## CRITICAL (Fix Immediately)

### 1. Event Processor Silent Death
- **File**: `lifecycle.rs:156-172`
- **Problem**: Background task fails with zero notification
- **Fix**: Track task handle, implement panic guard
- **Impact**: Server appears running but is dead
- **Time to Fix**: 2 hours

### 2. Task Abort No Cleanup
- **File**: `reactions/base.rs:194-222`
- **Problem**: Aborts tasks without graceful shutdown
- **Fix**: Implement timeout-based graceful shutdown
- **Impact**: Data loss, panic cascade
- **Time to Fix**: 4 hours

### 3. PostgreSQL No Connection Recovery
- **File**: `sources/postgres/stream.rs:91-150`
- **Problem**: Single network error = permanent source failure
- **Fix**: Add exponential backoff retry logic
- **Impact**: Data loss on transient network issues
- **Time to Fix**: 6 hours

---

## HIGH (Fix This Week)

### 4. Task Handle Leak
- **File**: `queries/manager.rs:348`
- **Problem**: Task handle overwritten without cleanup
- **Fix**: Check for existing handle before overwrite
- **Impact**: Duplicate processing, memory leak
- **Time to Fix**: 1 hour

### 5. HTTP Binding No Error Cleanup
- **File**: `sources/http/adaptive.rs:279+`
- **Problem**: Startup failure doesn't clean resources
- **Fix**: Rollback spawned tasks on binding failure
- **Impact**: Resource leaks
- **Time to Fix**: 3 hours

### 6. PostgreSQL Shutdown No Cleanup
- **File**: `sources/postgres/mod.rs:156-182`
- **Problem**: Connection abandoned without proper close
- **Fix**: Send graceful shutdown signal before abort
- **Impact**: WAL accumulation, replication slot lock
- **Time to Fix**: 2 hours

---

## MEDIUM (Fix Next Sprint)

### 7. Missing Bootstrap Timeouts
- **File**: `bootstrap/providers/postgres.rs:151-200`
- **Problem**: Can hang forever on network partition
- **Fix**: Wrap operations with `tokio::time::timeout`
- **Impact**: Server startup hangs indefinitely
- **Time to Fix**: 4 hours

### 8. Queue Full Silent Drop
- **File**: `reactions/base.rs:148-182`
- **Problem**: Message loss when priority queue full
- **Fix**: Implement backpressure or metrics
- **Impact**: Silent data loss
- **Time to Fix**: 3 hours

### 9. Status Check-Then-Act Race
- **File**: `sources/manager.rs:201-216`
- **Problem**: Source can be started twice concurrently
- **Fix**: Extend lock for entire check-and-act
- **Impact**: Duplicate processing, inconsistency
- **Time to Fix**: 2 hours

---

## One-Page Action Plan

### Week 1 (Critical)
1. **Monday**: Fix event processor monitoring (lifecycle.rs)
2. **Tuesday**: Fix task abort with graceful shutdown (reactions/base.rs)
3. **Wednesday**: Fix PostgreSQL recovery backoff (postgres/stream.rs)
4. **Thursday**: Fix task handle leak (queries/manager.rs)
5. **Friday**: Testing and integration

### Week 2 (High)
1. **Monday**: Fix HTTP error cleanup (http/adaptive.rs)
2. **Tuesday**: Fix PostgreSQL shutdown (postgres/mod.rs)
3. **Wednesday-Friday**: Testing and validation

### Week 3 (Medium)
1. **Monday**: Fix bootstrap timeouts (bootstrap/providers/postgres.rs)
2. **Tuesday**: Fix queue overflow (reactions/base.rs)
3. **Wednesday**: Fix TOCTOU races (sources/manager.rs)
4. **Thursday-Friday**: Integration testing

---

## Testing Checklist

For each fix, verify:

- [ ] Task health monitoring working
- [ ] Graceful shutdown completes within timeout
- [ ] No resource leaks on error
- [ ] Connection recovery with exponential backoff
- [ ] No duplicate processing
- [ ] Backpressure prevents silent loss
- [ ] Race conditions eliminated
- [ ] Timeouts prevent indefinite hangs

---

## Detection Indicators

In production, watch for:

| Issue | Observable Signal |
|-------|-------------------|
| Event processor dead | No component events in logs |
| Task not cleaned | Memory usage growing |
| Connection failures | Source status flips frequently |
| Message loss | Results suddenly disappear |
| Duplicates | Same result appears twice |
| Hangs | Process not responding |
| Race conditions | Intermittent failures |

---

## Code Patterns to Audit

Search codebase for these patterns that indicate issues:

```rust
// DANGER: Task abort without graceful shutdown
task.abort();

// DANGER: Task spawn with no health tracking
tokio::spawn(async { /* work */ });

// DANGER: Check then act with gap
let status = item.status().await;
// ... gap where status can change ...
item.start().await;

// DANGER: Resource error without cleanup
if let Err(e) = setup_task1() {
    return Err(e);  // task2 was already spawned!
}

// DANGER: Result/error silently ignored
let _ = operation().await;

// DANGER: Operations with no timeout
connection.query().await?;

// DANGER: Overwriting handles without cleanup
self.handle = Some(new_handle);  // old_handle lost!
```

---

## Monitoring Recommendations

Add these metrics/logs:

1. **Task Lifecycle**
   - Tasks spawned (counter)
   - Tasks completed (counter)
   - Task panic count (counter)
   - Task duration (histogram)

2. **Connection Health**
   - Connection attempts (counter)
   - Connection failures (counter)
   - Connection recovery attempts (counter)
   - Connection recovery delay (histogram)

3. **Message Flow**
   - Messages received (counter)
   - Messages processed (counter)
   - Messages dropped (counter)
   - Queue depth (gauge)

4. **Resource Usage**
   - Active tasks (gauge)
   - Memory per component (gauge)
   - Connection count (gauge)

---

## Emergency Mitigation

If critical issues occur in production:

1. **Event Processor Dead**
   - Restart server immediately
   - Monitor for other services depending on events

2. **Connection Loss**
   - Trigger manual replication slot check
   - Verify WAL disk usage
   - Consider graceful restart

3. **Message Backlog**
   - Check queue depth metrics
   - Identify slow consumer
   - Consider scaling reaction processing

4. **Duplicate Processing**
   - Check for concurrent starts
   - Look for task handle overwrites
   - Restart affected components

---

## Files Requiring Changes

```
server-core/src/
├── lifecycle.rs                    ← Event processor monitoring
├── reactions/base.rs               ← Task abort, queue overflow
├── queries/manager.rs              ← Task handle leak, TOCTOU
├── sources/
│   ├── manager.rs                  ← TOCTOU races
│   ├── postgres/
│   │   ├── stream.rs               ← Connection recovery
│   │   └── mod.rs                  ← Shutdown cleanup
│   └── http/adaptive.rs            ← Error cleanup
└── bootstrap/providers/postgres.rs ← Timeouts
```

**Total Files to Change**: 9
**Estimated LOC**: 500-800 (additions + modifications)
**Estimated Effort**: 3-4 weeks full-time

---

## Related Documentation

- `RELIABILITY_ANALYSIS.md` - Full analysis with detailed explanations
- `DETAILED_FINDINGS.md` - Code examples and recommended fixes
- This file - Quick reference for action items

