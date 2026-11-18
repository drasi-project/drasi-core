# drasi-computation-template-common

The one template engine of the native sinks. The HTTP, gRPC, SSE and log sinks
and the PostgreSQL, MySQL and SQL Server stored-procedure sinks all compile and
render their templates here, so one template means the same thing in every sink.
It has no component factories.

## Semantics

| Concern | Rule |
|---|---|
| Engine | Handlebars, compiled once when the sink is constructed or reconfigured, never per change. |
| Helpers | `if`, `unless`, `each`, `with`, `lookup`, `raw`, `log`, `eq`, `ne`, `gt`, `gte`, `lt`, `lte`, `and`, `or`, `not`, `len`, `json` (`HELPERS`). Every helper call, in every branch, block, sub-expression and hash argument, is checked statically at compile time. Partials and decorators are rejected. |
| Strictness | Strict. Reading a field the change does not carry (`{{after.name}}` on a delete) is a render error, never an empty string. `{{json x}}` and `{{param x}}` reject a missing `x`. A block condition may test a missing value: guard optional parts with `{{#if after}}…{{/if}}`. A present `null` is a value. |
| Escaping | None. No sink emits HTML, so `{{x}}` and `{{{x}}}` both write the value unchanged. Write `{{json x}}` to emit a quoted, escaped JSON value inside a JSON body. |
| Context | `change_context`: the change notification. `operation`, `output` (`{alias, component, port}`), `sequence`, `timestamp` (when known), `identity`, and `element`, `before`, `after`, `removedProperties` (patches), `effectiveFrom` as the record carries them. The stored-procedure sinks add `deliveryId`. |
| Failures | `TemplateError` (`Syntax`, `Unsupported`, `Render`, `InvalidJson`, `NotCompiled`, `Context`). A render failure is deterministic; every sink reports it as a permanent delivery failure (or applies its documented render-failure policy), never a silent fallback. |

## API

- `Templates`: text templates keyed by their source text. `compile`, `render`
  and `render_json` (a body that must be JSON).
- `ParameterTemplate`: parameter-only SQL. `{{param …}}` is the only construct
  that may emit a value; it renders the dialect's `Placeholder` (`Dollar` `$1`,
  `Question` `?`, `AtP` `@P1`) and collects the value to bind. Plain `{{x}}`,
  raw `{{{x}}}`, top-level `{{json x}}`, partials, decorators and blocks other
  than `if`, `unless`, `each` and `with` are rejected at compile time. A `param`
  argument may be a sub-expression of a standard helper, for example
  `{{param (json after)}}` binds the JSON text. `validate_parameter_only`
  checks a source without compiling a registry.

Routing (which template applies to an output and operation) and the response
to a render failure stay in each sink, because they are part of each sink's
configuration contract.
