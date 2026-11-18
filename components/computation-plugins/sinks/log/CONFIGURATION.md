# Native log sink: configuration and management

Implementation `drasi.standard/log`, version `1`, configuration version `1`.
For how the sink works, see [DESIGN.md](DESIGN.md).

```sh
cargo build -p drasi-computation-sink-log --features dynamic-plugin
```

## Fields

All keys use camelCase. Unknown keys, including unknown keys inside `routes`
and `defaultTemplate`, are rejected. The published configuration schema carries
the same descriptions, defaults and typed nested schemas.

| Field | Type | Default | Allowed values / rule | Update |
|---|---|---|---|---|
| `outputs` | object | `{}` | Alias to `{component, port?}`. Each alias must select exactly one incoming edge. Unaliased outputs are named by their component UUID. | Replace |
| `routes` | object | `{}` | Route name to [operation templates](#operation-templates). See [Routing](#routing). | Pause |
| `defaultTemplate` | object | none | [Operation templates](#operation-templates) used when the output's route does not define the operation. | Pause |
| `format` | string | `json` | `json`, `compact`. The representation of a change without a template, and of a failed render under `fallback`. | Pause |
| `includeHeader` | boolean | `false` | Emit one header line per non-empty input; change lines are then indented. | Pause |
| `level` | string | `info` | `error`, `warn`, `info`, `debug`, `trace`. Applies to `output: log`. | Pause |
| `target` | string | `drasi.log` | Nonempty, no control characters. The `log` record target for `output: log`. Fallback warnings always use this target. | Pause |
| `output` | string | `log` | `log` (the host's logger, also when loaded as a plugin library), `stdout`, `stderr`. | Pause |
| `renderFailurePolicy` | string | `fallback` | `fallback`, `strict`. See [Render failures](#render-failures). | Pause |
| `onPermanentFailure` | string | `stall` | `stall`, `skip`. What the graph does with an input that failed permanently (a `strict` render failure). See [Render failures](#render-failures). | Replace |

A change to the incoming bindings (adding or removing an edge) also requires
Replace. No field is secret.

### Operation templates

```json
{ "added":   { "template": "..." },
  "updated": { "template": "..." },
  "deleted": { "template": "..." } }
```

Each key is optional; `template` is required in each present key. `added`
serves `ADD`, `updated` serves `REPLACE` and `PATCH`, `deleted` serves
`DELETE`. An empty template selects `format` for that operation (it does not
fall through to `defaultTemplate`).

## Output lines

Every line starts with `[<sink component UUID>]`.

| Case | Line |
|---|---|
| Change, no header | `[id] <message>` |
| Header (`includeHeader`) | `[id] Output '<alias>' sequence <n> (<count> changes):` |
| Change after a header | `[id]   <message>` (three spaces) |

`<message>` is the rendered template, or:

- `format: json`: the complete template context below as compact JSON;
- `format: compact`: `[ADD] <after>`, `[UPDATE] <before> -> <after>` (for
  `REPLACE` and `PATCH`; a patch that removes properties appends
  ` removed ["name", ...]`), `[DELETE] <before>`. Images are compact JSON; an
  image the record does not carry is shown as the identity
  `{"id":…,"namespace":…}`.

With `output: log`, each line is one record at `level` with `target`. With
`stdout`/`stderr`, all lines of one input are written together and flushed
before the input completes; a write error fails the input.

## Template reference

Templates use [Handlebars](https://handlebarsjs.com/) syntax, rendered without
HTML escaping. A missing value renders as an empty string.

### Context

The context is the native truthful-change object (also the `format: json`
body):

| Key | Meaning |
|---|---|
| `operation` | `ADD`, `REPLACE`, `PATCH` or `DELETE` |
| `output` | `{alias, component, port}`: the configured output that produced the change |
| `sequence` | The producing stream's sequence for the envelope |
| `timestamp` | RFC 3339 event time, only when the envelope carries one |
| `identity` | `{namespace, id}`: the element identity (for a query row, `id` is the row identity) |
| `element` | `{labels, kind?, inNode?, outNode?}`, when the record carries it |
| `before` | The before image, only when the record carries one |
| `after` | The full element for `ADD`/`REPLACE`; only the properties a `PATCH` sets |
| `removedProperties` | Property names a `PATCH` removes (patches only) |
| `effectiveFrom` | The element's business time, when known |

For a continuous query, `before` and `after` are the result row values.

### Helpers

| Helper | Use |
|---|---|
| `json` | `{{json after}}`: compact JSON of any value; a missing value renders `null` |
| Built-ins | `if`, `unless`, `each`, `with`, `lookup`, `raw`, `log`, `eq`, `ne`, `gt`, `gte`, `lt`, `lte`, `and`, `or`, `not`, `len` |

Any other helper, any partial (`{{> name}}`) and any decorator (`{{*inline}}`)
is rejected when the sink is constructed or an update is assessed. Syntax errors
are rejected the same way.

### Routing

A route name selects outputs by one rule, used for both validation and lookup:

1. a route named exactly as an output alias applies to that output;
2. otherwise a route `r` applies to every output whose alias ends with `.r`
   (`orders` applies to `east.orders` and `west.orders`, not to `myorders`).

Construction fails when two suffix routes match one alias (add a route named
for that alias), and when a route applies to no bound output, including a
suffix route that exact routes always shadow.

Per operation: the matching route's template if it defines one; otherwise
`defaultTemplate`'s; otherwise `format`.

### Render failures

A template that passed validation can still fail while rendering, for example
`{{lookup after}}` (missing argument) or `{{#each}}` without a value.

| `renderFailurePolicy` | Effect |
|---|---|
| `fallback` (default) | The change is emitted in `format`, and a `warn` record on `target` names the output, sequence, operation and error. The input completes and is not retried. |
| `strict` | Nothing from the input is emitted and it fails permanently. With `onPermanentFailure: stall` (default) the sink records a terminal failure and emits nothing more until the template is fixed; with `skip` the graph logs and counts the input and continues. |

A failed stdout/stderr write is transient: the graph retries the same input in
place with backoff until it is written.

## Examples

Log every change as JSON at `info` on target `drasi.log`:

```json
{}
```

Legacy-style output on stdout, with a header per input:

```json
{ "output": "stdout", "format": "compact", "includeHeader": true }
```

Per-output templates with a default and a dotted-suffix route:

```json
{
  "outputs": {
    "east.orders": { "component": "5c3e49cc-32f8-5b75-888d-30b75b572221", "port": "out" },
    "sensors":     { "component": "7d2a1f0e-4c3b-4a59-8e6d-1f2a3b4c5d6e" }
  },
  "routes": {
    "orders":  { "added":   { "template": "new order {{after.id}}: {{json after}}" },
                 "updated": { "template": "{{operation}} order {{identity.id}}" } },
    "sensors": { "updated": { "template": "{{after.id}} is now {{after.temperature}}" } }
  },
  "defaultTemplate": { "deleted": { "template": "[DELETE] {{output.alias}} {{identity.id}}" } },
  "level": "debug",
  "target": "app.changes",
  "renderFailurePolicy": "strict"
}
```

## Update and assess matrix

| Proposed change | Assessment |
|---|---|
| Any of `routes`, `defaultTemplate`, `format`, `includeHeader`, `level`, `target`, `output`, `renderFailurePolicy`; or no change | Pause (InPlace), data preserved |
| `outputs`, `onPermanentFailure`, or the incoming bindings | Replace (Recreate) |
| Invalid configuration: unknown field or value, syntax error, unknown helper, partial, unmatched or ambiguous route | Error (rejected) |

An in-place update is applied inside a completed Pause and takes effect for the
next input.

## Removal

The sink owns no state. Removal with `Erase` succeeds and is idempotent; lines
already written stay in the destination.

## Legacy configuration mapping

This is a breaking change; existing legacy log reactions are not migrated.

| Legacy (`reactions/log`) | Native |
|---|---|
| `routes` keyed by query name | `routes` keyed by output alias (or dotted suffix); declare `outputs` aliases to keep readable names |
| `defaultTemplate` | `defaultTemplate` (same shape; `template` is now required in each present operation) |
| Always stdout | `output: "stdout"` |
| Header line | `includeHeader: true` |
| No-template `[ADD] …` / `[UPDATE] … -> …` / `[DELETE] …` | `format: "compact"` |
| Render error falls back | `renderFailurePolicy: "fallback"` (default) |

Template context:

| Legacy key | Native equivalent |
|---|---|
| `query_id` | `output.component` (the producing component UUID) |
| `query_name`, `input_alias` | `output.alias` |
| `operation` `ADD` / `UPDATE` / `DELETE` | `ADD` / `REPLACE` or `PATCH` / `DELETE` |
| `after` | `after` (for `PATCH`, only the changed properties; see `removedProperties`) |
| `before` | `before` (absent when the record carries no before image) |
| `data` (update only) | No equivalent; use `before`/`after` |
| none | `identity`, `element`, `sequence`, `timestamp`, `effectiveFrom`, `output.port` |
