# AGENTS.md: `query-ast` Crate

## Architectural Intent

The purpose of this crate is to **decouple the query engine from any specific query language** by defining a canonical, language-agnostic Abstract Syntax Tree (AST).

This crate is the "lingua franca" between parsers and the engine:
*   **Producers** (e.g., `query-cypher`) translate a query string into this AST.
*   **Consumers** (the `core` engine) operate exclusively on this AST.

This design makes the engine extensible. Supporting a new query language requires creating a new parser crate that targets this AST, with no changes to the `core` engine.

## Architectural Rules

*   **Language Agnostic**: AST structures must be generic and not tied to the syntax of a specific language.
*   **Stability**: This AST is a critical architectural boundary. Changes are high-impact and must be made carefully.
*   **Explicitness**: The AST must be an explicit and unambiguous representation of the query's logic.

## Key Components

*   **`ast.rs`**: Defines the Rust structs and enums that form the AST, rooted at `ast::Query`.
*   **`api.rs`**: Defines the public `QueryParser` trait, the contract all parser crates must implement.
