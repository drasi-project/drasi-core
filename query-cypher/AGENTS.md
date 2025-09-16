# AGENTS.md: `query-cypher` Crate

## Architectural Intent

This crate provides a **concrete implementation of a Cypher query language parser**. It is a language frontend "plugin" that fulfills the contract defined in the `query-ast` crate.

Its single responsibility is to translate a raw Cypher query string into the canonical `ast::Query` format, enabling the `drasi-core` engine to execute Cypher queries.

## Architectural Rules

*   **Implement `QueryParser` Trait**: The public `CypherParser` struct MUST implement the `query_ast::api::QueryParser` trait.
*   **Correctness**: The parser must correctly parse all supported Cypher syntax into the corresponding AST structures.
*   **Clear Error Reporting**: Parsing failures must produce clear error messages indicating the location and nature of the syntax error.

## Key Design Decisions

*   **Parsing Expression Grammar (`peg`)**: A PEG is used to implement the parser via the `peg` crate. This was chosen because it allows the grammar to be defined declaratively, making the parser easier to understand, maintain, and extend compared to a hand-written parser.
