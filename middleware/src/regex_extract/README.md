# RegexExtract Middleware

`regex_extract` searches a string property with a regex compiled during
middleware setup and writes one named or numbered capture as an
`ElementValue::String`. Insert and Update metadata and element types are
preserved. Delete and Future changes always pass through unchanged.

## Configuration

| Field | Type | Required | Default | Description |
|---|---|---:|---|---|
| `target_property` | string | yes | | Property to search |
| `pattern` | string | yes | | Rust `regex` syntax, compiled at setup |
| `capture_group` | string or non-negative integer | yes | | Existing named or numbered group; `0` selects the full match |
| `output_property` | string | yes | | Property receiving the captured string |
| `max_capture_size` | integer | no | `1048576` | Maximum captured bytes |
| `on_missing` | `passthrough`, `drop`, or `fail` | no | `passthrough` | Missing target policy |
| `on_no_match` | `passthrough`, `drop`, or `fail` | no | `passthrough` | No-match policy |
| `on_error` | `passthrough`, `drop`, or `fail` | no | `fail` | Wrong type, nonparticipating capture, or oversize capture policy |
| `on_collision` | `overwrite`, `passthrough`, `drop`, or `fail` | no | `fail` | Existing distinct output property policy |

```json
{
  "target_property": "message",
  "pattern": "(?s)^Envelope/v1\\n```json\\s*(?<document>.*?)\\s*```",
  "capture_group": "document",
  "output_property": "document_json",
  "max_capture_size": 65536,
  "on_missing": "passthrough",
  "on_no_match": "passthrough",
  "on_error": "fail",
  "on_collision": "fail"
}
```

The defaults make the middleware safe in a source-wide pipeline: unrelated
elements and nonmatching strings pass through, while malformed matching input
does not silently succeed. If `output_property` equals `target_property`, the
captured value replaces the input without being treated as a collision.
