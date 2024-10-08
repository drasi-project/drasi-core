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

use clap::Parser;

use std::{env, sync::Arc};
use uuid::Uuid;

use drasi_core::{
    interface::{AccumulatorIndex, ElementIndex},
    query::QueryBuilder,
};
use drasi_index_garnet::{element_index::GarnetElementIndex, result_index::GarnetResultIndex};
use drasi_index_rocksdb::{
    element_index::{RocksDbElementIndex, RocksIndexOptions},
    result_index::RocksDbResultIndex,
};

use test_run::{IndexType, TestRunArgs, TestRunConfig, TestRunResult};

mod models;
mod scenario;
mod test_run;

#[allow(clippy::print_stdout, clippy::unwrap_used)]
#[tokio::main]
async fn main() {
    let args = TestRunArgs::parse();

    // println!("{:#?}", args);

    let test_run_config = TestRunConfig::new(args);

    let scenarios = scenario::get_scenarios(
        &test_run_config.scenario,
        test_run_config.iterations,
        test_run_config.seed,
    );

    if scenarios.is_empty() {
        println!("Scenario {} not found", test_run_config.scenario);
        return;
    }

    println!("--------------------------------");
    println!("Drasi Query Component Perf Tests");
    println!("--------------------------------");
    println!("Test Run Config: \n{:?}\n", test_run_config);

    for scenario in scenarios {
        let mut result: TestRunResult = TestRunResult::new(
            scenario.get_scenario_config().name.clone(),
            test_run_config.element_index_type,
            test_run_config.result_index_type,
        );

        let scenario_config = scenario.get_scenario_config();
        println!("--------------------------------");
        println!("Scenario - {}", scenario_config.name);
        println!("--------------------------------");
        println!(" - Scenario Config: \n{:?}\n", scenario_config);
        println!(" - Initializing Scenario...");

        let query_id = format!("test-{}", Uuid::new_v4());

        let mut builder = QueryBuilder::new(&scenario_config.query);

        // Configure the correct element index
        builder = match test_run_config.element_index_type {
            IndexType::Memory => builder,
            IndexType::Redis => {
                let url = match env::var("REDIS_URL") {
                    Ok(url) => url,
                    Err(_) => "redis://127.0.0.1:6379".to_string(),
                };

                let element_index = GarnetElementIndex::connect(&query_id, &url).await.unwrap();

                builder.with_element_index(Arc::new(element_index))
            }
            IndexType::RocksDB => {
                let options = RocksIndexOptions {
                    archive_enabled: false,
                    direct_io: false,
                };

                let url = match env::var("ROCKS_PATH") {
                    Ok(url) => url,
                    Err(_) => "test-data".to_string(),
                };

                let element_index = RocksDbElementIndex::new(&query_id, &url, options).unwrap();
                element_index.clear().await.unwrap();

                builder
            }
        };

        // Configure the correct result index
        builder = match test_run_config.result_index_type {
            IndexType::Memory => builder,
            IndexType::Redis => {
                let url = match env::var("REDIS_URL") {
                    Ok(url) => url,
                    Err(_) => "redis://127.0.0.1:6379".to_string(),
                };

                let ari = GarnetResultIndex::connect(&query_id, &url).await.unwrap();

                builder.with_result_index(Arc::new(ari))
            }
            IndexType::RocksDB => {
                let url = match env::var("ROCKS_PATH") {
                    Ok(url) => url,
                    Err(_) => "test-data".to_string(),
                };

                let ari = RocksDbResultIndex::new(&query_id, &url).unwrap();
                ari.clear().await.unwrap();

                builder
            }
        };

        let cq = builder.build().await;

        println!(" - Bootstrapping Scenario...");
        let mut bootstrap_change_stream_iter = scenario.get_bootstrap_source_change_stream();
        result.start_bootstrap();
        for source_change in bootstrap_change_stream_iter.iter() {
            // println!("source_change: {:#?}", source_change);
            result.start_bootstrap_event();
            let _change_result = cq.process_source_change(source_change).await.unwrap();
            result.end_bootstrap_event();
            // println!("_change_result: {:#?}", _change_result);
        }
        result.end_bootstrap();
        // println!("Bootstrap result: {:#?}", result);

        println!(" - Running Scenario... ");
        let mut scenario_change_stream_iter = scenario.get_scenario_source_change_stream();
        result.start_run();
        for source_change in scenario_change_stream_iter.iter() {
            // println!("source_change: {:#?}", source_change);
            result.start_run_event();
            let _change_result = cq.process_source_change(source_change).await.unwrap();
            result.end_run_event();
            // println!("change_result: {:#?}", _change_result);
        }
        result.end_run();
        println!(" - Result: {:#?}", result);
    }
}
