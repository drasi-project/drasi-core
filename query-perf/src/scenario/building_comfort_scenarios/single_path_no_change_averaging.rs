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

use rand::Rng;
use std::sync::Arc;

use crate::scenario::{
    building_comfort_scenarios::{
        building_comfort_model::{BootstrapSourceChangeGenerator, BuildingComfortModel},
        generate_building_sizes, FloatRange, RoomPropertySourceChangeGenerator,
        RoomPropertySourceChangeGeneratorConfig, TimeRange,
    },
    {PerformanceTestScenario, PerformanceTestScenarioConfig, SourceChangeStream},
};

const SCENARIO_NAME: &str = "single_path_no_change_averaging_projection";
const SCENARIO_SOURCE_ID: &str = "perf_tests";
const SCENARIO_QUERY: &str = "\
    MATCH (r:Room)-[:PART_OF]->(f:Floor) \
    WITH \
        f.name AS FloorName, \
        floor( \
            50 + (r.temperature - 72) + (r.humidity - 42) + \
            CASE WHEN r.co2 > 500 THEN (r.co2 - 500) / 25 ELSE 0 END \
        ) AS RoomComfortLevel \
    RETURN \
        FloorName, \
        avg(RoomComfortLevel) AS ComfortLevel";
const SCENARIO_DEFAULT_ITERATIONS: u128 = 10000;
const SCENARIO_INIT_START_TIME_MS: u64 = 0;
const SCENARIO_INIT_TIME_STEP_MS: u64 = 1;
const SCENARIO_START_TIME_MS: u64 = 5000000;

// The number of buildings to generate for the scenario.
const SCENARIO_BUILDING_COUNT: usize = 10;

// The set of building size templates to use for the scenario.
// Each tuple represents a (floor_count, rooms_per_floor) pair.
// These are used in a round robin fashion to generate the buildings
// for the scenario. The first building will use the first tuple, the
// second building will use the second tuple, etc. When the end of the
// list is reached, it will start over at the beginning.
const SCENARIO_BUILDING_SIZE_TEMPLATES: [(usize, usize); 1] = [(10, 20)];

// e.g. another alternative might be
// const SCENARIO_BUILDING_SIZE_TEMPLATES: [(usize,usize);3] = [(10,20), (15,25), (20,10)];

pub struct SinglePathNoChangeAveragingProjectionScenario {
    model: Arc<BuildingComfortModel>,
    config: PerformanceTestScenarioConfig,
}

impl SinglePathNoChangeAveragingProjectionScenario {
    pub fn new(
        iterations: Option<u128>,
        seed: Option<u64>,
    ) -> SinglePathNoChangeAveragingProjectionScenario {
        // Generate the set of Building sizes for the scenario.
        let building_size_templates = SCENARIO_BUILDING_SIZE_TEMPLATES.to_vec();
        let building_sizes =
            generate_building_sizes(SCENARIO_BUILDING_COUNT, &building_size_templates);

        // Create the BuildingComfortModel to use for the scenario.
        let mut current_time_ms = SCENARIO_INIT_START_TIME_MS;
        let mut model = BuildingComfortModel::new(current_time_ms);

        // Add the initial set of buildings to the model.
        for building in building_sizes {
            // get the building size at the current index
            let (_, floor_count, rooms_per_floor) = building;
            // add the building to the model
            model.add_building_hierarchy(
                SCENARIO_SOURCE_ID.to_string(),
                current_time_ms,
                SCENARIO_INIT_TIME_STEP_MS,
                floor_count,
                rooms_per_floor,
            );

            current_time_ms = model.get_last_change_time_ms() + SCENARIO_INIT_TIME_STEP_MS;
        }

        SinglePathNoChangeAveragingProjectionScenario {
            model: Arc::new(model),
            config: PerformanceTestScenarioConfig {
                name: String::from(SCENARIO_NAME),
                query: String::from(SCENARIO_QUERY),
                iterations: iterations.unwrap_or(SCENARIO_DEFAULT_ITERATIONS),
                start_time_ms: SCENARIO_START_TIME_MS,
                seed: seed.unwrap_or(rand::thread_rng().gen()),
            },
        }
    }
}

impl PerformanceTestScenario for SinglePathNoChangeAveragingProjectionScenario {
    fn get_scenario_config(&self) -> PerformanceTestScenarioConfig {
        self.config.clone()
    }

    fn get_bootstrap_source_change_stream(&self) -> SourceChangeStream {
        let bootstrap_change_generator = BootstrapSourceChangeGenerator::new(self.model.clone());
        SourceChangeStream::new(Box::new(bootstrap_change_generator))
    }

    fn get_scenario_source_change_stream(&self) -> SourceChangeStream {
        SourceChangeStream::new(Box::new(RoomPropertySourceChangeGenerator::new(
            self.model.clone(),
            RoomPropertySourceChangeGeneratorConfig::new(
                self.config.iterations,
                self.config.start_time_ms,
                TimeRange::new(1, 3),
                self.config.seed,
                FloatRange::new(-2.0, 2.0),
                FloatRange::new(-2.0, 2.0),
                FloatRange::new(-2.0, 2.0),
            ),
        )))
    }
}
