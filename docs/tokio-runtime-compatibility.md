# Tokio Runtime Compatibility Analysis

## Executive Summary

**Can drasi-core work with a single-threaded tokio runtime?**

**Answer:** No, not currently. The project has deep dependencies on multi-threaded runtime features that would cause panics if run on a single-threaded runtime.

**Key Blockers:**
- 36 critical `spawn_blocking` operations in storage layers
- 1 `block_in_place` call in source base
- Architecture designed for concurrent task execution

---

## Current State

### Multi-Threaded Dependencies

The project explicitly requires multi-threaded runtime features:

```toml
# lib/Cargo.toml
tokio = { version = "1.0", features = ["rt-multi-thread", "sync", "time", "macros"] }

# core/Cargo.toml
tokio = { version = "1.29.1", features = ["rt-multi-thread", "sync", "time", "macros"] }
```

### Statistics

- **77 instances** of `tokio::spawn()` for concurrent task processing
- **36 instances** of `tokio::task::spawn_blocking()` for storage I/O
- **17 tests** explicitly using `#[tokio::test(flavor = "multi_thread")]`
- **1 test** using `#[tokio::test(flavor = "current_thread")]` (isolated log worker)
- **1 instance** of `tokio::task::block_in_place()` (requires multi-threaded runtime)

---

## Critical Issues for Single-Threaded Runtime

### 1. Storage Layer Blocking Operations

**RocksDB Index** (`components/indexes/rocksdb/`):
All database operations use `spawn_blocking` because RocksDB is a synchronous C++ library:

```rust
// element_index.rs:127
let task = task::spawn_blocking(move || {
    let stored = get_element_internal(context.clone(), &element_key)?;
    // ... synchronous RocksDB operations
});
```

**Impact:** On single-threaded runtime, `spawn_blocking` will panic:
```
cannot spawn blocking task on current-thread runtime
```

**Affected Files:**
- `components/indexes/rocksdb/src/element_index.rs`: 8 occurrences
- `components/indexes/rocksdb/src/archive_index.rs`: 5 occurrences  
- `components/indexes/rocksdb/src/result_index.rs`: 9 occurrences
- `components/indexes/rocksdb/src/future_queue.rs`: 6 occurrences
- `components/state_stores/redb/src/provider.rs`: 14 occurrences

### 2. Synchronous-to-Async Bridge

**Location:** `lib/src/sources/base.rs:536`

```rust
tokio::task::block_in_place(|| {
    tokio::runtime::Handle::current().block_on(self.create_streaming_receiver())
})
```

**Impact:** `block_in_place` only works on multi-threaded runtime. It moves the current task off the thread so it can block without blocking the executor. Single-threaded runtime will panic.

### 3. Concurrent Task Architecture

The system spawns many independent concurrent tasks:
- **Per-query event processors** (`lib/src/queries/manager.rs:528`)
- **Per-source forwarders** (`lib/src/sources/base.rs:431`)
- **Per-reaction processors** (`lib/src/reactions/common/base.rs:349, 663, 693`)
- **HTTP/gRPC servers** with concurrent request handlers (`components/reactions/sse/src/sse.rs:506, 525`)
- **Priority queue workers** (`lib/src/channels/priority_queue.rs:526, 566, 599`)

**Impact:** While these could technically run via cooperative multitasking on a single thread, the architecture assumes true parallelism for performance. Single-threaded execution would create severe bottlenecks.

### 4. Potential Deadlock Scenarios

**Broadcast Channels:** The system uses `tokio::sync::broadcast` extensively. On single-threaded runtime:
- If a broadcast sender blocks waiting for slow receivers
- And receivers need CPU time to process (but can't get scheduled)
- **Deadlock occurs**

**Example:**
```rust
// lib/src/queries/manager.rs:541
priority_queue.enqueue_wait(arc_event).await;  // Can block if queue is full
```

---

## Solution Approaches

### Approach 1: Async Storage Abstraction (Recommended for Single-Threaded Support)

**Design:** Replace `spawn_blocking` with a dedicated thread pool independent of tokio runtime type.

**Implementation:**
```rust
use once_cell::sync::Lazy;
use threadpool::ThreadPool;

// Dedicated blocking pool independent of tokio runtime
static STORAGE_POOL: Lazy<ThreadPool> = Lazy::new(|| {
    ThreadPool::new(num_cpus::get())
});

// Replace spawn_blocking calls
async fn async_db_operation<T>(db_op: impl FnOnce() -> T + Send + 'static) -> T 
where
    T: Send + 'static,
{
    let (tx, rx) = tokio::sync::oneshot::channel();
    STORAGE_POOL.execute(move || {
        let result = db_op();
        let _ = tx.send(result);
    });
    rx.await.expect("Storage operation failed")
}
```

**Changes Required:**
1. Add dependencies: `threadpool`, `once_cell`, `num_cpus`
2. Create `async-trait` based storage abstraction
3. Replace all 36 `spawn_blocking` calls with custom async wrapper
4. Replace `block_in_place` with alternative approach
5. Update all tests to verify single-threaded compatibility

**Pros:**
- Works with any tokio runtime type
- Maintains performance for blocking operations
- Minimal changes to business logic
- Compatible with existing storage backends

**Cons:**
- Additional dependency complexity
- Need to manage thread pool lifecycle
- Requires careful migration

**Estimated Effort:** 1-2 weeks (one developer)

### Approach 2: Fully Async Storage Backend

**Design:** Replace RocksDB/Redb with async-native database (e.g., `sled`, `surrealdb`).

**Pros:**
- Native async, no blocking operations
- Simpler runtime requirements
- Better integration with async ecosystem

**Cons:**
- **Major breaking change** to storage layer
- Database migration for existing users
- May sacrifice RocksDB performance/maturity
- Alternative databases may lack required features

**Estimated Effort:** 1-2 months (major refactoring)

### Approach 3: Document Multi-Threaded Requirement (Recommended for Current State)

**Design:** Accept multi-threaded runtime as an architectural constraint.

**Implementation:**
1. Update documentation to clearly state requirement
2. Add compile-time checks for correct tokio features
3. Update all examples with proper runtime configuration
4. Add troubleshooting guide

**Example Compile-Time Check:**
```rust
// In lib.rs or core/lib.rs
#[cfg(not(all(
    feature = "tokio",
    any(
        all(not(tokio_unstable), tokio_unstable),
        all(tokio_rt_multi_thread, not(tokio_rt_current_thread))
    )
)))]
compile_error!(
    "drasi-core requires tokio with multi-threaded runtime.\n\
     Add to your Cargo.toml:\n\
     tokio = { version = \"1.0\", features = [\"rt-multi-thread\", \"macros\", \"sync\", \"time\"] }"
);
```

**Pros:**
- Zero code changes
- Maintains current performance
- Honest about system requirements
- Immediate solution

**Cons:**
- Cannot run in single-threaded environments
- Limits deployment scenarios (WASM, embedded)
- Higher resource usage

**Estimated Effort:** 1 day (documentation only)

---

## Recommendations

### Short-Term (Immediate)

**Implement Approach 3: Document the Requirement**

1. Create this documentation file
2. Update README.md with runtime requirements
3. Add troubleshooting section to docs
4. Update all example code with proper annotations

### Medium-Term (If Single-Threaded is Critical)

**Implement Approach 1: Async Storage Abstraction**

Only pursue if there are specific use cases requiring single-threaded runtime:
- Embedded systems with limited resources
- WebAssembly deployment
- Testing simplicity
- Specific deployment constraints

### Long-Term (Consider for Major Version)

**Evaluate Approach 2: Fully Async Storage**

If moving toward fully async ecosystem, consider:
- Research async-native database alternatives
- Benchmark performance vs. RocksDB
- Plan migration path for existing users
- Design backward compatibility strategy

---

## Testing Strategy

### For Single-Threaded Support (if implementing Approach 1)

1. **Convert all tests to current_thread flavor:**
   ```rust
   #[tokio::test(flavor = "current_thread")]
   async fn test_name() { ... }
   ```

2. **Add explicit runtime tests:**
   ```rust
   #[test]
   fn test_single_threaded_runtime() {
       let rt = tokio::runtime::Builder::new_current_thread()
           .enable_all()
           .build()
           .unwrap();
       
       rt.block_on(async {
           // Test full system functionality
       });
   }
   ```

3. **Deadlock testing under load:**
   - Multiple concurrent queries
   - High-throughput sources
   - Slow consumers with broadcast channels

4. **Performance benchmarks:**
   - Compare single-threaded vs. multi-threaded
   - Measure I/O operation latency
   - Test concurrent query processing

### For Documenting Requirement (Approach 3)

1. **Verify all examples work with multi-threaded runtime**
2. **Add negative tests that verify panics with wrong runtime**
3. **Document expected error messages**

---

## Migration Guide (if implementing Approach 1)

### For Library Users

**Before:**
```rust
#[tokio::main]
async fn main() {
    // Works (defaults to multi-threaded)
}
```

**After:**
```rust
// Both work:
#[tokio::main]  // multi-threaded
#[tokio::main(flavor = "current_thread")]  // single-threaded
async fn main() {
    // Both supported
}
```

### For Plugin Developers

**Before:**
```rust
async fn store_data(&self, key: &str, value: &[u8]) -> Result<()> {
    tokio::task::spawn_blocking(move || {
        // RocksDB operations
    }).await?
}
```

**After:**
```rust
async fn store_data(&self, key: &str, value: &[u8]) -> Result<()> {
    storage::async_operation(move || {
        // RocksDB operations (runs on dedicated pool)
    }).await
}
```

---

## Open Questions

1. **What is the target deployment environment?**
   - Server/cloud → multi-threaded is fine
   - Embedded/WASM → single-threaded is critical

2. **What is acceptable performance profile?**
   - Need parallelism for throughput?
   - Can tolerate single-threaded constraints?

3. **Are there specific use cases requiring single-threaded?**
   - Testing simplicity?
   - Resource constraints?
   - Specific runtime environments?

4. **What is priority vs. effort tradeoff?**
   - Quick documentation (1 day) vs.
   - Storage abstraction (1-2 weeks) vs.
   - Full async migration (1-2 months)

---

## References

### Tokio Documentation
- [spawn_blocking](https://docs.rs/tokio/latest/tokio/task/fn.spawn_blocking.html)
- [block_in_place](https://docs.rs/tokio/latest/tokio/task/fn.block_in_place.html)
- [Runtime flavors](https://docs.rs/tokio/latest/tokio/runtime/index.html#runtime-flavors)
- [Bridging with sync code](https://tokio.rs/tokio/topics/bridging)

### Key Code Locations
- **Blocking storage:** `components/indexes/rocksdb/src/*.rs`
- **State store:** `components/state_stores/redb/src/provider.rs`
- **Source base:** `lib/src/sources/base.rs:536`
- **Task spawning:** `lib/src/queries/manager.rs`, `lib/src/reactions/common/base.rs`
- **Runtime creation:** `lib/src/managers/tracing_layer.rs:117`

---

## Conclusion

**Current Answer:** drasi-core **requires multi-threaded tokio runtime** and will panic if run with single-threaded runtime.

**Path Forward:** 
- **Immediate:** Document the requirement clearly (Approach 3)
- **Optional:** If single-threaded support becomes critical, implement async storage abstraction (Approach 1)

The decision depends on your deployment requirements and acceptable engineering effort.
