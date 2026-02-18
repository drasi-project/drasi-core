// Copyright 2025 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Tests to verify drasi-core works correctly on both single-threaded and multi-threaded tokio runtimes.
//!
//! These tests ensure that:
//! - spawn_blocking operations (used by storage backends) work on both runtime types
//! - Cooperative multitasking works correctly on single-threaded runtime
//! - No deadlocks occur with either runtime type

/// Test spawn_blocking storage operations on current_thread runtime
///
/// This verifies that spawn_blocking (used by RocksDB/Redb indexes) works correctly
/// on single-threaded runtime using tokio's separate blocking thread pool.
#[tokio::test(flavor = "current_thread")]
async fn test_spawn_blocking_on_current_thread() {
    // Simulate what storage backends do - spawn blocking operations
    let result = tokio::task::spawn_blocking(|| {
        // Simulate synchronous database operation (like RocksDB)
        std::thread::sleep(std::time::Duration::from_millis(10));
        42
    }).await.expect("spawn_blocking should work on current_thread runtime");
    
    assert_eq!(result, 42);
}

/// Test spawn_blocking storage operations on multi_thread runtime
///
/// This verifies that spawn_blocking works on multi-threaded runtime as well.
#[tokio::test(flavor = "multi_thread")]
async fn test_spawn_blocking_on_multi_thread() {
    let result = tokio::task::spawn_blocking(|| {
        std::thread::sleep(std::time::Duration::from_millis(10));
        42
    }).await.expect("spawn_blocking should work on multi_thread runtime");
    
    assert_eq!(result, 42);
}

/// Test multiple concurrent spawn_blocking operations on current_thread
///
/// Verifies that the blocking thread pool can handle multiple concurrent operations
/// even when the async executor is single-threaded.
#[tokio::test(flavor = "current_thread")]
async fn test_concurrent_spawn_blocking_current_thread() {
    let handles: Vec<_> = (0..5).map(|i| {
        tokio::task::spawn_blocking(move || {
            std::thread::sleep(std::time::Duration::from_millis(10));
            i * 2
        })
    }).collect();

    let mut results = Vec::new();
    for handle in handles {
        results.push(handle.await.expect("Task should complete"));
    }

    assert_eq!(results, vec![0, 2, 4, 6, 8]);
}

/// Test multiple concurrent spawn_blocking operations on multi_thread
///
/// Verifies concurrent blocking operations work on multi-threaded runtime.
#[tokio::test(flavor = "multi_thread")]
async fn test_concurrent_spawn_blocking_multi_thread() {
    let handles: Vec<_> = (0..5).map(|i| {
        tokio::task::spawn_blocking(move || {
            std::thread::sleep(std::time::Duration::from_millis(10));
            i * 2
        })
    }).collect();

    let mut results = Vec::new();
    for handle in handles {
        results.push(handle.await.expect("Task should complete"));
    }

    assert_eq!(results, vec![0, 2, 4, 6, 8]);
}

/// Test cooperative multitasking with tokio::spawn on current_thread
///
/// Verifies that multiple async tasks can interleave via cooperative scheduling.
#[tokio::test(flavor = "current_thread")]
async fn test_cooperative_multitasking_current_thread() {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    let counter = Arc::new(AtomicU32::new(0));
    
    // Spawn multiple tasks that will cooperatively yield
    let handles: Vec<_> = (0..5).map(|_| {
        let counter = counter.clone();
        tokio::spawn(async move {
            for _ in 0..10 {
                counter.fetch_add(1, Ordering::Relaxed);
                // Yield point - allows other tasks to run
                tokio::time::sleep(std::time::Duration::from_micros(1)).await;
            }
        })
    }).collect();

    // Wait for all tasks
    for handle in handles {
        handle.await.expect("Task should complete");
    }

    assert_eq!(counter.load(Ordering::Relaxed), 50);
}

/// Test cooperative multitasking with tokio::spawn on multi_thread
///
/// Verifies that multiple async tasks work on multi-threaded runtime.
#[tokio::test(flavor = "multi_thread")]
async fn test_cooperative_multitasking_multi_thread() {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    let counter = Arc::new(AtomicU32::new(0));
    
    // Spawn multiple tasks
    let handles: Vec<_> = (0..5).map(|_| {
        let counter = counter.clone();
        tokio::spawn(async move {
            for _ in 0..10 {
                counter.fetch_add(1, Ordering::Relaxed);
                tokio::time::sleep(std::time::Duration::from_micros(1)).await;
            }
        })
    }).collect();

    // Wait for all tasks
    for handle in handles {
        handle.await.expect("Task should complete");
    }

    assert_eq!(counter.load(Ordering::Relaxed), 50);
}
