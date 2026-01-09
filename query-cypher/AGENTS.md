# AGENTS.md: `query-cypher` Crate

## Architectural Intent
A **Cypher language frontend** for the Drasi engine. It translates Cypher query strings into the canonical `ast::Query` format defined in `query-ast`.

## Architectural Rules
*   **Contract**: MUST implement the `QueryParser` trait from `query-ast`.
*   **Output**: MUST produce a valid `ast::Query` that accurately represents the logic of the input Cypher string.
*   **Grammar**: The parser is defined using a **Parsing Expression Grammar (PEG)** via the `rust-peg` crate.

## Implementation Details
*   **`CypherParser`**: The public entry point struct.
*   **`grammar cypher() ...`**: The PEG definition block containing the rules for tokens, clauses, and expressions.
*   **Error Handling**: Maps `peg::error::ParseError` to `query_ast::api::QueryParseError`.
