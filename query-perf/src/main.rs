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
    evaluation::functions::FunctionRegistry,
    interface::{AccumulatorIndex, ElementIndex},
    query::QueryBuilder,
};
use drasi_functions_cypher::CypherFunctionSet;
use drasi_index_garnet::{
    element_index::GarnetElementIndex, result_index::GarnetResultIndex, GarnetSessionState,
};
use drasi_index_rocksdb::{
    element_index::{RocksDbElementIndex, RocksIndexOptions},
    open_unified_db,
    result_index::RocksDbResultIndex,
    RocksDbSessionState,
};
use drasi_query_cypher::CypherParser;

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
    println!("Test Run Config: \n{test_run_config:?}\n");

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
        println!(" - Scenario Config: \n{scenario_config:?}\n");
        println!(" - Initializing Scenario...");

        let query_id = format!("test-{}", Uuid::new_v4());

        let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
        let parser = Arc::new(CypherParser::new(function_registry.clone()));
        let mut builder = QueryBuilder::new(&scenario_config.query, parser)
            .with_function_registry(function_registry);

        // Open shared Redis connection and session state if either index type needs it
        let (redis_connection, garnet_session_state) = if test_run_config.element_index_type
            == IndexType::Redis
            || test_run_config.result_index_type == IndexType::Redis
        {
            let url = match env::var("REDIS_URL") {
                Ok(url) => url,
                Err(_) => "redis://127.0.0.1:6379".to_string(),
            };
            let client = redis::Client::open(url.as_str()).unwrap();
            let con = client.get_multiplexed_async_connection().await.unwrap();
            let session_state = Arc::new(GarnetSessionState::new(con.clone()));
            (Some(con), Some(session_state))
        } else {
            (None, None)
        };

        // Open shared RocksDB and session state if either index type needs it
        // (avoids LOCK conflict from opening the same unified DB path twice)
        let (rocks_db, rocks_session_state) = if test_run_config.element_index_type
            == IndexType::RocksDB
            || test_run_config.result_index_type == IndexType::RocksDB
        {
            let options = RocksIndexOptions {
                archive_enabled: false,
                direct_io: false,
            };
            let path = match env::var("ROCKS_PATH") {
                Ok(p) => p,
                Err(_) => "test-data".to_string(),
            };
            let db = open_unified_db(&path, &query_id, &options).unwrap();
            let session_state = Arc::new(RocksDbSessionState::new(db.clone()));
            (Some(db), Some(session_state))
        } else {
            (None, None)
        };

        // Configure the correct element index
        builder = match test_run_config.element_index_type {
            IndexType::Memory => builder,
            IndexType::Redis => {
                let con = redis_connection.clone().unwrap();
                let session_state = garnet_session_state.clone().unwrap();
                let element_index = GarnetElementIndex::new(&query_id, con, false, session_state);

                builder.with_element_index(Arc::new(element_index))
            }
            IndexType::RocksDB => {
                let options = RocksIndexOptions {
                    archive_enabled: false,
                    direct_io: false,
                };

                let db = rocks_db.clone().unwrap();
                let session_state = rocks_session_state.clone().unwrap();
                let element_index = RocksDbElementIndex::new(db, options, session_state);
                element_index.clear().await.unwrap();

                builder.with_element_index(Arc::new(element_index))
            }
        };

        // Configure the correct result index
        builder = match test_run_config.result_index_type {
            IndexType::Memory => builder,
            IndexType::Redis => {
                let con = redis_connection.clone().unwrap();
                let session_state = garnet_session_state.unwrap();
                let ari = GarnetResultIndex::new(&query_id, con, session_state);

                builder.with_result_index(Arc::new(ari))
            }
            IndexType::RocksDB => {
                let db = rocks_db.unwrap();
                let session_state = rocks_session_state.unwrap();
                let ari = RocksDbResultIndex::new(db, session_state);
                ari.clear().await.unwrap();

                builder.with_result_index(Arc::new(ari))
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
        println!(" - Result: {result:#?}");
    }
}
