// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! JSON templates with `$` selectors, used to build the Rego `input` from an
//! incoming element and the output properties from a decision.
//!
//! A template is any JSON value. A string that starts with `$` selects from
//! the evaluation document: `$` is the whole document, `$root.a.0.b` walks
//! object keys and array indices. `$$` escapes a literal leading `$`. A
//! selector that does not resolve omits its object member, becomes `null` in
//! an array and leaves a top-level template unresolved.

use anyhow::{bail, ensure, Result};
use serde_json::{Map, Value};

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Template {
    Literal(Value),
    Select(Vec<String>),
    Object(Vec<(String, Template)>),
    Array(Vec<Template>),
}

impl Template {
    /// Compiles `value`, accepting only selectors rooted at one of `roots`
    /// (and the whole-document selector `$` when `whole` is true).
    pub(crate) fn compile(value: &Value, roots: &[&str], whole: bool) -> Result<Self> {
        Ok(match value {
            Value::String(text) => match text.strip_prefix('$') {
                Some(rest) if rest.starts_with('$') => Self::Literal(Value::String(rest.into())),
                Some("") => {
                    ensure!(
                        whole,
                        "the whole-document selector `$` is not available here"
                    );
                    Self::Select(Vec::new())
                }
                Some(path) => {
                    let segments: Vec<String> = path.split('.').map(str::to_owned).collect();
                    if segments.iter().any(String::is_empty) {
                        bail!("selector `{text}` has an empty path segment");
                    }
                    ensure!(
                        roots.contains(&segments[0].as_str()),
                        "selector `{text}` must start with one of: {}",
                        roots
                            .iter()
                            .map(|root| format!("${root}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                    Self::Select(segments)
                }
                None => Self::Literal(value.clone()),
            },
            Value::Object(members) => {
                let compiled = members
                    .iter()
                    .map(|(key, value)| Ok((key.clone(), Self::compile(value, roots, whole)?)))
                    .collect::<Result<Vec<_>>>()?;
                if compiled
                    .iter()
                    .all(|(_, template)| matches!(template, Self::Literal(_)))
                {
                    Self::Literal(value.clone())
                } else {
                    Self::Object(compiled)
                }
            }
            Value::Array(items) => {
                let compiled = items
                    .iter()
                    .map(|item| Self::compile(item, roots, whole))
                    .collect::<Result<Vec<_>>>()?;
                if compiled
                    .iter()
                    .all(|template| matches!(template, Self::Literal(_)))
                {
                    Self::Literal(value.clone())
                } else {
                    Self::Array(compiled)
                }
            }
            scalar => Self::Literal(scalar.clone()),
        })
    }

    /// Resolves the template against `document`; `None` when a top-level
    /// selector does not resolve.
    pub(crate) fn resolve(&self, document: &Value) -> Option<Value> {
        match self {
            Self::Literal(value) => Some(value.clone()),
            Self::Select(path) => select(document, path).cloned(),
            Self::Object(members) => Some(Value::Object(
                members
                    .iter()
                    .filter_map(|(key, template)| {
                        template.resolve(document).map(|value| (key.clone(), value))
                    })
                    .collect::<Map<_, _>>(),
            )),
            Self::Array(items) => Some(Value::Array(
                items
                    .iter()
                    .map(|item| item.resolve(document).unwrap_or(Value::Null))
                    .collect(),
            )),
        }
    }
}

fn select<'a>(document: &'a Value, path: &[String]) -> Option<&'a Value> {
    path.iter()
        .try_fold(document, |current, segment| match current {
            Value::Object(members) => members.get(segment),
            Value::Array(items) => segment
                .parse::<usize>()
                .ok()
                .and_then(|index| items.get(index)),
            _ => None,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ROOTS: &[&str] = &["properties", "labels"];

    #[test]
    fn selectors_resolve_objects_arrays_and_escapes() -> Result<()> {
        let template = Template::compile(
            &json!({
                "a": "$properties.a",
                "missing": "$properties.none",
                "first": "$labels.0",
                "list": ["$properties.none", "$properties.nested.x", 3],
                "literal": "$$properties.a",
                "plain": "text",
                "fixed": {"k": [1, 2]}
            }),
            ROOTS,
            true,
        )?;
        let document = json!({
            "properties": {"a": 1, "nested": {"x": "y"}},
            "labels": ["L"]
        });
        assert_eq!(
            template.resolve(&document),
            Some(json!({
                "a": 1,
                "first": "L",
                "list": [null, "y", 3],
                "literal": "$properties.a",
                "plain": "text",
                "fixed": {"k": [1, 2]}
            }))
        );
        assert_eq!(
            Template::compile(&json!("$"), ROOTS, true)?.resolve(&document),
            Some(document.clone())
        );
        assert_eq!(
            Template::compile(&json!("$properties.none"), ROOTS, true)?.resolve(&document),
            None
        );
        Ok(())
    }

    #[test]
    fn invalid_selectors_are_rejected() {
        assert!(Template::compile(&json!("$other"), ROOTS, true).is_err());
        assert!(Template::compile(&json!({"x": ["$properties..a"]}), ROOTS, true).is_err());
        assert!(Template::compile(&json!("$"), ROOTS, false).is_err());
    }
}
