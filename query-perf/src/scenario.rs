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

use building_comfort_scenarios::{
    single_node_calculation::SingleNodeCalculationProjectionScenario,
    single_node_property_projection::SingleNodePropertyProjectionScenario,
    single_path_averaging::SinglePathAveragingProjectionScenario,
    single_path_no_change_averaging::SinglePathNoChangeAveragingProjectionScenario,
};
use null_scenario::NullScenario;

mod building_comfort_scenarios;
mod null_scenario;

pub trait SourceChangeGenerator {
    fn generate_change(&mut self) -> Option<SourceChange>;
}

pub struct SourceChangeStream {
    change_stream_generator: Box<dyn SourceChangeGenerator>,
}

impl SourceChangeStream {
    pub fn new(change_stream_generator: Box<dyn SourceChangeGenerator>) -> SourceChangeStream {
        SourceChangeStream {
            change_stream_generator,
        }
    }
    pub fn iter(&mut self) -> SrcChangeStreamIter {
        SrcChangeStreamIter::new(self)
    }
}

pub struct SrcChangeStreamIter<'a> {
    stream: &'a mut SourceChangeStream,
}

impl<'a> SrcChangeStreamIter<'a> {
    pub fn new(stream: &'a mut SourceChangeStream) -> SrcChangeStreamIter<'a> {
        SrcChangeStreamIter { stream }
    }
}

impl<'a> Iterator for SrcChangeStreamIter<'a> {
    type Item = SourceChange;

    fn next(&mut self) -> Option<Self::Item> {
        self.stream.change_stream_generator.generate_change()
    }
}

#[derive(Debug, Clone)]
pub struct PerformanceTestScenarioConfig {
    pub name: String,
    pub query: String,
    pub iterations: u128,
    pub start_time_ms: u64,
    pub seed: u64,
}

pub trait PerformanceTestScenario {
    fn get_scenario_config(&self) -> PerformanceTestScenarioConfig;
    fn get_bootstrap_source_change_stream(&self) -> SourceChangeStream;
    fn get_scenario_source_change_stream(&self) -> SourceChangeStream;
}

impl PerformanceTestScenario for Box<dyn PerformanceTestScenario> {
    fn get_scenario_config(&self) -> PerformanceTestScenarioConfig {
        self.as_ref().get_scenario_config()
    }

    fn get_bootstrap_source_change_stream(&self) -> SourceChangeStream {
        self.as_ref().get_bootstrap_source_change_stream()
    }

    fn get_scenario_source_change_stream(&self) -> SourceChangeStream {
        self.as_ref().get_scenario_source_change_stream()
    }
}

// The get_scenarios function returns a list of PerformanceTestScenario instances whose name includes
// the text specified in the scenario_filter parameter. The scenario_filter parameter is passed in
// from the command line. If the scenario_filter parameter is "*", then all scenarios are returned.
// Otherwise, only scenarios whose name matches the scenario_filter parameter are returned.
pub fn get_scenarios(
    scenario_filter: &str,
    iterations: Option<u128>,
    seed: Option<u64>,
) -> Vec<Box<dyn PerformanceTestScenario>> {
    let mut scenarios: Vec<Box<dyn PerformanceTestScenario>> = Vec::new();

    // TODO: Could do this more cleanly with dyanamic loading of modules.
    if scenario_filter == "all" || "single_node_property_projection".contains(scenario_filter) {
        scenarios.push(Box::new(SingleNodePropertyProjectionScenario::new(
            iterations, seed,
        )));
    }

    if scenario_filter == "all" || "single_node_calculation_projection".contains(scenario_filter) {
        scenarios.push(Box::new(SingleNodeCalculationProjectionScenario::new(
            iterations, seed,
        )));
    }

    if scenario_filter == "all" || "single_path_averaging_projection".contains(scenario_filter) {
        scenarios.push(Box::new(SinglePathAveragingProjectionScenario::new(
            iterations, seed,
        )));
    }

    if scenario_filter == "all"
        || "single_path_no_change_averaging_projection".contains(scenario_filter)
    {
        scenarios.push(Box::new(
            SinglePathNoChangeAveragingProjectionScenario::new(iterations, seed),
        ));
    }

    if scenario_filter == "null" {
        scenarios.push(Box::new(NullScenario::new()));
    }

    scenarios
}
