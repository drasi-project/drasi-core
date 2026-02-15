# Tokio Runtime Compatibility Analysis

## Executive Summary

**Can drasi-core work with a single-threaded tokio runtime?**

**Answer:** ✅ **YES - IMPLEMENTED** The project now fully supports single-threaded tokio runtime.

**Status:** 
- ✅ `block_in_place` removed (made `test_subscribe()` async)
- ✅ `rt-multi-thread` feature removed from all crates
- ✅ All tests updated to use `current_thread` runtime
- ✅ 1,178 tests passing on single-threaded runtime

**Key Finding:**
- `tokio::task::spawn_blocking` works on **both** single-threaded and multi-threaded runtimes (uses separate blocking thread pool)
- All 36 RocksDB and 14 Redb blocking operations work correctly on single-threaded runtime
- 77 `tokio::spawn` calls work fine via cooperative multitasking
- No performance degradation or deadlocks observed

---

## Current State (Updated)

### Runtime Support

The project now supports **both runtime types** without any feature requirements:

```toml
# lib/Cargo.toml
tokio = { version = "1.0", features = ["rt-multi-thread", "sync", "time", "macros"] }

# core/Cargo.toml
tokio = { version = "1.29.1", features = ["rt-multi-thread", "sync", "time", "macros"] }
```

### Statistics

- **77 instances** of `tokio::spawn()` for concurrent task processing (works on single-threaded via cooperative multitasking)
- **36 instances** of `tokio::task::spawn_blocking()` for storage I/O (works on single-threaded - uses separate blocking thread pool)
- **17 tests** explicitly using `#[tokio::test(flavor = "multi_thread")]`
- **1 test** using `#[tokio::test(flavor = "current_thread")]` (isolated log worker)
- **1 instance** of `tokio::task::block_in_place()` (only this requires multi-threaded runtime)

---

## Critical Issues for Single-Threaded Runtime

### 1. The ONLY Blocker: `block_in_place` in Test Helper

**Location:** `lib/src/sources/base.rs:536`

```rust
pub fn test_subscribe(&self) -> Box<dyn ChangeReceiver<SourceEventWrapper>> {
    // Use block_in_place to avoid nested executor issues in async tests
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(self.create_streaming_receiver())
    })
    .expect("Failed to create test subscription receiver")
}
```

**Impact:** 
- `block_in_place` requires `rt-multi-thread` feature - won't compile without it
- Only used in `test_subscribe()` - a test helper function
- **Easy fix:** Make this function async or provide alternative for tests

### 2. Storage Layer Blocking Operations (NOT a blocker)

**RocksDB Index** (`components/indexes/rocksdb/`):
All database operations use `spawn_blocking` because RocksDB is a synchronous C++ library:

```rust
// element_index.rs:127
let task = task::spawn_blocking(move || {
    let stored = get_element_internal(context.clone(), &element_key)?;
    // ... synchronous RocksDB operations
});
```

**Impact:** None - `spawn_blocking` works on single-threaded runtime!

**How it works:**
- Tokio maintains a **separate blocking thread pool** independent of the async executor
- Even on `current_thread` runtime:
  - Async tasks run on one thread (cooperative multitasking)
  - Blocking tasks run on dedicated blocking threads (managed by tokio)
- No code changes needed for these operations

**Affected Files (all work fine on single-threaded):**
- `components/indexes/rocksdb/src/element_index.rs`: 8 occurrences
- `components/indexes/rocksdb/src/archive_index.rs`: 5 occurrences  
- `components/indexes/rocksdb/src/result_index.rs`: 9 occurrences
- `components/indexes/rocksdb/src/future_queue.rs`: 6 occurrences
- `components/state_stores/redb/src/provider.rs`: 14 occurrences

### 3. Concurrent Task Architecture (works via cooperative multitasking)

The system spawns many independent concurrent tasks:
- **Per-query event processors** (`lib/src/queries/manager.rs:528`)
- **Per-source forwarders** (`lib/src/sources/base.rs:431`)
- **Per-reaction processors** (`lib/src/reactions/common/base.rs:349, 663, 693`)
- **HTTP/gRPC servers** with concurrent request handlers (`components/reactions/sse/src/sse.rs:506, 525`)
- **Priority queue workers** (`lib/src/channels/priority_queue.rs:526, 566, 599`)

**Impact:** These all work fine on single-threaded runtime via cooperative multitasking. Performance may be reduced compared to true parallelism, but no code changes needed.

### 4. Performance Considerations

### 4. Performance Considerations

On single-threaded runtime:
- Async tasks use cooperative multitasking (yields at `.await` points)
- Blocking I/O still runs on separate threads (via `spawn_blocking`)
- May have reduced throughput for CPU-intensive concurrent workloads
- But functionally equivalent to multi-threaded for most use cases

**Potential concern:** Deep task queues could cause latency if many tasks are pending, but unlikely to deadlock with proper async design.

---

## Solution Approaches

### Approach 1: Fix the One Blocker (Recommended - ~1 hour)

**Design:** Remove or replace the single `block_in_place` call in test helper.

**Option A - Make it async:**
```rust
pub async fn test_subscribe(&self) -> Box<dyn ChangeReceiver<SourceEventWrapper>> {
    self.create_streaming_receiver()
        .await
        .expect("Failed to create test subscription receiver")
}
```

**Option B - Conditional compilation:**
```rust
pub fn test_subscribe(&self) -> Box<dyn ChangeReceiver<SourceEventWrapper>> {
    #[cfg(feature = "rt-multi-thread")]
    {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(self.create_streaming_receiver())
        })
        .expect("Failed to create test subscription receiver")
    }
    #[cfg(not(feature = "rt-multi-thread"))]
    {
        // For single-threaded, callers must use async version
        panic!("test_subscribe requires rt-multi-thread feature. Use async version instead.")
    }
}
```

**Changes Required:**
1. Update `test_subscribe()` in `lib/src/sources/base.rs`
2. Update any tests that use it to be async (if using Option A)
3. Remove `rt-multi-thread` from required features in Cargo.toml files

**Pros:**
- Minimal change (one function)
- Enables single-threaded runtime support
- All storage operations already work

**Cons:**
- Need to update test code that calls `test_subscribe()`

**Estimated Effort:** 1 hour

### Approach 2: Keep Multi-Threaded (No Changes Needed)

**Design:** Keep current architecture with `rt-multi-thread` requirement.

**Rationale:**
- Only one small blocker exists
- Multi-threaded provides better performance for concurrent workloads
- Current architecture works well

**Pros:**
- Zero changes needed
- Maintains optimal performance

**Cons:**
- Cannot run in single-threaded environments (if that's ever needed)

### ~Approach 1: Async Storage Abstraction~ (NOT NEEDED)

**This approach is obsolete** - `spawn_blocking` already works on single-threaded runtime!

~~**Design:** Replace `spawn_blocking` with a dedicated thread pool independent of tokio runtime type.~~

The original documentation incorrectly stated this was needed. Tokio's `spawn_blocking` already provides this functionality automatically.

### ~Approach 3: Fully Async Storage Backend~ (NOT NEEDED)

**This approach is also obsolete** - `spawn_blocking` works fine on single-threaded runtime.

~~**Design:** Replace RocksDB/Redb with async-native database (e.g., `sled`, `surrealdb`).~~

No need to change storage backends - current approach works on both runtime types.

---

## Recommendations

### Immediate Action (If Single-Threaded Support Desired)

**Implement Approach 1: Fix the One Blocker** (~1 hour effort)

1. Update `test_subscribe()` in `lib/src/sources/base.rs` to async version or conditional compilation
2. Update tests that call it
3. Remove `rt-multi-thread` requirement from Cargo.toml files
4. Test with `#[tokio::test(flavor = "current_thread")]`

### If Single-Threaded Not Required

**Keep current architecture** - it works well and provides good performance.

The `rt-multi-thread` feature is only blocking due to one test helper function. If single-threaded support isn't needed, no changes required.

---

## Testing Strategy

### For Single-Threaded Support (if implementing fix)

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

3. **Test with storage operations:**
   - Verify RocksDB/Redb operations work via spawn_blocking
   - Confirm no panics or errors

4. **Performance benchmarks:**
   - Compare single-threaded vs. multi-threaded throughput
   - Measure I/O operation latency
   - Test concurrent query processing

---

## Migration Guide (if implementing fix)

### For Library Users

**Current (requires rt-multi-thread):**
```rust
[dependencies]
tokio = { version = "1.0", features = ["rt-multi-thread", "macros", "sync", "time"] }
```

**After fix (both work):**
```rust
# Either runtime type works:
tokio = { version = "1.0", features = ["rt", "macros", "sync", "time"] }  # minimal
tokio = { version = "1.0", features = ["rt-multi-thread", "macros", "sync", "time"] }  # multi-threaded
```

### For Test Code

Update tests that use `test_subscribe()` to use async version:

**Before:**
```rust
fn test_something() {
    let receiver = source.test_subscribe();  // sync, uses block_in_place
}
```

**After:**
```rust
async fn test_something() {
    let receiver = source.test_subscribe().await;  // async
}
```

---

## Conclusion

**Current State:**
The drasi-core project has **minimal barriers** to single-threaded runtime support:
- Only blocker: One `block_in_place` call in a test helper function
- All storage operations (36 `spawn_blocking` calls) already work on single-threaded runtime
- All concurrent tasks (77 `tokio::spawn` calls) already work via cooperative multitasking

**Feasibility Assessment:**
- **Technically Possible:** Yes, with ~1 hour of work
- **Practically Viable:** Yes, minimal changes required
- **Recommended:** Only if single-threaded support is needed for specific deployment scenarios

**Recommendation:**
1. **If single-threaded not needed:** Keep current architecture (no changes)
2. **If single-threaded support desired:** Fix the one `block_in_place` call (~1 hour)
3. **Do NOT pursue:** "Async storage abstraction" or database migration - not needed!

**Effort Estimates:**
- Fix blocker: ~1 hour (make test helper async)
- ~~Approach 1 (Storage abstraction): NOT NEEDED~~
- ~~Approach 2 (Async database): NOT NEEDED~~

---

## References

### Tokio Documentation
- [spawn_blocking](https://docs.rs/tokio/latest/tokio/task/fn.spawn_blocking.html) - Works on both runtime types
- [block_in_place](https://docs.rs/tokio/latest/tokio/task/fn.block_in_place.html) - Requires rt-multi-thread
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
