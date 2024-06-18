use thiserror::Error;

use crate::ast;

pub trait QueryParser: Send + Sync {
    fn parse(&self, input: &str) -> Result<ast::Query, QueryParseError>;
}

#[derive(Error, Debug)]
pub enum QueryParseError {
    #[error("parser error: {0}")]
    ParserError(Box<dyn std::error::Error + Send + Sync>),
}