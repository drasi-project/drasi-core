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

use std::collections::HashSet;
use thiserror::Error;

use crate::ast;

const ERR_PARSE: &str = "parser error";
const ERR_MISSING_GROUP_BY: &str = "Non-grouped RETURN expressions must appear in GROUP BY clause";
const ERR_UNALIASED_COMPLEX: &str = "Complex expression must have an alias";
const ERR_ID_NOT_IN_SCOPE: &str = "Identifier not found in current scope";

pub trait QueryParser: Send + Sync {
    fn parse(&self, input: &str) -> Result<ast::Query, QueryParseError>;
}

pub trait QueryConfiguration: Send + Sync {
    fn get_aggregating_function_names(&self) -> HashSet<String>;
}

#[derive(Error, Debug)]
pub enum QueryParseError {
    #[error("{ERR_PARSE}: {0}")]
    ParserError(Box<dyn std::error::Error + Send + Sync>),

    #[error("{ERR_MISSING_GROUP_BY}")]
    MissingGroupByKey,

    #[error("{ERR_UNALIASED_COMPLEX}: {0}")]
    UnaliasedComplexExpression(String),

    #[error("{ERR_ID_NOT_IN_SCOPE}")]
    IdentifierNotInScope,
}

// This is used to map the error to a static string for the PEG parser.
// The PEG parser requires &'static str, so we must define const strings. This is a PEG limitation.
// TODO: Should improve this later to provide a more detailed error message.
impl QueryParseError {
    pub fn as_peg_error(&self) -> &'static str {
        match self {
            QueryParseError::ParserError(_) => ERR_PARSE,
            QueryParseError::MissingGroupByKey => ERR_MISSING_GROUP_BY,
            QueryParseError::UnaliasedComplexExpression(_) => ERR_UNALIASED_COMPLEX,
            QueryParseError::IdentifierNotInScope => ERR_ID_NOT_IN_SCOPE,
        }
    }
}
