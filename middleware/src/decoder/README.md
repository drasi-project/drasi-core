# Decoder Middleware

## Overview

The `decoder` middleware processes `SourceChange` events (specifically `Insert` and `Update`) to decode a string value found in a specified property of an `Element`. It supports various common encoding formats and allows configuration for handling errors and specifying the output property.

## Functionality

1.  **Identify Target:** For each incoming `Insert` or `Update` change, the middleware looks for the property specified in `target_property`.
2.  **Validate Type:** It checks if the value of the `target_property` is a string (`ElementValue::String`). If not, it either skips the element or fails, based on the `on_error` setting.
3.  **Strip Quotes (Optional):** If `strip_quotes` is set to `true`, it removes leading and trailing double quotes (`"`) from the string value before decoding.
4.  **Decode:** It decodes the (potentially quote-stripped) string using the method specified by `encoding_type`.
5.  **Handle Errors:** If decoding fails (e.g., invalid input format), it either skips the element (keeping the original value) or fails the processing entirely, based on the `on_error` setting.
6.  **Store Result:** If decoding is successful, the resulting decoded string is stored:
    *   In the property specified by `output_property`, if provided.
    *   Otherwise, it overwrites the original `target_property`.
    *   If `output_property` is specified and already exists, it will be overwritten (a warning will be logged).
7.  **Pass Through:** `Delete` and `Future` changes are passed through without modification.

## Configuration Options

The middleware is configured using a JSON object with the following fields:

| Field             | Type                                     | Required | Default | Description                                                                                                                               |
| :---------------- | :--------------------------------------- | :------- | :------ | :---------------------------------------------------------------------------------------------------------------------------------------- |
| `encoding_type`   | String                                   | Yes      |         | The encoding format of the `target_property` value. Must be one of: `base64`, `base64url`, `hex`, `url`, `json_escape`.                   |
| `target_property` | String                                   | Yes      |         | The name of the element property containing the encoded string to be decoded.                                                             |
| `output_property` | String                                   | No       | `null`  | Optional. The name of the property where the decoded string should be stored. If omitted or `null`, `target_property` will be overwritten. |
| `strip_quotes`    | Boolean                                  | No       | `false` | If `true`, removes surrounding double quotes (`"`) from the `target_property` value *before* decoding.                                    |
| `on_error`        | String (`"skip"` or `"fail"`)            | No       | `"fail"`| Defines behavior when an error occurs (missing target, wrong type, decoding failure): `"skip"` logs a warning and skips the element; `"fail"` stops processing and returns an error. |

## Encoding Types

*   **`base64`**: Standard Base64 encoding (RFC 4648).
*   **`base64url`**: URL-safe Base64 encoding (RFC 4648 §5), without padding.
*   **`hex`**: Hexadecimal encoding (e.g., `48656c6c6f`).
*   **`url`**: Percent-encoding (e.g., `Hello%20World`).
*   **`json_escape`**: Decodes JSON string escape sequences (e.g., `\"`, `\\`, `\n`). Assumes the input is the *content* of a JSON string literal.

## Example Configuration

```json
{
  "name": "decode_user_data",
  "kind": "decoder",
  "config": {
    "encoding_type": "base64",
    "target_property": "raw_user_payload",
    "output_property": "user_data",
    "strip_quotes": true,
    "on_error": "skip"
  }
}
```

This configuration will:

- Look for the raw_user_payload property.
- If it's a string, remove surrounding quotes.
- Decode the result using Base64.
- Store the decoded string in the user_data property.
- If any step fails, log a warning and leave the element unchanged.

## Transformation Examples

### Example 1: Basic Base64 Decode (Overwrite)

- Configuration
```json
{
  "encoding_type": "base64",
  "target_property": "data"
}
```

- Input:
```rust
SourceChange::Insert {
    element: Element::Node {
        metadata: /* ... */,
        properties: { "data": ElementValue::String("SGVsbG8gV29ybGQh".into()) }.into(),
    },
}
```

Output:
```rust
SourceChange::Insert {
    element: Element::Node {
        metadata: /* ... */,
        properties: { "data": ElementValue::String("Hello World!".into()) }.into(), // "data" is overwritten
    },
}
```

### Example 2: Hex Decode with Output Property and Strip Quotes

- Configuration
```json
{
  "encoding_type": "hex",
  "target_property": "raw",
  "output_property": "decoded",
  "strip_quotes": true
}
```

- Input:
```rust
SourceChange::Update {
    element: Element::Node {
        metadata: /* ... */,
        // Note the escaped quotes within the string value
        properties: { "raw": ElementValue::String("\"4865782044617461\"".into()) }.into(),
    },
}
```

Output:
```rust
SourceChange::Update {
    element: Element::Node {
        metadata: /* ... */,
        properties: {
            "raw": ElementValue::String("\"4865782044617461\"".into()), // Original remains
            "decoded": ElementValue::String("Hex Data".into())         // Decoded value added
        }.into(),
    },
}
```

Explanation: Quotes are stripped from "4865782044617461" -> 4865782044617461, which is then hex-decoded to Hex Data and stored in decoded.

### Example 3: URL Decode Failure with Skip

- Configuration
```json
{
  "encoding_type": "url",
  "target_property": "url_encoded",
  "on_error": "skip"
}
```

- Input:
```rust
SourceChange::Insert {
    element: Element::Node {
        metadata: /* ... */,
        properties: { "url_encoded": ElementValue::String("Invalid%URL%Encoding".into()) }.into(),
    },
}
```

Output:
```rust
SourceChange::Insert {
    element: Element::Node {
        metadata: /* ... */,
        // Properties remain unchanged due to decoding error and on_error: skip
        properties: { "url_encoded": ElementValue::String("Invalid%URL%Encoding".into()) }.into(),
    },
}
```

Explanation: The URL decoding fails because %UR and %En are invalid percent-encodings. Since on_error is skip, a warning is logged, and the original SourceChange is passed through unmodified.