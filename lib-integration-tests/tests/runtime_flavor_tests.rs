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

use drasi_lib::DrasiLib;

#[tokio::test(flavor = "current_thread")]
async fn test_drasi_lib_builds_on_current_thread_runtime() {
    let drasi = DrasiLib::builder()
        .with_id("current-thread-runtime")
        .build()
        .await
        .expect("Failed to build DrasiLib on current_thread runtime");

    drasi.stop().await.ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_drasi_lib_builds_on_multi_thread_runtime() {
    let drasi = DrasiLib::builder()
        .with_id("multi-thread-runtime")
        .build()
        .await
        .expect("Failed to build DrasiLib on multi_thread runtime");

    drasi.stop().await.ok();
}

#[tokio::test(flavor = "current_thread")]
async fn test_spawn_blocking_works_on_current_thread_runtime() {
    let result = tokio::task::spawn_blocking(|| 21 * 2)
        .await
        .expect("spawn_blocking task panicked on current_thread runtime");

    assert_eq!(result, 42);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_spawn_blocking_works_on_multi_thread_runtime() {
    let result = tokio::task::spawn_blocking(|| 21 * 2)
        .await
        .expect("spawn_blocking task panicked on multi_thread runtime");

    assert_eq!(result, 42);
}
