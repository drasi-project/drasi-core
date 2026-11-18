// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use serde_json::json;

fn compiled(source: &str) -> Templates {
    let mut templates = Templates::new();
    templates
        .compile(source)
        .unwrap_or_else(|error| panic!("{source}: {error}"));
    templates
}

fn render(source: &str, context: Value) -> Result<String, TemplateError> {
    compiled(source).render(source, &context)
}

#[test]
fn known_helpers_compile_in_every_position_and_unknown_helpers_are_refused() {
    for good in [
        "{{json after}}",
        "{{#if after}}{{after.id}}{{else}}none{{/if}}",
        "{{#each after}}{{@key}}={{this}}{{/each}}",
        "{{lookup after \"id\"}} {{#if (eq operation \"ADD\")}}a{{/if}}",
        "{{operation}} {{{raw_value}}}",
        "{{#with after}}{{len name}}{{/with}}",
    ] {
        Templates::new()
            .compile(good)
            .unwrap_or_else(|error| panic!("{good}: {error}"));
    }
    for bad in [
        "{{missing after}}",
        "{{#missing after}}x{{/missing}}",
        "{{#if (missing after)}}x{{/if}}",
        // A static check sees branches a trial render on `{}` would skip.
        "{{#if after}}{{lowercase after.name}}{{/if}}",
        "{{#if after}}x{{else}}{{upper before}}{{/if}}",
        "{{json (missing after)}}",
        "{{eq a b c=(missing d)}}",
        "{{> partial}}",
        "{{*inline}}",
    ] {
        assert!(
            matches!(
                Templates::new().compile(bad),
                Err(TemplateError::Unsupported(_))
            ),
            "{bad} must be refused as unsupported"
        );
    }
    assert!(matches!(
        Templates::new().compile("{{#if}"),
        Err(TemplateError::Syntax(_))
    ));
}

#[test]
fn reading_a_missing_field_is_a_render_error_but_a_condition_may_test_it() {
    let delete = json!({"operation": "DELETE", "before": {"name": "a"}});
    assert!(matches!(
        render("{\"name\":\"{{after.name}}\"}", delete.clone()),
        Err(TemplateError::Render(_))
    ));
    assert!(matches!(
        render("{{json after}}", delete.clone()),
        Err(TemplateError::Render(_))
    ));
    assert_eq!(
        render(
            "{{#if after}}{{after.name}}{{else}}gone:{{before.name}}{{/if}}",
            delete
        ),
        Ok("gone:a".to_owned())
    );
    // A present null is a value, not a missing field.
    assert_eq!(
        render("{{json after.name}}", json!({"after": {"name": null}})),
        Ok("null".to_owned())
    );
}

#[test]
fn values_are_written_unescaped_and_json_quotes_them() {
    let context = json!({"after": {"name": "O'Brien <a&b> \"q\""}});
    assert_eq!(
        render("{{after.name}}", context.clone()),
        Ok("O'Brien <a&b> \"q\"".to_owned())
    );
    assert_eq!(
        render("{{{after.name}}}", context.clone()),
        Ok("O'Brien <a&b> \"q\"".to_owned())
    );
    assert_eq!(
        render("{\"name\":{{json after.name}}}", context),
        Ok("{\"name\":\"O'Brien <a&b> \\\"q\\\"\"}".to_owned())
    );
}

#[test]
fn json_bodies_must_render_json_and_uncompiled_sources_are_refused() {
    let templates = compiled("{\"id\":{{json after.id}}}");
    assert_eq!(
        templates.render_json("{\"id\":{{json after.id}}}", &json!({"after": {"id": "a"}})),
        Ok(json!({"id": "a"}))
    );
    let templates = compiled("{\"id\":{{after.id}}}");
    assert!(matches!(
        templates.render_json("{\"id\":{{after.id}}}", &json!({"after": {"id": "a"}})),
        Err(TemplateError::InvalidJson(_))
    ));
    assert_eq!(
        templates.render("other", &json!({})),
        Err(TemplateError::NotCompiled)
    );
    // Identical sources share one compiled template; empty is never compiled.
    let mut templates = Templates::new();
    templates.compile("{{a}}").expect("compile");
    templates.compile("{{a}}").expect("compile again");
    templates.compile("").expect("empty");
    assert!(templates.contains("{{a}}") && !templates.contains(""));
}

#[test]
fn parameters_bind_in_order_with_each_dialects_placeholder() {
    let hostile = "x'); DROP TABLE users; --";
    let context = json!({"after": {"id": 7, "name": hostile}});
    let source = "CALL add({{param after.name}}, {{param after.id}}, {{param after.name}})";
    for (placeholder, sql) in [
        (Placeholder::Dollar, "CALL add($1, $2, $3)"),
        (Placeholder::Question, "CALL add(?, ?, ?)"),
        (Placeholder::AtP, "CALL add(@P1, @P2, @P3)"),
    ] {
        let template = ParameterTemplate::compile(source, placeholder).expect("compile");
        let rendered = template.render(&context).expect("render");
        assert_eq!(rendered.sql, sql);
        assert_eq!(
            rendered.parameters,
            [json!(hostile), json!(7), json!(hostile)]
        );
        // The buffer starts empty on every render.
        let again = template
            .render(&json!({"after": {"id": 8, "name": "n"}}))
            .expect("render again");
        assert_eq!(again.parameters, [json!("n"), json!(8), json!("n")]);
    }
}

#[test]
fn parameter_templates_bind_objects_and_sub_expressions_and_fail_on_missing_values() {
    let context = json!({"after": {"id": 1, "tags": ["a"]}});
    let whole = ParameterTemplate::compile("CALL ingest({{param after}})", Placeholder::Dollar)
        .expect("compile");
    assert_eq!(
        whole.render(&context).expect("render").parameters,
        [json!({"id": 1, "tags": ["a"]})]
    );
    let text = ParameterTemplate::compile("EXEC p {{param (json after.tags)}}", Placeholder::AtP)
        .expect("compile");
    let rendered = text.render(&context).expect("render");
    assert_eq!(rendered.sql, "EXEC p @P1");
    assert_eq!(rendered.parameters, [json!("[\"a\"]")]);
    for missing in [
        "CALL p({{param after.missing}})",
        "CALL p({{param (json after.missing)}})",
    ] {
        let template = ParameterTemplate::compile(missing, Placeholder::Question).expect("compile");
        assert!(matches!(
            template.render(&context),
            Err(TemplateError::Render(_))
        ));
    }
    let excluded = ParameterTemplate::compile(
        "{{#if after.active}}CALL x({{param after.id}}){{/if}}",
        Placeholder::Question,
    )
    .expect("compile");
    let rendered = excluded.render(&context).expect("render");
    assert!(rendered.sql.trim().is_empty() && rendered.parameters.is_empty());
    let empty = ParameterTemplate::compile("  ", Placeholder::Dollar).expect("compile");
    assert!(empty.is_empty());
}

#[test]
fn only_bound_parameters_and_control_flow_are_accepted() {
    for safe in [
        "",
        "CALL upsert({{param after.id}}, {{param after.name}})",
        "EXEC dbo.Upsert @Id = {{param after.id}}",
        "{{#if after.active}}CALL on({{param after.id}}){{else}}CALL off({{param after.id}}){{/if}}",
        "{{#each after.tags}}CALL tag({{param this}});{{/each}}",
        "{{! a comment }}CALL p({{param (lookup after \"id\")}})",
        "{{#if (eq operation \"ADD\")}}CALL a({{param after.id}}){{/if}}",
    ] {
        validate_parameter_only(safe).unwrap_or_else(|error| panic!("{safe}: {error}"));
    }
    for unsafe_template in [
        "CALL upsert('{{after.name}}')",
        "CALL upsert({{{after.name}}})",
        "CALL upsert({{& after.name}})",
        "EXEC p '{{json after.name}}'",
        "CALL {{after.procedure}}()",
        "{{> partial}}",
        "{{#custom after}}x{{/custom}}",
        "{{#if after.x}}'{{after.name}}'{{/if}}",
        "{{#each after.rows}}{{{this}}}{{/each}}",
        "CALL p({{param}})",
        "CALL p({{param a b}})",
        "CALL p({{param (missing after)}})",
        "{{#if (missing after)}}CALL p(){{/if}}",
        "{{*inline}}",
    ] {
        assert!(
            matches!(
                validate_parameter_only(unsafe_template),
                Err(TemplateError::Unsupported(_))
            ),
            "{unsafe_template} must be rejected"
        );
        assert!(
            ParameterTemplate::compile(unsafe_template, Placeholder::Dollar).is_err(),
            "{unsafe_template} must not compile"
        );
    }
    assert!(matches!(
        validate_parameter_only("{{#if}"),
        Err(TemplateError::Syntax(_))
    ));
}
