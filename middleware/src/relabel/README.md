# Relabel Middleware

## Overview

The **relabel** middleware processes `SourceChange` events (`Insert`, `Update`, and `Delete`) to transform element labels according to user-defined mapping rules. This middleware enables dynamic label renaming and normalization of incoming data, allowing source labels to be mapped to target labels that better fit the query context or domain model.

## Functionality

1. **Identify Target Changes**  
   All `SourceChange` types (`Insert`, `Update`, and `Delete`) are processed. `Future` changes pass through unchanged.

2. **Extract Element Labels**  
   The middleware extracts the current labels from the element metadata in the `SourceChange`.

3. **Apply Label Mappings**  
   For each label in the element:
   - Check if the label exists in the configured `labelMappings`.
   - If a mapping exists, replace the label with the target label.
   - If no mapping exists, keep the original label unchanged.

4. **Update Element Metadata**  
   Create a new element with the transformed labels while preserving all other element properties, metadata, and relationships.

5. **Error Handling**
   Invalid configurations (empty `labelMappings` or malformed JSON) will cause middleware creation to fail.

## Configuration Options

| Field          | Type                          | Required | Default  | Description                                                                                      |
|----------------|-------------------------------|----------|----------|--------------------------------------------------------------------------------------------------|
| `labelMappings`| **Object** (String → String) | **Yes**  | –        | Key-value pairs mapping source labels to target labels. Must contain at least one mapping.     |

## Example Configuration

```json
{
  "name": "normalize_user_labels",
  "kind": "relabel",
  "labelMappings": {
    "Person": "User",
    "Employee": "Staff",
    "Company": "Organization"
  }
}
```

When using the middleware as part of the query spec, we can use the middleware like shown below:

```yaml
# spec.sources.middleware
- name: normalize_user_labels
  kind: relabel
  labelMappings:
    Person: User
    Employee: Staff
    Company: Organization
```

## Transformation Examples

### 1. Basic Label Remapping

- **Config**
```json
{
  "labelMappings": {
    "Person": "User",
    "Employee": "Staff"
  }
}
```

- **Input**
```rust
SourceChange::Insert {
    element: Element::Node {
        metadata: ElementMetadata {
            labels: vec!["Person".into()].into(),
            /* ... */
        },
        properties: { "name": ElementValue::String("John".into()) }.into(),
    },
}
```

- **Output**
```rust
SourceChange::Insert {
    element: Element::Node {
        metadata: ElementMetadata {
            labels: vec!["User".into()].into(),  // Person → User
            /* ... */
        },
        properties: { "name": ElementValue::String("John".into()) }.into(),
    },
}
```

---

### 2. Multiple Labels and Unmapped Labels

- **Config**
```json
{
  "labelMappings": {
    "Person": "User",
    "Employee": "Staff"
  }
}
```

- **Input**
```rust
SourceChange::Update {
    element: Element::Node {
        metadata: ElementMetadata {
            labels: vec!["Person".into(), "Manager".into()].into(),
            /* ... */
        },
        /* ... */
    },
}
```

- **Output**  
```rust
SourceChange::Update {
    element: Element::Node {
        metadata: ElementMetadata {
            labels: vec!["User".into(), "Manager".into()].into(),  // Person → User, Manager unchanged
            /* ... */
        },
        /* ... */
    },
}
```

---

### 3. Relationship Label Remapping

- **Config**
```json
{
  "labelMappings": {
    "WORKS_FOR": "EMPLOYED_BY"
  }
}
```

- **Input**
```rust
SourceChange::Insert {
    element: Element::Relation {
        metadata: ElementMetadata {
            labels: vec!["WORKS_FOR".into()].into(),
            /* ... */
        },
        /* ... */
        in_node: ElementReference::new("test", "person1"),
        out_node: ElementReference::new("test", "company1"),
    },
}
```

- **Output**
```rust
SourceChange::Insert {
    element: Element::Relation {
        metadata: ElementMetadata {
            labels: vec!["EMPLOYED_BY".into()].into(),  // WORKS_FOR → EMPLOYED_BY
            /* ... */
        },
        /* ... */
    },
}
```