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

use async_trait::async_trait;
use drasi_core::query::QueryBuilder;

pub mod index;
pub mod mssql_helpers;
pub mod redis_helpers;
pub mod sequence_counter;
pub mod temporal_retrieval;
pub mod use_cases;

#[cfg(test)]
mod in_memory;

#[async_trait]
pub trait QueryTestConfig {
    async fn config_query(&self, builder: QueryBuilder) -> QueryBuilder;
}
