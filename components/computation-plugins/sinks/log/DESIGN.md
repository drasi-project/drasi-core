# Native log sink: design

`drasi.standard/log` writes each graph change it receives as one text line. This
document describes how it works. [CONFIGURATION.md](CONFIGURATION.md) lists
every setting, the template reference and the legacy mapping.

## Purpose and scope

The sink is for development, demos and operational tracing. It accepts the
standard graph-change schema on its `in` port, so it works directly behind a
source, a transformer or a continuous query. For each change it either renders
a Handlebars template chosen by output and operation, or formats the change as
JSON or as the legacy compact form. It then sends the line to the host's
logger (through the `log` facade), stdout or stderr.

The sink stores nothing. It has no checkpoint and no buffer; stdout and stderr
writes run on a shared writer thread per stream.

## Architecture

```
InputEnvelope ─► SinkOutputs::output_for(stream) ─► alias
             └─► GraphChangeBatch::notifications ─► per change:
                     Renderer: alias + operation group ─► template | format
                 ─► lines ([id] header?, [id] message…)
                 ─► Emitter: log facade (host logger) | stdout/stderr writer thread (write_all + flush)
```

| Piece | Location | Responsibility |
|---|---|---|
| `LogSinkFactory` | `src/lib.rs` | Metadata, configuration details, factory update planning, construction |
| `LogSinkConfig` | `src/config.rs` | Fields, defaults, published schema (typed nested schemas for `routes` and `defaultTemplate`) |
| `Renderer` | `src/render.rs` | Template compilation and static validation, route resolution, `json` helper, plain formats |
| `Emitter` | `src/output.rs` | Writing lines to the selected output |
| `LogSink` | `src/component.rs` | Lifecycle hooks, management (assess/apply), `handle` |

## Rendering

**Compile once.** Construction (and an in-place update) compiles every
non-empty template once and registers it in a private Handlebars registry.
Nothing is parsed per change.

**Static validation.** Each compiled template is walked, including blocks,
`else` branches, sub-expressions and hash arguments:

- every helper call must name a supported helper: the Handlebars built-ins
  (`if`, `unless`, `each`, `with`, `lookup`, `raw`, `log`, `eq`, `ne`, `gt`,
  `gte`, `lt`, `lte`, `and`, `or`, `not`, `len`) plus `json`;
- partials (`{{> p}}`) and decorators (`{{*inline}}`) are rejected, because
  the sink registers none.

A walk is used instead of a trial render because a trial render only visits the
branches that one sample context selects. The helper list is fixed in this
crate, so a Handlebars feature enabled elsewhere in the build (for example
`string_helpers`) never changes which templates are accepted. Strict mode
(missing values are errors) is **not** enabled: a missing value renders as an
empty string, as it did in the legacy reaction, so a template can serve
operations with and without a `before` image.

**No HTML escaping.** Log lines are not HTML, so `{{after.name}}` renders the
value unchanged (the legacy reaction escaped `"`, `<`, `&` and so on).

**Routing.** Each output alias is resolved to at most one route once, at
construction, by one rule that both validation and lookup use: a route named
exactly as the alias wins; otherwise a route `r` matches when the alias ends
with `.r`. Two suffix routes matching one alias are rejected as ambiguous, and
every route must be selected for at least one bound output, so a route that
matches nothing, or that an exact route always shadows, is rejected. Per
operation group, the route's template is used if the route defines one,
otherwise the `defaultTemplate`'s, otherwise the configured `format`.

**Render failures.** A template that passes validation can still fail at render
time (for example `{{lookup after}}` with a missing argument). The
`renderFailurePolicy` decides:

- `fallback` (default, legacy behaviour): the change is emitted in the
  configured `format` and a `warn` record names the output, sequence, operation
  and error. The input completes. A failure is never retried.
- `strict`: `handle` fails, and nothing from that input is emitted, because
  every change is rendered before any line is written. The host then applies
  its own failure handling to the input. Because the failure is deterministic,
  use `strict` only when a stalled pipe is preferable to an unformatted line.

## Delivery profile

Completion is `Handled`. `handle` returns `Ok` only after every line of the
input was emitted:

- `log` output: every record was passed to the `log` facade. Loaded as a
  plugin library, the SDK's logging bridge forwards each record to the host's
  logger (the host installs it when it loads the library, with the level it had
  then); statically linked, the sink shares the host's facade. The facade
  cannot report failures, and the host's logger decides whether, when and where
  records are written (including its own buffering and level filtering).
- `stdout`/`stderr` output: all lines of the input were written in one
  `write_all` and flushed by the stream's writer thread (one per stream, shared
  by every log sink in the process or library). `handle` awaits that write
  without blocking the async runtime and stays cancellable; a write already
  queued is not cancelled. A write or flush error (for example a closed pipe)
  fails the input transiently, and the graph retries it in place.
- A template that fails under `renderFailurePolicy: strict` is a permanent
  failure: `onPermanentFailure` decides whether the sink stalls (default) or
  skips and counts the input (see the
  [delivery-failure model](../NATIVE-SINK-GUIDE.md#delivery-failures)).

Delivery is at-least-once. A crash or failure after some or all lines were
written, but before the host recorded completion, causes the input to be
delivered again, and its lines appear twice. Nothing is lost while the host
retains the input. A stalled reader of a stdout/stderr pipe stalls the sink's
input (it keeps waiting for its write) but never a runtime thread, and Stop
can still abort it.

## Lifecycle

| Hook | Behaviour |
|---|---|
| construction | Parses the configuration, binds outputs from the declared incoming edges, compiles and validates all templates and resolves routes. Any error fails construction. |
| `initialize` | Nothing to bind; succeeds. |
| `start` | Opens input handling. A Start whose cancellation was already requested returns `Cancelled` and leaves the sink startable. A second Start while running is refused. |
| `pause` | The sink has no worker, and `handle` completes before returning, so Pause succeeds once the host has settled the in-flight `handle` (host-driven pause). Input offered while paused is refused. |
| `resume` | Accepts input again. |
| `stop` | Closes input handling. Nothing is buffered, so `Drain` and `Abort` are identical and complete at once. Stop before Start succeeds. Input offered while stopped is refused. |
| `dispose` | The sink owns no state; the lines belong to the destination. `Preserve` and `Erase` are idempotent no-ops (`erases_owned_state` is declared so removal can request Erase). Refused while running. |

## Management

The factory plans updates (`supports_update_planning`) and the live object
exports `NativeManagement`. Supported modes are `InPlace` and `Recreate`.

| Change | Boundary |
|---|---|
| `routes`, `defaultTemplate`, `format`, `includeHeader`, `level`, `target`, `output`, `renderFailurePolicy` | Pause (InPlace) |
| `outputs` aliases, `onPermanentFailure`, or the incoming bindings (`current.inputs != proposed.inputs`) | Replace (Recreate) |

Assessment validates the proposed configuration exactly as construction would
(against the proposed inputs when the request carries them, otherwise templates
only), so an invalid template or route is an assessment error, never a
Replace. `apply` re-parses both configurations, refuses when the current
configuration no longer matches the live one or the plan has changed, requires
a completed Pause (or a stopped sink), builds the new renderer completely, and
only then swaps it in. A refused apply leaves the sink unchanged.

## Security

Templates and configuration are trusted operator input. Lines contain change
data verbatim, including any sensitive property values; the sink performs no
redaction. Choose `output` and the host logger accordingly.

## Differences from the legacy log reaction

| Legacy (`reactions/log`) | Native |
|---|---|
| Always `println!` to stdout | `output`: `log` (default, with `level` and `target`), `stdout` or `stderr` |
| Header line per query result | Optional `includeHeader` |
| Context: `query_id`, `query_name`, `input_alias`, `operation`, `before`, `after`, `data` | Native truthful-change context (see CONFIGURATION.md) |
| Operations `ADD`, `UPDATE`, `DELETE` | `ADD`, `REPLACE`, `PATCH`, `DELETE`; `updated` templates serve `REPLACE` and `PATCH` |
| Dotted-suffix routes accepted by validation, never applied | Consistent resolution in validation and lookup |
| Templates parsed per message | Compiled once at construction |
| Unknown helpers failed only at render | Rejected at construction |
| Render error: debug log, compact fallback | `renderFailurePolicy` (`fallback` default with a warning, or `strict`) |
| HTML-escaped values | No escaping |
| Durable reaction checkpoint | None: the graph pipe owns retry and replay |
