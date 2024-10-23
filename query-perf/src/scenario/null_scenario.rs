// Copyright 2024 The Drasi Authors.
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

use drasi_core::models::SourceChange;

use super::{
    PerformanceTestScenario, PerformanceTestScenarioConfig, SourceChangeGenerator,
    SourceChangeStream,
};

const SCENARIO_NAME: &str = "null";
const SCENARIO_QUERY: &str = "MATCH (r:Room) RETURN r.Name as RoomName";
const SCENARIO_ITERATIONS: u128 = 0;
const SCENARIO_START_TIME_MS: u64 = 5000000;

pub struct NullScenario {}

impl NullScenario {
    pub fn new() -> NullScenario {
        NullScenario {}
    }
}

impl PerformanceTestScenario for NullScenario {
    fn get_scenario_config(&self) -> PerformanceTestScenarioConfig {
        PerformanceTestScenarioConfig {
            name: String::from(SCENARIO_NAME),
            query: String::from(SCENARIO_QUERY),
            iterations: SCENARIO_ITERATIONS,
            start_time_ms: SCENARIO_START_TIME_MS,
            seed: 0,
        }
    }

    fn get_bootstrap_source_change_stream(&self) -> SourceChangeStream {
        SourceChangeStream::new(Box::new(EmptySourceChangeGenerator::new()))
    }

    fn get_scenario_source_change_stream(&self) -> SourceChangeStream {
        SourceChangeStream::new(Box::new(EmptySourceChangeGenerator::new()))
    }
}

struct EmptySourceChangeGenerator {}

impl EmptySourceChangeGenerator {
    fn new() -> EmptySourceChangeGenerator {
        EmptySourceChangeGenerator {}
    }
}

impl SourceChangeGenerator for EmptySourceChangeGenerator {
    fn generate_change(&mut self) -> Option<SourceChange> {
        None
    }
}
