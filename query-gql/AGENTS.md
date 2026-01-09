# AGENTS.md: `query-gql` Crate

## Architectural Intent
A **GQL (Graph Query Language) language frontend** for the Drasi engine. It translates GQL query strings into the canonical `ast::Query` format defined in `query-ast`.

## Architectural Rules
*   **Contract**: MUST implement the `QueryParser` trait from `query-ast`.
*   **Output**: MUST produce a valid `ast::Query` that accurately represents the logic of the input GQL string.
*   **Grammar**: The parser is defined using a **Parsing Expression Grammar (PEG)** via the `rust-peg` crate.

## Implementation Details
*   **`GQLParser`**: The public entry point struct.
*   **`grammar gql() ...`**: The PEG definition block containing the rules for tokens, clauses, and expressions.
*   **Error Handling**: Maps `peg::error::ParseError` to `query_ast::api::QueryParseError`.
*   **Language Features**: Supports `MATCH`, `WHERE`, `RETURN`, `FILTER`, `LET`, `YIELD`, `GROUP BY`, and `NEXT` (linear composition of query parts).