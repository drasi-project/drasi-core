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
use drasi_lib::{ComponentId, ComponentName};
use drasi_reaction_profiler::ProfilerReaction;
use shared_tests::recovery_test_helpers::exercise_autoskipgap_recovery;

#[tokio::test(flavor = "current_thread")]
async fn test_profiler_reaction_autoskipgap_recovery() -> Result<()> {
    let reaction = ProfilerReaction::builder(ComponentId::generate())
        .with_query_by_name(ComponentName::try_new("q1")?)
        .build()?;
    exercise_autoskipgap_recovery(
        "profiler-autoskipgap",
        &ComponentName::try_new("profiler-autoskip")?,
        reaction,
    )
    .await
}
