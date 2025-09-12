# AGENTS.md: `query-ast` Crate

## Architectural Intent
The **canonical, language-agnostic Abstract Syntax Tree (AST)** for the Drasi engine. It serves as the stable contract between query language parsers (producers) and the evaluation 
      engine (consumer).

## Architectural Rules
*   **Language Agnosticism**: The AST must represent the *logic* of a query, not the syntax of a specific language (like Cypher or SQL).
*   **Stability**: This crate is a central dependency. Breaking changes here propagate to all parsers and the core engine.
*   **Completeness**: The AST must be expressive enough to represent all supported query features (filtering, projection, aggregation, temporal logic).

## Core Structures (`ast.rs`)
*   **`Query`**: The root node, containing a list of `QueryPart`s.
*   **`QueryPart`**: Represents a linear stage of a query (e.g., `MATCH ... RETURN ...`), containing `match_clauses`, `where_clauses`, and a `return_clause`.
*   **`Expression`**: A recursive enum representing all computational logic (`BinaryExpression`, `FunctionExpression`, `Literal`, etc.).

## Interfaces (`api.rs`)
*   **`QueryParser`**: The trait that all parser crates must implement to translate string input into an `ast::Query`.
