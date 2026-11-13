// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use drasi_computation_contracts::{
    ChangeNotification, ChangeRoute, ComponentDescriptor, GraphChangeBatch, InputEnvelope,
    OutputAliases, SinkCompletion, SinkOutputs,
};
use drasi_computation_plugin_sdk::{NativeComponent, NativeSink};
use drasi_reaction_common::{OperationType, QueryConfig, TemplateRouting};
use handlebars::Handlebars;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub(super) fn configuration_definition() -> drasi_computation_plugin_sdk::ConfigDefinition {
    use drasi_computation_plugin_sdk::{ConfigDefinition, ConfigOption as Field, ConfigType::*};
    ConfigDefinition::try_new([
        Field::new(
            "outputs",
            Object,
            false,
            "Optional aliases for incoming outputs: alias to {component, port}.",
        )
        .constraint(drasi_computation_contracts::ConfigurationConstraint::output_aliases()),
        Field::new(
            "routes",
            Object,
            false,
            "Per-output templates for added, updated, and deleted changes.",
        ),
        Field::new(
            "defaultTemplate",
            Object,
            false,
            "Default templates used when an output has no matching route.",
        ),
    ])
    .expect("constant log sink configuration")
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LogSinkConfig {
    #[serde(default, skip_serializing_if = "OutputAliases::is_empty")]
    pub outputs: OutputAliases,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub routes: HashMap<String, QueryConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_template: Option<QueryConfig>,
}

impl TemplateRouting for LogSinkConfig {
    fn routes(&self) -> &HashMap<String, QueryConfig> {
        &self.routes
    }

    fn default_template(&self) -> Option<&QueryConfig> {
        self.default_template.as_ref()
    }
}

impl LogSinkConfig {
    pub fn parse(value: serde_json::Value) -> Result<Self> {
        let config: Self = serde_json::from_value(value)?;
        config.outputs.validate()?;
        Ok(config)
    }

    fn validate(&self, outputs: &SinkOutputs) -> Result<()> {
        let aliases: Vec<_> = outputs.aliases().collect();
        for route in self.routes.keys() {
            ensure!(
                aliases
                    .iter()
                    .any(|alias| *alias == route || alias.ends_with(&format!(".{route}"))),
                "log route '{route}' does not match an incoming output"
            );
        }
        for template in self
            .routes
            .values()
            .chain(self.default_template.iter())
            .flat_map(|config| {
                [
                    config.added.as_ref(),
                    config.updated.as_ref(),
                    config.deleted.as_ref(),
                ]
                .into_iter()
                .flatten()
            })
        {
            if !template.template.is_empty() {
                handlebars::Template::compile(&template.template)
                    .context("invalid log template")?;
            }
        }
        Ok(())
    }
}

pub(super) struct LogSink {
    descriptor: ComponentDescriptor,
    config: LogSinkConfig,
    outputs: SinkOutputs,
    handlebars: Handlebars<'static>,
    started: bool,
}

impl LogSink {
    pub(super) fn new(
        descriptor: ComponentDescriptor,
        config: LogSinkConfig,
        outputs: SinkOutputs,
    ) -> Result<Self> {
        config.validate(&outputs)?;
        Ok(Self {
            descriptor,
            config,
            outputs,
            handlebars: Handlebars::new(),
            started: false,
        })
    }

    fn operation(notification: &ChangeNotification) -> OperationType {
        match notification.change().route() {
            ChangeRoute::Added => OperationType::Add,
            ChangeRoute::Updated => OperationType::Update,
            ChangeRoute::Deleted => OperationType::Delete,
        }
    }

    fn render(&self, notification: &ChangeNotification) -> Result<String> {
        let context = notification.to_json()?;
        let Some(template) = self
            .config
            .get_template_spec(notification.alias(), Self::operation(notification))
            .map(|template| template.template.as_str())
            .filter(|template| !template.is_empty())
        else {
            return Ok(serde_json::to_string(&context)?);
        };
        Ok(self.handlebars.render_template(template, &context)?)
    }

    fn messages(&self, input: &InputEnvelope) -> Result<Vec<String>> {
        ensure!(input.port.as_str() == "in", "unknown log input port");
        let output = self
            .outputs
            .output_for(input.envelope.system().stream())
            .context("log input stream is not bound to an output")?;
        GraphChangeBatch::from_envelope(&input.envelope)?
            .notifications(output)
            .into_iter()
            .map(|notification| self.render(&notification))
            .collect()
    }
}

#[async_trait]
impl NativeComponent for LogSink {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }

    fn configuration(&self) -> Result<serde_json::Value> {
        Ok(serde_json::to_value(&self.config)?)
    }

    async fn start(&mut self) -> Result<()> {
        ensure!(!self.started, "log sink already started");
        self.started = true;
        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        self.started = false;
        Ok(())
    }
}

#[async_trait]
impl NativeSink for LogSink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }

    async fn handle(&mut self, input: InputEnvelope) -> Result<()> {
        ensure!(self.started, "log sink is not running");
        for message in self.messages(&input)? {
            log::info!("[{}] {message}", self.descriptor.id());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_computation_contracts::{
        ComponentId, GraphChangeCodec, InputBinding, OutputEndpoint, PortDescriptor, PortDirection,
        PortId, StreamId,
    };
    use drasi_core_types::models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
    };
    use std::sync::Arc;

    const SOURCE_ID: &str = "5c3e49cc-32f8-5b75-888d-30b75b572221";

    fn sink(template: Option<&str>) -> Result<LogSink> {
        let source = ComponentId::try_new(SOURCE_ID)?;
        let stream = StreamId::try_new(format!("{SOURCE_ID}/out"))?;
        let inputs = [InputBinding {
            input_port: PortId::try_new("in")?,
            output: OutputEndpoint::new(source.clone(), PortId::try_new("out")?),
            stream,
        }];
        let mut value = serde_json::json!({
            "outputs":{"items":{"component":SOURCE_ID,"port":"out"}}
        });
        if let Some(template) = template {
            value["defaultTemplate"] = serde_json::json!({"added":{"template":template}});
        }
        let config = LogSinkConfig::parse(value)?;
        let outputs = SinkOutputs::from_inputs(&inputs, &config.outputs)?;
        LogSink::new(
            ComponentDescriptor::try_new(
                ComponentId::try_new("b11e8f74-9ac7-5734-b9b8-d9fd4c356f13")?,
                vec![PortDescriptor::new(
                    PortId::try_new("in")?,
                    PortDirection::Input,
                    GraphChangeCodec::schema().descriptor().clone(),
                    Default::default(),
                )],
            )?,
            config,
            outputs,
        )
    }

    fn input() -> Result<InputEnvelope> {
        let element = Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new(SOURCE_ID, "one"),
                labels: Arc::from([Arc::from("Item")]),
                effective_from: 1,
            },
            properties: ElementPropertyMap::new(),
        };
        Ok(InputEnvelope {
            port: PortId::try_new("in")?,
            envelope: GraphChangeCodec::encode_change(
                SourceChange::Insert { element },
                StreamId::try_new(format!("{SOURCE_ID}/out"))?,
                7,
                None,
            )?,
        })
    }

    #[test]
    fn templates_and_default_json_use_native_change_context() -> Result<()> {
        let rendered =
            sink(Some("{{output.alias}} {{operation}} {{identity.id}}"))?.messages(&input()?)?;
        assert_eq!(rendered, ["items ADD one"]);
        let raw: serde_json::Value = serde_json::from_str(&sink(None)?.messages(&input()?)?[0])?;
        assert_eq!(raw["output"]["alias"], "items");
        assert_eq!(raw["operation"], "ADD");
        assert_eq!(raw["identity"]["namespace"], SOURCE_ID);
        Ok(())
    }
}
