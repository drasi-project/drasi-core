// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Compilation and bounded, deterministic evaluation of the configured Rego.

use anyhow::{anyhow, bail, Context, Result};
use regorus::{
    utils::limits::ExecutionTimerConfig, Engine, PolicyLengthConfig, Rc as RegoRc, Value as Rego,
};
use serde_json::{json, Value};
use std::{
    num::{NonZeroU32, NonZeroUsize},
    time::Duration,
};

use crate::{
    config::{PolicyConfig, RegoVersion},
    digest::{canonical, sha256_hex},
};

/// Builtins whose results depend on the clock, randomness, the network or the
/// host. Extensions take precedence over builtins, so each is replaced by one
/// that fails evaluation whatever features the final build unifies.
const NONDETERMINISTIC: &[(&str, u8)] = &[
    ("time.now_ns", 0),
    ("rand.intn", 2),
    ("uuid.rfc4122", 1),
    ("http.send", 1),
    ("opa.runtime", 0),
    ("net.lookup_ip_addr", 1),
];

/// The result of evaluating the query for one input.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Outcome {
    Decided(Value),
    Undefined,
    Error(String),
}

impl Outcome {
    pub(crate) fn status(&self) -> &'static str {
        match self {
            Self::Decided(_) => "decided",
            Self::Undefined => "undefined",
            Self::Error(_) => "error",
        }
    }
}

/// A compiled, prepared engine that every evaluation clones.
#[derive(Clone)]
pub(crate) struct CompiledPolicy {
    engine: Engine,
    query: String,
    hash: String,
    max_input_bytes: u64,
    max_decision_bytes: u64,
}

impl CompiledPolicy {
    /// Parses every module, loads the data document, installs the limits and
    /// compiles the query as the entrypoint so invalid Rego, a missing rule or
    /// a non-rule query fail here rather than per record.
    pub(crate) fn compile(config: &PolicyConfig) -> Result<Self> {
        let data = canonical(&Value::Object(config.data.clone()));
        anyhow::ensure!(
            data.len() as u64 <= config.max_data_bytes,
            "policy data is {} bytes, above max_data_bytes {}",
            data.len(),
            config.max_data_bytes
        );
        let mut engine = Engine::new();
        engine.set_rego_v0(config.rego_version == RegoVersion::V0);
        engine.set_strict_builtin_errors(config.strict_builtin_errors);
        engine.set_gather_prints(true);
        engine.set_execution_timer_config(ExecutionTimerConfig {
            limit: Duration::from_millis(config.timeout_ms),
            check_interval: NonZeroU32::new(config.timeout_check_interval)
                .context("timeout_check_interval must be positive")?,
        });
        let max_file_bytes = usize::try_from(config.max_policy_bytes)
            .ok()
            .and_then(NonZeroUsize::new)
            .context("max_policy_bytes must be positive")?;
        engine.set_policy_length_config(PolicyLengthConfig {
            max_col: NonZeroU32::new(1 << 20).expect("constant"),
            max_file_bytes,
            max_lines: NonZeroUsize::new(1 << 20).expect("constant"),
        });
        for (path, nargs) in NONDETERMINISTIC {
            let name = *path;
            engine.add_extension(
                name.into(),
                *nargs,
                Box::new(move |_: Vec<Rego>| -> Result<Rego> {
                    bail!("`{name}` is disabled: policy evaluation must be deterministic")
                }),
            )?;
        }
        for module in &config.modules {
            engine
                .add_policy(module.name.clone(), module.source.clone())
                .with_context(|| format!("invalid Rego module `{}`", module.name))?;
        }
        engine
            .add_data(Rego::from_json_str(&data).context("invalid policy data")?)
            .context("invalid policy data")?;
        engine
            .compile_with_entrypoint(&RegoRc::<str>::from(config.query.as_str()))
            .with_context(|| format!("invalid policy query `{}`", config.query))?;
        let hash = sha256_hex(
            canonical(&json!({
                "rego_version": config.rego_version,
                "modules": config.modules,
                "query": config.query,
                "data": config.data,
            }))
            .as_bytes(),
        );
        Ok(Self {
            engine,
            query: config.query.clone(),
            hash,
            max_input_bytes: config.max_input_bytes,
            max_decision_bytes: config.max_decision_bytes,
        })
    }

    /// Stable digest of the language version, modules, query and data.
    pub(crate) fn hash(&self) -> &str {
        &self.hash
    }

    /// Evaluates the query for one canonical JSON input.
    pub(crate) fn evaluate(&self, input: &str) -> Outcome {
        if input.len() as u64 > self.max_input_bytes {
            return Outcome::Error(format!(
                "policy input is {} bytes, above max_input_bytes {}",
                input.len(),
                self.max_input_bytes
            ));
        }
        match self.try_evaluate(input) {
            Ok(Some(decision)) => {
                let size = canonical(&decision).len() as u64;
                if size > self.max_decision_bytes {
                    Outcome::Error(format!(
                        "policy decision is {size} bytes, above max_decision_bytes {}",
                        self.max_decision_bytes
                    ))
                } else {
                    Outcome::Decided(decision)
                }
            }
            Ok(None) => Outcome::Undefined,
            Err(error) => Outcome::Error(format!("{error:#}")),
        }
    }

    fn try_evaluate(&self, input: &str) -> Result<Option<Value>> {
        let mut engine = self.engine.clone();
        engine.set_input(Rego::from_json_str(input).context("invalid policy input")?);
        let decision = engine
            .eval_rule(self.query.clone())
            .map_err(|error| anyhow!("{error:#}"))?;
        if decision == Rego::Undefined {
            return Ok(None);
        }
        Ok(Some(
            serde_json::to_value(&decision).context("policy decision is not JSON")?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(source: &str, query: &str, extra: Value) -> Result<PolicyConfig> {
        let mut value = json!({
            "stream": "policy/out",
            "modules": [{"name": "test.rego", "source": source}],
            "query": query,
        });
        if let (Value::Object(base), Value::Object(extra)) = (&mut value, extra) {
            base.extend(extra);
        }
        PolicyConfig::parse(value)
    }

    #[test]
    fn evaluates_decisions_undefined_and_data() -> Result<()> {
        let policy = CompiledPolicy::compile(&config(
            "package t\nallow if input.x > data.limit\nlabels contains l if { some l in input.tags }",
            "data.t.allow",
            json!({"data": {"limit": 3}}),
        )?)?;
        assert_eq!(policy.evaluate(r#"{"x":4}"#), Outcome::Decided(json!(true)));
        assert_eq!(policy.evaluate(r#"{"x":1}"#), Outcome::Undefined);
        let labels = CompiledPolicy::compile(&config(
            "package t\nlabels contains l if { some l in input.tags }",
            "data.t.labels",
            json!({}),
        )?)?;
        assert_eq!(
            labels.evaluate(r#"{"tags":["b","a","b"]}"#),
            Outcome::Decided(json!(["a", "b"]))
        );
        Ok(())
    }

    #[test]
    fn invalid_policies_and_queries_fail_compilation() -> Result<()> {
        assert!(CompiledPolicy::compile(&config(
            "package t\nallow if {",
            "data.t.allow",
            json!({})
        )?)
        .is_err());
        assert!(CompiledPolicy::compile(&config(
            "package t\nallow := true",
            "data.t.missing",
            json!({})
        )?)
        .is_err());
        assert!(CompiledPolicy::compile(&config(
            "package t\nallow { true }",
            "data.t.allow",
            json!({})
        )?)
        .is_err());
        assert!(CompiledPolicy::compile(&config(
            "package t\nallow { true }",
            "data.t.allow",
            json!({"rego_version": "v0"})
        )?)
        .is_ok());
        Ok(())
    }

    #[test]
    fn nondeterministic_builtins_are_disabled() -> Result<()> {
        for (expression, query) in [
            ("t := time.now_ns()", "data.t.t"),
            ("t := rand.intn(\"x\", 10)", "data.t.t"),
        ] {
            let policy = CompiledPolicy::compile(&config(
                &format!("package t\n{expression}"),
                query,
                json!({"strict_builtin_errors": true}),
            )?)?;
            match policy.evaluate("{}") {
                Outcome::Error(message) => assert!(message.contains("deterministic"), "{message}"),
                other => panic!("expected an error, got {other:?}"),
            }
        }
        let lenient = CompiledPolicy::compile(&config(
            "package t\nt := time.now_ns()",
            "data.t.t",
            json!({}),
        )?)?;
        assert!(
            matches!(lenient.evaluate("{}"), Outcome::Error(message) if message.contains("deterministic"))
        );
        Ok(())
    }

    #[test]
    fn limits_produce_errors() -> Result<()> {
        let policy = CompiledPolicy::compile(&config(
            "package t\nspin := count([x | some x in numbers.range(1, 100000000)])",
            "data.t.spin",
            json!({"timeout_ms": 5, "timeout_check_interval": 1}),
        )?)?;
        assert!(matches!(policy.evaluate("{}"), Outcome::Error(_)));
        let small = CompiledPolicy::compile(&config(
            "package t\necho := input",
            "data.t.echo",
            json!({"max_input_bytes": 16, "max_decision_bytes": 8}),
        )?)?;
        assert!(
            matches!(small.evaluate(r#"{"x":"0123456789abcdef"}"#), Outcome::Error(m) if m.contains("max_input_bytes"))
        );
        assert!(
            matches!(small.evaluate(r#"{"x":"0123456"}"#), Outcome::Error(m) if m.contains("max_decision_bytes"))
        );
        assert!(config(
            "package t\necho := input",
            "data.t.echo",
            json!({"max_policy_bytes": 10}),
        )
        .is_err());
        Ok(())
    }
}
