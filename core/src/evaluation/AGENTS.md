# AGENTS.md: `core/src/evaluation` Module

## Architectural Intent

This module is the computational core of the engine. It executes the query logic (`WHERE`, `WITH`, `RETURN`) on the graph solutions found by the `path_solver`.

The architecture is a **multi-stage, context-passing pipeline**. The key design principle is the separation between the high-level orchestration of data flow (`QueryPartEvaluator`) and the low-level, stateless computation of individual expressions (`ExpressionEvaluator`).

## Architectural Rules

*   **Stateless Evaluation**: The `ExpressionEvaluator` MUST be stateless. It receives all necessary information via its arguments, making it a pure and predictable function.
*   **Context-Driven Flow**: The `ExpressionEvaluationContext` and `QueryPartEvaluationContext` structs carry the state of a data "row" as it flows through the query.
*   **Extensibility via Registration**: New query functions are added by implementing the appropriate function `trait` and registering them in the `FunctionRegistry`. This is the primary extension point.

## Core Sub-module Intent

*   **`parts` (`QueryPartEvaluator`)**: Orchestrates the flow of data between the major clauses (`WITH`, `RETURN`) of a query, acting as the pipeline manager.
*   **`expressions` (`ExpressionEvaluator`)**: Provides a single, consistent, stateless service to compute the value of any expression by recursively traversing its AST.
*   **`functions` (`FunctionRegistry`)**: Acts as a central, extensible repository for all built-in functions, decoupling the evaluator from the function implementations.
*   **`variable_value` (`VariableValue`)**: Provides a unified, dynamically-typed value representation for all data manipulated during evaluation, which is essential as variable types are not known at compile time.