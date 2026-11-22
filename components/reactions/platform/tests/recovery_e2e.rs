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

use anyhow::Result;
use drasi_reaction_platform::PlatformReaction;
use shared_tests::recovery_test_helpers::exercise_strict_gap_failure;
use shared_tests::redis_helpers::setup_redis;

#[tokio::test]
#[ignore = "requires Docker: Redis container"]
async fn test_platform_reaction_strict_recovery() -> Result<()> {
    let redis = setup_redis().await;
    let reaction = PlatformReaction::builder(
        drasi_core::ComponentId::try_new("00000000-0000-0000-0000-00000000001c")
            .expect("fixture UUID"),
    )
    .with_redis_url(redis.url())
    .with_query_by_name(drasi_core::ComponentName::try_new("q1").expect("fixture component name"))
    .build()?;

    let result = exercise_strict_gap_failure(
        "platform-strict-recovery",
        &drasi_core::ComponentName::try_new("platform-strict").expect("fixture component name"),
        reaction,
    )
    .await;
    redis.cleanup().await;
    result
}
