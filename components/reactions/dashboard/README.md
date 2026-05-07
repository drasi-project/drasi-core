# Dashboard Reaction

`drasi-reaction-dashboard` serves an embeddable web dashboard UI from a Drasi reaction. It provides:

- Drag-and-drop visual dashboard layout (Gridstack.js)
- Chart/table/KPI/gauge/text/map widgets (ECharts + HTML widgets)
- Real-time query-result updates over WebSocket
- Dashboard configuration CRUD via REST API
- Persistence through DrasiLib `StateStoreProvider`

## Configuration

Use the builder pattern:

```rust,ignore
use drasi_reaction_dashboard::DashboardReaction;

let reaction = DashboardReaction::builder("my-dashboard")
    .with_queries(vec!["sensor-query".to_string(), "alerts-query".to_string()])
    .with_host("0.0.0.0")
    .with_port(3000)
    .with_heartbeat_interval_ms(30_000)
    .with_results_api_url("http://localhost:8080")
    .build()?;
```

### Fields

| Field | Type | Default | Description |
|---|---|---|---|
| `host` | `String` | `"0.0.0.0"` | Bind host for HTTP + WebSocket server |
| `port` | `u16` | `3000` | Bind port |
| `heartbeat_interval_ms` | `u64` | `30000` | WebSocket heartbeat interval |
| `results_api_url` | `Option<String>` | `None` | Optional results API URL for snapshot bootstrap |
| `priority_queue_capacity` | `Option<usize>` | `None` | Maximum pending change events in the priority queue; unbounded if not set |
| `queries` | `Vec<String>` | `[]` | Subscribed query IDs |

## Predefined Dashboards

You can ship dashboards as part of your reaction configuration using `.with_dashboard()`.
These are seeded into the state store on first startup. If a dashboard with the same ID
already exists (e.g. the user modified it via the UI), it is **not** overwritten.

```rust,ignore
use drasi_reaction_dashboard::{
    DashboardReaction, DashboardConfig, DashboardWidget, GridOptions, WidgetGrid,
};

let dashboard = DashboardConfig::with_id(
    "production-metrics",              // Stable ID — won't duplicate on restart
    "Production Metrics".to_string(),
    GridOptions::default(),
    vec![
        DashboardWidget {
            id: "w-table".to_string(),
            widget_type: "table".to_string(),
            title: "All Sensors".to_string(),
            grid: WidgetGrid { x: 0, y: 0, w: 8, h: 4 },
            config: serde_json::json!({
                "queryId": "sensor-query",
                "columns": ["name", "value", "unit"]
            }),
        },
        DashboardWidget {
            id: "w-kpi".to_string(),
            widget_type: "kpi".to_string(),
            title: "Sensor Count".to_string(),
            grid: WidgetGrid { x: 8, y: 0, w: 4, h: 2 },
            config: serde_json::json!({
                "queryId": "sensor-query",
                "valueField": "name",
                "aggregation": "count",
                "label": "Sensors"
            }),
        },
    ],
);

let reaction = DashboardReaction::builder("my-dashboard")
    .with_query("sensor-query")
    .with_dashboard(dashboard)
    .build()?;
```

### Widget Types & Config

| Type | `widget_type` | Required Config Fields |
|---|---|---|
| Table | `"table"` | `queryId`, `columns` (array of field names) |
| Bar Chart | `"bar_chart"` | `queryId`, `categoryField`, `valueFields` (array) |
| Line Chart | `"line_chart"` | `queryId`, `categoryField`, `valueFields` (array) |
| Pie Chart | `"pie_chart"` | `queryId`, `nameField`, `valueField` |
| Gauge | `"gauge"` | `queryId`, `valueField`, `min`, `max`, `aggregation` |
| KPI | `"kpi"` | `queryId`, `valueField`, `aggregation`, `label` |
| Markdown | `"text"` | `queryId`, `template` (Handlebars + Markdown) |
| Map | `"map"` | `queryId`, `latField`, `lngField`, `valueField` |

### Aggregation Modes (KPI & Gauge)

The `aggregation` field controls how multiple rows are reduced to a single value:

| Mode | Description |
|---|---|
| `"last"` | Last updated row (default) |
| `"first"` | First row in the result set |
| `"sum"` | Sum of all values in the field |
| `"avg"` | Average of all values |
| `"min"` | Minimum value |
| `"max"` | Maximum value |
| `"count"` | Number of rows |
| `"filter"` | Single row matching `filterField`/`filterValue` |

### Markdown Widget Template

The markdown widget uses [Handlebars](https://handlebarsjs.com/) templates rendered as Markdown.
Available context:

| Variable | Description |
|---|---|
| `rows` | Array of all result rows |
| `count` | Number of rows |
| `latest` | Last updated row |
| `aggregation` | Query-level aggregation value (if any) |

Built-in helpers: `sum`, `avg`, `min`, `max`, `count`, `format` (currency/percent/compact),
`eq`, `gt`, `lt`, `gte`, `lte`.

```handlebars
## {{count}} sensors online

{{#each rows}}
- **{{this.name}}**: {{this.value}} {{this.unit}}
{{/each}}

Average reading: {{format (avg "value") "compact"}}
```

## HTTP API

| Method | Path | Description |
|---|---|---|
| `GET` | `/` | Dashboard SPA |
| `GET` | `/assets/*` | Static assets |
| `GET` | `/api/dashboards` | List dashboards |
| `POST` | `/api/dashboards` | Create dashboard |
| `GET` | `/api/dashboards/:id` | Get dashboard |
| `PUT` | `/api/dashboards/:id` | Update dashboard |
| `DELETE` | `/api/dashboards/:id` | Delete dashboard |
| `GET` | `/api/queries` | List subscribed queries |
| `GET` | `/ws` | WebSocket stream endpoint |

## WebSocket Protocol

Client subscription message:

```json
{ "type": "subscribe", "query_ids": ["test-query"] }
```

Server query-result message:

```json
{
  "type": "query_result",
  "query_id": "test-query",
  "timestamp": 1714500000000,
  "results": [
    { "op": "add", "data": { "name": "Alice" } },
    { "op": "update", "before": { "name": "Alice" }, "after": { "name": "Alice Updated" } },
    { "op": "delete", "data": { "name": "Alice Updated" } }
  ]
}
```

Heartbeat message:

```json
{ "type": "heartbeat", "ts": 1714500000000 }
```

## Data Mapping

The frontend performs **live state accumulation** per widget:

- `add` inserts rows
- `update` replaces matching rows
- `delete` removes matching rows
- `aggregation` updates aggregate values

This keeps the server simple: it forwards query diffs directly to clients.

## Integration Test

Protocol-target integration is in:

- `tests/integration_tests.rs`

Run:

```bash
cargo test -p drasi-reaction-dashboard -- --ignored --nocapture
```

The test verifies:

1. Dashboard REST CRUD (`POST`, `GET`, `PUT`, `DELETE`)
2. WebSocket subscription handshake
3. Query change propagation for `INSERT`, `UPDATE`, `DELETE`

## Makefile Targets

```bash
make build
make test
make integration-test
make lint
```

## Limitations

- No authentication in v1 (single-user assumption)
- Client re-accumulates in-memory state after reconnect
- Map widget uses scatter coordinates unless ECharts geo map data is registered
