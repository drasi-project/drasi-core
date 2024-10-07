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

use std::{
    process, result,
    str::FromStr,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct TestRunArgs {
    /// The test scenario to run
    #[arg(short, long)]
    scenario: String,

    /// The element index to use
    #[arg(short, long)]
    element_index: String,

    /// The result index to use
    #[arg(short, long)]
    result_index: String,

    /// The number of iterations to run
    /// If not specified, the default number of iterations for the scenario will be used.
    #[arg(short, long)]
    iterations: Option<u128>,

    /// The random number seed to use. If not specified, a random seed will be used by the scenario.
    #[arg(long)]
    seed: Option<u64>,
}

#[derive(Debug, Copy, Clone)]
pub enum IndexType {
    Memory,
    Redis,
    RocksDB,
}

impl FromStr for IndexType {
    type Err = String;

    fn from_str(s: &str) -> result::Result<Self, Self::Err> {
        match s {
            // "garnet" => Ok(IndexType::Garnet),
            "memory" => Ok(IndexType::Memory),
            "redis" => Ok(IndexType::Redis),
            "rocksdb" => Ok(IndexType::RocksDB),
            _ => Err(format!("Unknown index type: {}", s)),
        }
    }
}

#[derive(Debug)]
pub struct TestRunConfig {
    pub scenario: String,
    pub element_index_type: IndexType,
    pub result_index_type: IndexType,
    pub iterations: Option<u128>,
    pub seed: Option<u64>,
}

impl TestRunConfig {
    #[allow(clippy::print_stdout, clippy::unwrap_used)]
    pub fn new(args: TestRunArgs) -> TestRunConfig {
        let scenario = args.scenario;

        let element_index_type =
            IndexType::from_str(&args.element_index).unwrap_or_else(|_err: String| {
                println!("Invalid element index type: {}", args.element_index);
                process::exit(1);
            });

        let result_index_type =
            IndexType::from_str(&args.result_index).unwrap_or_else(|_err: String| {
                println!("Invalid result index type: {}", args.result_index);
                process::exit(1);
            });

        TestRunConfig {
            scenario,
            element_index_type,
            result_index_type,
            iterations: args.iterations,
            seed: args.seed,
        }
    }
}

#[derive(Debug)]
pub struct TestRunResult {
    scenario_name: String,
    element_index_type: IndexType,
    result_index_type: IndexType,
    bootstrap_start_time: u128,
    bootstrap_end_time: u128,
    bootstrap_duration_ms: u128,
    bootstrap_events: u128,
    last_bootstrap_event_start_time: u128,
    min_bootstrap_duration_ms: u128,
    max_bootstrap_duration_ms: u128,
    avg_bootstrap_duration_ms: f64,
    avg_bootstrap_events_per_sec: f64,
    run_start_time: u128,
    run_end_time: u128,
    run_duration_ms: u128,
    run_events: u128,
    last_run_event_start_time: u128,
    min_run_duration_ms: u128,
    max_run_duration_ms: u128,
    avg_run_duration_ms: f64,
    avg_run_events_per_sec: f64,
}

#[allow(clippy::unwrap_used)]
impl TestRunResult {
    pub fn new(
        scenario_name: String,
        element_index_type: IndexType,
        result_index_type: IndexType,
    ) -> TestRunResult {
        TestRunResult {
            scenario_name,
            element_index_type,
            result_index_type,
            bootstrap_start_time: 0,
            bootstrap_end_time: 0,
            bootstrap_duration_ms: 0,
            bootstrap_events: 0,
            last_bootstrap_event_start_time: 0,
            min_bootstrap_duration_ms: 0,
            max_bootstrap_duration_ms: 0,
            avg_bootstrap_duration_ms: 0.0_f64,
            avg_bootstrap_events_per_sec: 0.0_f64,
            run_start_time: 0,
            run_end_time: 0,
            run_duration_ms: 0,
            run_events: 0,
            last_run_event_start_time: 0,
            min_run_duration_ms: 0,
            max_run_duration_ms: 0,
            avg_run_duration_ms: 0.0_f64,
            avg_run_events_per_sec: 0.0_f64,
        }
    }

    pub fn start_bootstrap(&mut self) {
        self.bootstrap_start_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis();
    }

    pub fn start_bootstrap_event(&mut self) {
        self.last_bootstrap_event_start_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis();
    }

    pub fn end_bootstrap_event(&mut self) {
        let duration_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis()
            - self.last_bootstrap_event_start_time;

        self.bootstrap_events += 1;
        self.bootstrap_duration_ms += duration_ms;

        // Update min/max duration
        if self.bootstrap_events == 1 {
            self.min_bootstrap_duration_ms = duration_ms;
            self.max_bootstrap_duration_ms = duration_ms;
        } else {
            if duration_ms < self.min_bootstrap_duration_ms {
                self.min_bootstrap_duration_ms = duration_ms;
            }

            if duration_ms > self.max_bootstrap_duration_ms {
                self.max_bootstrap_duration_ms = duration_ms;
            }
        }
    }

    pub fn end_bootstrap(&mut self) {
        self.bootstrap_end_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis();

        self.bootstrap_duration_ms = self.bootstrap_end_time - self.bootstrap_start_time;

        if self.bootstrap_events > 0 {
            self.avg_bootstrap_duration_ms =
                self.bootstrap_duration_ms as f64 / self.bootstrap_events as f64;
            self.avg_bootstrap_events_per_sec =
                1000.0_f64 / (self.bootstrap_duration_ms as f64 / self.bootstrap_events as f64);
        } else {
            self.avg_bootstrap_duration_ms = 0.0_f64;
            self.avg_bootstrap_events_per_sec = 0.0_f64;
        }
    }

    pub fn start_run(&mut self) {
        self.run_start_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis();
    }

    pub fn start_run_event(&mut self) {
        self.last_run_event_start_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis();
    }

    pub fn end_run_event(&mut self) {
        let duration_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis()
            - self.last_run_event_start_time;

        self.run_events += 1;
        self.run_duration_ms += duration_ms;

        // Update min/max duration
        if self.run_events == 1 {
            self.min_run_duration_ms = duration_ms;
            self.max_run_duration_ms = duration_ms;
        } else {
            if duration_ms < self.min_run_duration_ms {
                self.min_run_duration_ms = duration_ms;
            }

            if duration_ms > self.max_run_duration_ms {
                self.max_run_duration_ms = duration_ms;
            }
        }
    }

    pub fn end_run(&mut self) {
        self.run_end_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis();

        self.run_duration_ms = self.run_end_time - self.run_start_time;

        if self.run_events > 0 {
            self.avg_run_duration_ms = self.run_duration_ms as f64 / self.run_events as f64;
            self.avg_run_events_per_sec =
                1000.0_f64 / (self.run_duration_ms as f64 / self.run_events as f64);
        } else {
            self.avg_run_duration_ms = 0.0_f64;
            self.avg_run_events_per_sec = 0.0_f64;
        }
    }
}
