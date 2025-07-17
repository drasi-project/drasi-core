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

use thiserror::Error;

use crate::ast;

pub trait QueryParser: Send + Sync {
    fn parse(&self, input: &str) -> Result<ast::Query, QueryParseError>;
}

#[derive(Error, Debug)]
pub enum QueryParseError {
    #[error("parser error: {0}")]
    ParserError(Box<dyn std::error::Error + Send + Sync>),

    #[error("Non-grouped RETURN expressions must appear in GROUP BY clause")]
    MissingGroupByKey,
}
