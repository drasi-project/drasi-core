// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! SMS-01: native templates are parameter-only. Every way of writing row
//! data into the SQL text is rejected at construction, in routes and in the
//! default template.

use super::*;
use crate::MsSqlStoredProcConfig;

const UNSAFE: &[&str] = &[
    // Plan finding 1: inline `{{json}}` inside a SQL literal.
    "EXEC p '{{json after.name}}'",
    "EXEC p {{json after}}",
    // Raw triple-stash and its `&` form.
    "EXEC p {{{after.name}}}",
    "EXEC p {{& after.name}}",
    // Plain interpolation in a literal, as an argument and as structure.
    "EXEC p '{{after.name}}'",
    "EXEC p {{after.id}}",
    "EXEC {{after.procedure}} 1",
    "EXEC p {{lookup after \"id\"}}",
    // Unsafe output nested inside allowed control flow.
    "{{#if after.x}}EXEC p '{{after.name}}'{{/if}}",
    "{{#each after.rows}}EXEC p {{{this}}};{{/each}}",
    "{{#with after}}EXEC p '{{json name}}'{{/with}}",
    "{{#unless after.x}}EXEC p 1{{else}}EXEC p {{after.id}}{{/unless}}",
    // Partials, decorators and non-control block helpers.
    "{{> partial}}",
    "{{#> layout}}EXEC p 1{{/layout}}",
    "{{*inline}}",
    "{{#*inline \"x\"}}EXEC p 1{{/inline}}",
    "{{#json after}}EXEC p 1{{/json}}",
    "{{#param after.id}}x{{/param}}",
    // `param` with nothing to bind.
    "EXEC p {{param}}",
];

const SAFE: &[&str] = &[
    "EXEC dbo.p {{param after.id}}, {{param after.name}}",
    "EXEC dbo.p @Id = {{param after.id}}, @Doc = {{param (json after)}}",
    "{{#if after.active}}EXEC dbo.on {{param after.id}}{{else}}EXEC dbo.off {{param after.id}}{{/if}}",
    "{{#each after.tags}}EXEC dbo.tag {{param this}};{{/each}}",
    "{{! a comment }}EXEC dbo.p {{param (lookup after \"id\")}}",
    "EXEC dbo.p {{param operation}}, {{param deliveryId}}",
];

fn with_default(template: &str) -> Value {
    let mut configuration = config();
    configuration["defaultTemplate"] = json!({"added": {"template": template}});
    configuration
}

fn with_route(template: &str) -> Value {
    let mut configuration = config();
    configuration["routes"] = json!({"q": {"deleted": {"template": template}}});
    configuration
}

#[test]
fn unsafe_templates_are_rejected_at_construction() {
    for template in UNSAFE {
        for configuration in [with_default(template), with_route(template)] {
            let error = MsSqlStoredProcConfig::parse(configuration.clone())
                .err()
                .unwrap_or_else(|| panic!("{template} must be rejected"));
            assert!(
                !format!("{error:#}").is_empty(),
                "{template} must explain the rejection"
            );
            assert!(sink(configuration).is_err(), "{template}");
        }
    }
}

#[test]
fn parameter_only_templates_are_accepted() -> Result<()> {
    for template in SAFE {
        sink(with_default(template))?;
        sink(with_route(template))?;
    }
    Ok(())
}

#[test]
fn a_rejection_names_the_template_location_and_the_rule() {
    let error = MsSqlStoredProcConfig::parse(with_route("EXEC p '{{json after.name}}'"))
        .expect_err("unsafe");
    let message = format!("{error:#}");
    assert!(message.contains("routes.q.deleted"), "{message}");
    assert!(message.contains("param"), "{message}");
}

#[test]
fn hostile_values_are_bound_never_interpolated() -> Result<()> {
    let mut sink = sink(with_default(
        "EXEC dbo.p {{param after.name}}, {{param (json after)}}",
    ))?;
    let hostile = "O'Brien'); DROP TABLE dbo.Effects; --";
    let batch = batch(
        1,
        &[insert(
            "r1",
            &[(
                "name",
                drasi_core_types::models::ElementValue::String(hostile.into()),
            )],
        )],
    )?;
    let command = sink
        .render_item(&batch.item(0)?)?
        .expect("rendered command");
    assert_eq!(command.sql, "EXEC dbo.p @P1, @P2");
    assert!(!command.sql.contains("DROP"));
    assert_eq!(command.parameters[0], json!(hostile));
    assert!(command.parameters[1]
        .as_str()
        .is_some_and(|document| document.contains("DROP TABLE")));
    Ok(())
}
