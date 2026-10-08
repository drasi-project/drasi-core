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
- Raw JSON fallback when no templates are configured: each diff is serialized as a JSON object containing `operation`, `query_name`, `timestamp`, and the relevant data fields

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
| `outputPath` | string | Base directory for output files |
| `writeMode` | `append` \| `overwrite` \| `per_change` | File persistence strategy |
| `filenameTemplate` | string (optional) | Handlebars filename template |
| `routes` | map | Per-query templates |
| `defaultTemplate` | object (optional) | Fallback templates when no route template is present |

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
| `timestamp` | ✅ | ✅ | ✅ |
| `uuid` | ✅ | ✅ | ✅ |

For ADD, `data` is equivalent to `after`; for DELETE, `data` is equivalent to `before`.

Aggregation diffs use the `updated` template and populate `before`, `after`, and `data` (equivalent to `after`). The `operation` variable is set to `"AGGREGATION"`.

`{{json value}}` helper serializes any value as JSON.

## Filename safety

Rendered filenames are sanitized before write. The following characters are
replaced with `_`:

`/ \ : * ? " < > |` and `\0`

This allows payload-derived filenames like `item_{{after.id}}.json` while
preventing invalid or unsafe file names.

## Stop and Restart

The processing worker is owned before it can run. Stop signals shutdown and waits
up to two seconds for that worker without aborting an in-flight file operation.
A timeout or cancelled stop leaves cleanup pending and blocks restart until a
later stop finishes. Cancelled startup and worker panics also require cleanup;
only completed cleanup reports Stopped.

This preserves the existing acceptance-only delivery contract. Enqueue is not
confirmation that bytes were written, output errors retain their existing
logged-error policy, and queued but not yet processed results are drained during
successful cleanup. This is not exactly-once delivery or whole-transaction file
publication.

Lifecycle tests hold actual append/overwrite operations, cancel and time out
stops, then verify exact completed output. Three restart cycles cover every write
mode. All integrations permanently shut down their library instances.

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
4. raw JSON fallback when no templates are configured
5. aggregation diffs use the `updated` template
