# File Reaction

File Reaction writes query result diffs to local or mounted filesystem paths.
Each diff can be formatted with Handlebars templates and persisted with append,
overwrite, or per-change behavior.

## Overview

This reaction subscribes to one or more queries and processes `ADD`, `UPDATE`,
and `DELETE` result diffs in timestamp order.

### Key capabilities

- `append` mode: append each rendered diff to a file
- `overwrite` mode: rewrite a single file with the latest rendered diff
- `per_change` mode: write each rendered diff to a unique file
- Handlebars templates per operation (`added`, `updated`, `deleted`)
- Filename templating including payload fields such as `{{after.id}}`
- Filename sanitization for filesystem safety
- Canonical default output envelope when no template is configured (see
  [Default output](#default-output))

## Configuration

```json
{
  "outputPath": "/data/drasi-out",
  "writeMode": "append",
  "filenameTemplate": "{{query_name}}.ndjson",
  "routes": {
    "orders-query": {
      "added": {
        "template": "{\"op\":\"add\",\"order\":{{json after}}}"
      },
      "updated": {
        "template": "{\"op\":\"update\",\"before\":{{json before}},\"after\":{{json after}}}"
      },
      "deleted": {
        "template": "{\"op\":\"delete\",\"order\":{{json before}}}"
      }
    }
  },
  "defaultTemplate": {
    "added": {
      "template": "{\"op\":\"add-default\",\"data\":{{json after}}}"
    }
  }
}
```

### Fields

| Field | Type | Description |
|---|---|---|
| `outputPath` | string | Base directory for output files. Supports secrets and `${ENV}` resolution. |
| `writeMode` | `append` \| `overwrite` \| `per_change` | File persistence strategy |
| `filenameTemplate` | string (optional) | Handlebars filename template. Supports secrets and `${ENV}` resolution. |
| `routes` | map | Per-query templates |
| `defaultTemplate` | object (optional) | Fallback templates when no route template matches |

### Default filename templates

- append: `{{query_name}}.log`
- overwrite: `{{query_name}}.json`
- per_change: `{{query_name}}_{{operation}}_{{uuid}}.json`

## Template context

| Variable | ADD | UPDATE | DELETE |
|---|---|---|---|
| `after` | ✅ | ✅ | ❌ |
| `before` | ❌ | ✅ | ✅ |
| `data` | ✅ | ✅ | ✅ |
| `operation` | ✅ | ✅ | ✅ |
| `query_name` | ✅ | ✅ | ✅ |
| `query_id` | ✅ | ✅ | ✅ |
| `timestamp` | ✅ | ✅ | ✅ |
| `metadata` | ✅ | ✅ | ✅ |
| `uuid` | ✅ | ✅ | ✅ |

`query_id` is an alias of `query_name`. For ADD, `data` is equivalent to
`after`; for DELETE, `data` is equivalent to `before`.

Aggregation diffs use the `updated` template and populate `before`, `after`,
and `data` (equivalent to `after`). The `operation` variable is set to
`"AGGREGATION"`.

`{{json value}}` helper serializes any value as JSON.

## Template resolution

For each diff, the body template is resolved in this order:

1. `routes[<full query id>]` for the matching operation.
2. `routes[<last dotted segment of the query id>]` for the matching operation
   (so `routes.orders` matches a wire id of `source.orders`).
3. `defaultTemplate` for the matching operation.
4. The default output envelope (below).

If a configured template fails to render at runtime, the reaction logs the
error and falls back to the default output envelope for that diff rather than
dropping it.

## Default output

When no template applies, the reaction writes one JSON object per diff using
the canonical camelCase envelope:

```json
{
  "queryId": "orders",
  "sequenceId": 42,
  "timestamp": "2026-06-09T19:06:44.123Z",
  "operation": "ADD",
  "after": { "id": "item-1" }
}
```

Presence of `before`/`after` follows the operation type: `ADD` carries
`after`; `UPDATE` carries `before` and `after`; `DELETE` carries `before`.
`metadata` is included only when the query result carries non-empty metadata.

## Filename safety

Rendered filenames are sanitized before write. The following characters are
replaced with `_`:

`/ \ : * ? " < > |` and `\0`

This allows payload-derived filenames like `item_{{after.id}}.json` while
preventing invalid or unsafe file names.

## Running checks

```bash
make build
make test
make integration-test
make lint
```

## Integration test strategy

Integration tests use:

- `DrasiLib` as runtime host
- `ApplicationSource` for deterministic change injection
- `TempDir` for isolated filesystem assertions

The integration suite verifies:

1. INSERT/UPDATE/DELETE written in append mode
2. overwrite mode keeps latest state
3. per_change mode creates payload-derived filenames
4. the default output envelope when no templates are configured
5. aggregation diffs use the `updated` template
