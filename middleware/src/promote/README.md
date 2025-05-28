# Promote Middleware

## Overview

The **promote** middleware processes `SourceChange` events (`Insert` and `Update`) to copy values from deep‑nested locations inside an element’s `properties` map to new **top‑level** properties.  
Selection is performed with JSONPath expressions; each promoted value is written under an explicit `target_name`.

## Functionality

1. **Identify Target Changes**  
   Only `Insert` and `Update` changes are processed. `Delete` and `Future` changes pass through unchanged.

2. **Prepare Data**  
   The element’s `ElementPropertyMap` is converted to a `serde_json::Value` so that JSONPath can be evaluated.

3. **Iterate Mappings**  
   For each mapping in `mappings`:
   - Evaluate `path` (JSONPath) and **require exactly one match**.  
     - `0` or `>1` results are treated as errors governed by `on_error`.
   - Convert the selected `serde_json::Value` to an `ElementValue`.
   - Check for name conflicts on `target_name`; resolve per `on_conflict`.
   - Insert (or overwrite) the property when allowed.

4. **Error Handling**  
   Errors (invalid path, wrong type, conflict with `fail`, etc.) are handled according to `on_error`.

## Configuration Options

| Field         | Type & Allowed Values                              | Required | Default       | Description                                                                                |
|---------------|----------------------------------------------------|----------|---------------|--------------------------------------------------------------------------------------------|
| `mappings`    | **Array** of *Mapping* objects                     | **Yes**  | –             | Promotion rules (must contain at least one entry).                                         |
| `on_conflict` | `"overwrite" \| "skip" \| "fail"`                  | No       | `"overwrite"` | What to do if `target_name` already exists in the top‑level properties.                    |
| `on_error`    | `"skip" \| "fail"`                                 | No       | `"fail"`      | Behaviour when a mapping errors (path selects 0 or >1 items, conversion fails, etc.).       |

### Mapping Object

| Field         | Type   | Required | Description                                                   |
|---------------|--------|----------|---------------------------------------------------------------|
| `path`        | String | **Yes**  | JSONPath expression that selects exactly one value.           |
| `target_name` | String | **Yes**  | Name of the new top‑level property that will receive the value.|

## Example Configuration

```json
{
  "name": "promote_user_and_order_data",
  "kind": "promote",
  "config": {
    "mappings": [
      { "path": "$.user.id",              "target_name": "userId"     },
      { "path": "$.user.location.city",   "target_name": "city"       },
      { "path": "$.order.total",          "target_name": "orderTotal" },
      { "path": "$.metadata",             "target_name": "meta"       }
    ],
    "on_conflict": "skip",
    "on_error": "skip"
  }
}
```

When using the middleware as part of the query spec, we can use the middleware like shown below:

```yaml
# spec.sources.middleware
- name: promote_user_and_order_data
  kind: promote
  config:
    mappings:
      - path: "$.user.id"
        target_name: "userId"
      - path: "$.user.location.city"
        target_name: "city"
      - path: "$.order.total"
        target_name: "orderTotal"
      - path: "$.metadata"
        target_name: "meta"
    on_conflict: skip   # keep existing values on conflicts
    on_error:    skip   # skip mappings that error
```

## Transformation Examples

### 1. Basic Promotion

- **Config**
```json
{
  "mappings": [
    { "path": "$.device.id",           "target_name": "deviceId"   },
    { "path": "$.payload.temperature", "target_name": "temperature"}
  ]
}
```

This can be specified in the query YAML like:
```yaml
# spec.sources.middleware.config
mappings:
  - path: "$.device.id"
    target_name: "deviceId"
  - path: "$.payload.temperature"
    target_name: "temperature"
```

- **Input**
```rust
SourceChange::Insert {
    element: Element::Node {
        metadata: /* ... */,
        properties: {
            "device": ElementValue::Object(
                json!({"id": "sensor-A1", "type": "temp"}).into()
            ),
            "payload": ElementValue::Object(
                json!({"temperature": 25.5}).into()
            )
        }.into(),
    },
}
```

- **Output**
```rust
SourceChange::Insert {
    element: Element::Node {
        metadata: /* ... */,
        properties: {
            "device": ElementValue::Object(/* unchanged */),
            "payload": ElementValue::Object(/* unchanged */),
            "deviceId": ElementValue::String("sensor-A1".into()),
            "temperature": ElementValue::Float(OrderedFloat(25.5))
        }.into(),
    },
}
```

---

### 2. Conflict Handling (`skip`)

- **Config**
```json
{
  "mappings": [
    { "path": "$.config.priority", "target_name": "level"     },
    { "path": "$.status.code",     "target_name": "statusCode"}
  ],
  "on_conflict": "skip"
}
```

This can be specified in the query YAML like:
```yaml
# spec.sources.middleware.config
mappings:
  - path: "$.config.priority"
    target_name: "level"
  - path: "$.status.code"
    target_name: "statusCode"
on_conflict: skip
```

- **Input**
```rust
SourceChange::Update {
    element: Element::Node {
        metadata: /* ... */,
        properties: {
            "level": ElementValue::String("INFO".into()),
            "config": ElementValue::Object(json!({"priority": "HIGH"}).into()),
            "status": ElementValue::Object(json!({"code": 200}).into())
        }.into(),
    },
}
```

- **Output**  
`level` is kept unchanged due to `skip`; `statusCode` is promoted.
```rust
SourceChange::Update {
    element: Element::Node {
        metadata: /* ... */,
        properties: {
            "level": ElementValue::String("INFO".into()),
            "config": ElementValue::Object(/* unchanged */),
            "status": ElementValue::Object(/* unchanged */),
            "statusCode": ElementValue::Integer(200)
        }.into(),
    },
}
```

---

### 3. Error Handling (`skip`)

- **Config**
```json
{
  "mappings": [
    { "path": "$.user.name",        "target_name": "userName" },
    { "path": "$.nonexistent.value","target_name": "someValue"}
  ],
  "on_error": "skip"
}
```

- **Input**
```rust
SourceChange::Insert {
    element: Element::Node {
        metadata: /* ... */,
        properties: {
            "user": ElementValue::Object(json!({"name": "Bob", "id": 789}).into())
        }.into(),
    },
}
```

- **Output**  
Second mapping is skipped because the path matches nothing.
```rust
SourceChange::Insert {
    element: Element::Node {
        metadata: /* ... */,
        properties: {
            "user": ElementValue::Object(/* unchanged */),
            "userName": ElementValue::String("Bob".into())
        }.into(),
    },
}
```

---

### 4. Promoting an Object

- **Config**
```json
{ "mappings": [ { "path": "$.event.details", "target_name": "eventDetails" } ] }
```

- **Input**
```rust
SourceChange::Insert {
    element: Element::Node {
        metadata: /* ... */,
        properties: {
            "eventId": ElementValue::String("evt-001".into()),
            "event": ElementValue::Object(json!({
                "type": "LOGIN",
                "details": { "ip": "192.168.1.100", "success": true }
            }).into())
        }.into(),
    },
}
```

- **Output**
```rust
SourceChange::Insert {
    element: Element::Node {
        metadata: /* ... */,
        properties: {
            "eventId": ElementValue::String("evt-001".into()),
            "event": ElementValue::Object(/* unchanged */),
            "eventDetails": ElementValue::Object(
                json!({ "ip": "192.168.1.100", "success": true }).into()
            )
        }.into(),
    },
}
```