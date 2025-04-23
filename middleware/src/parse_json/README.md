# ParseJson Middleware

## Overview

The `parse_json` middleware processes `SourceChange` events (specifically `Insert` and `Update`) to parse a string property containing a JSON document into a structured `ElementValue` (Object or List). It allows configuration for handling errors and specifying where the parsed result should be stored.

## Functionality

1.  **Identify Target:** For each incoming `Insert` or `Update` change, the middleware looks for the property specified in `target_property`.
2.  **Validate Type:** It checks if the value of the `target_property` is a string (`ElementValue::String`). If not, it either skips the element or fails, based on the `on_error` setting.
3.  **Parse JSON:** It attempts to parse the string value as a JSON document using `serde_json`.
4.  **Convert Value:** If parsing is successful, it converts the resulting `serde_json::Value` into the corresponding `ElementValue` variant (e.g., `ElementValue::Object`, `ElementValue::List`, `ElementValue::Integer`, etc.).
5.  **Handle Errors:** If parsing or conversion fails (e.g., invalid JSON syntax, unsupported JSON structure), it either skips the element (keeping the original string value) or fails the processing entirely, based on the `on_error` setting.
6.  **Store Result:** If parsing and conversion are successful, the resulting `ElementValue` is stored:
    *   In the property specified by `output_property`, if provided.
    *   Otherwise, it overwrites the original `target_property`.
    *   If `output_property` is specified and already exists, it will be overwritten (a warning will be logged).
7.  **Pass Through:** `Delete` and `Future` changes are passed through without modification.

## Configuration Options

The middleware is configured using a JSON object with the following fields:

| Field             | Type                          | Required | Default | Description                                                                                                                               |
| :---------------- | :---------------------------- | :------- | :------ | :---------------------------------------------------------------------------------------------------------------------------------------- |
| `target_property` | String                        | Yes      |         | The name of the element property containing the JSON string to be parsed.                                                                 |
| `output_property` | String                        | No       | `null`  | Optional. The name of the property where the parsed `ElementValue` should be stored. If omitted or `null`, `target_property` will be overwritten. |
| `on_error`        | String (`"skip"` or `"fail"`) | No       | `"fail"`| Defines behavior when an error occurs (missing target, wrong type, parsing/conversion failure): `"skip"` logs a warning and skips the element; `"fail"` stops processing and returns an error. |

## Example Configuration

```json
{
  "name": "parse_event_data",
  "kind": "parse_json",
  "config": {
    "target_property": "raw_event_json",
    "output_property": "event_details",
    "on_error": "skip"
  }
}
```

This configuration will:

- Look for the raw_event_json property.
- If it's a string, attempt to parse it as JSON.
- If successful, convert the parsed JSON to an ElementValue and store it in the event_details property.
- If any step fails (not a string, invalid JSON), log a warning and leave the element unchanged.

## Transformation Examples

### Example 1: Parse JSON Object (Overwrite)

- Configuration
```json
{
  "target_property": "json_data"
}
```

- Input:
```rust
SourceChange::Insert {
    element: Element::Node {
        metadata: /* ... */,
        properties: {
            "id": ElementValue::Integer(101),
            "json_data": ElementValue::String(r#"{"user": "alice", "active": true, "score": 95.5}"#.into())
        }.into(),
    },
}
```

Output:
```rust
SourceChange::Insert {
    element: Element::Node {
        metadata: /* ... */,
        properties: {
            "id": ElementValue::Integer(101),
            // "json_data" is overwritten with the parsed object
            "json_data": ElementValue::Object(
                json!({"user": "alice", "active": true, "score": 95.5}).into()
            )
        }.into(),
    },
}
```

### Example 2: Parse JSON Array with Output Property

- Configuration
```json
{
  "target_property": "tags_json",
  "output_property": "tag_list"
}
```

- Input:
```rust
SourceChange::Update {
    element: Element::Node {
        metadata: /* ... */,
        properties: {
            "name": ElementValue::String("config_node".into()),
            "tags_json": ElementValue::String(r#"["alpha", "beta", "gamma"]"#.into())
        }.into(),
    },
}
```

Output:
```rust
SourceChange::Update {
    element: Element::Node {
        metadata: /* ... */,
        properties: {
            "name": ElementValue::String("config_node".into()),
            // Original string remains
            "tags_json": ElementValue::String(r#"["alpha", "beta", "gamma"]"#.into()),
            // Parsed list added to "tag_list"
            "tag_list": ElementValue::List(vec![
                ElementValue::String("alpha".into()),
                ElementValue::String("beta".into()),
                ElementValue::String("gamma".into()),
            ])
        }.into(),
    },
}
```

### Example 3: Invalid JSON with Skip

- Configuration
```json
{
  "target_property": "bad_json",
  "on_error": "skip"
}
```

- Input:
```rust
SourceChange::Insert {
    element: Element::Node {
        metadata: /* ... */,
        properties: {
            "id": ElementValue::Integer(202),
            // Note the missing closing brace
            "bad_json": ElementValue::String(r#"{"key": "value" "#.into())
        }.into(),
    },
}
```

Output:
```rust
SourceChange::Insert {
    element: Element::Node {
        metadata: /* ... */,
        // Properties remain unchanged due to parsing error and on_error: skip
        properties: {
            "id": ElementValue::Integer(202),
            "bad_json": ElementValue::String(r#"{"key": "value" "#.into())
        }.into(),
    },
}
```

(Explanation: The JSON string is invalid. Since on_error is skip, a warning is logged, and the original SourceChange is passed through unmodified.)