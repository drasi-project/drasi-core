# AGENTS.md: `core/src/path_solver` Module

## Architectural Intent

This module is the **graph pattern matching engine**, responsible for executing the `MATCH` clauses of a query. It takes a graph pattern and an "anchor" element (from a `SourceChange`) and finds all matching sets of graph elements.

The core design principle is to **decouple graph traversal logic from query evaluation**. This module's sole responsibility is to generate a stream of solutions (variable bindings), which the `evaluation` module then filters and projects.

## Architectural Rules

*   **Performance is Critical**: The path solving implementation must be highly optimized.
*   **Correctness over Feature Completeness**: The solver must be correct for a well-defined, performant subset of `MATCH` functionality.
*   **Depend only on `ElementIndex`**: All graph data access MUST go through the `ElementIndex` trait.

## Key Design Decisions

*   **Anchor-Based Solving**: Solving is not a full graph scan. It is always initiated from an `anchor` element affected by a `SourceChange`. This is a fundamental optimization that constrains the search space.
*   **Breadth-First Search (BFS)**: A BFS-style traversal algorithm is used to explore the graph layer by layer from the anchor, branching to find all possible paths.
*   **Compiled `MatchPath`**: The `MATCH` AST is pre-compiled into a `MatchPath` structure. This is an optimization to create a solver-friendly representation of the pattern, avoiding repeated AST interpretation in the hot loop.
*   **Solution Streaming**: The solver produces a `Stream` of solutions, not a `Vec`. This allows the downstream `evaluation` module to begin processing immediately, improving pipeline parallelism and reducing memory pressure.
