# AGENTS.md: `core/src/path_solver` Module

## Architectural Intent
The **structural graph pattern matching engine**. It executes `MATCH` clauses by traversing the graph topology starting from a specific "anchor" element.

*   **Scope**: Responsible *only* for finding valid topological paths that connect the anchor to other elements.
*   **Output**: Produces a set of `MatchPathSolution`s (variable bindings) for downstream processing.

## Architectural Rules
*   **Topological Only**: The solver assumes elements are already pre-filtered into "slots" (by the Index). It does NOT evaluate property predicates (like `n.age > 10`) during traversa
*   **Anchor-Driven**: Solving MUST always start from a specific `anchor_element` and `anchor_slot`. Full graph scans are prohibited.
*   **Depend only on `ElementIndex`**: All graph access is mediated via the `ElementIndex` trait.

## Key Design Decisions
*   **Slot-Based Optimization**: The query pattern is compiled into `MatchPath` "slots". Elements are pre-assigned to these slots in the `ElementIndex`. The solver simply traverses connections between valid slot members, avoiding expensive expression evaluation in the hot loop.
*   **BFS Traversal**: Uses an asynchronous, Breadth-First Search (BFS) to explore paths layer-by-layer.
*   **Internal Streaming**: The core traversal (`create_solution_stream`) is implemented as an async stream to support concurrency (`parallel_solver` feature), though the public API currently returns a materialized `HashMap`.
*   **MatchPathSolution**: A lightweight cursor structure that tracks the state of a partial or complete match.