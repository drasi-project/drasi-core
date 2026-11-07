// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::sync::Arc;

use crate::config::{AssignedQueryConfig as QueryConfig, SourceSubscriptionSettings};
use drasi_computation_contracts::QueryConfigurationSource;
pub use drasi_computation_contracts::QueryExecutionSettings;
use drasi_core::middleware::MiddlewareTypeRegistry;

pub struct QueryMiddlewareResource(pub Arc<MiddlewareTypeRegistry>);

/// Ordinary API declaration metadata, not an execution or lifecycle owner.
#[derive(Clone)]
pub(crate) struct OrdinaryQueryDeclaration {
    pub(crate) id: super::ComponentId,
    pub(crate) failure: Option<Arc<super::GraphError>>,
    pub(crate) waiting: Vec<super::DependencyWait>,
    pub(crate) backend: Option<Arc<dyn drasi_core::interface::IndexBackendPlugin>>,
    pub(crate) default_dispatch_capacity: usize,
    pub(crate) sources: Vec<Arc<super::SourcePluginHost>>,
    pub(crate) managed: Option<crate::management::OrdinaryComponentRecord>,
    #[cfg(feature = "test-support")]
    pub(crate) test_control: Option<crate::test_support::QueryTestControl>,
}

impl QueryConfigurationSource for QueryConfig {
    type Subscription = SourceSubscriptionSettings;

    fn execution_settings(&self) -> QueryExecutionSettings {
        QueryExecutionSettings {
            joins: self.joins.clone().unwrap_or_default(),
            middleware: self.middleware.clone(),
            inputs: self
                .inputs
                .iter()
                .map(crate::config::QueryInputConfig::execution_input)
                .collect(),
            result: self.result.clone(),
            source_transactions: None,
        }
    }

    fn subscription_settings(&self) -> anyhow::Result<Vec<SourceSubscriptionSettings>> {
        let labels =
            crate::queries::LabelExtractor::extract_labels(&self.query, &self.query_language)?;
        crate::queries::SubscriptionSettingsBuilder::build_subscription_settings(self, &labels)
    }
}

pub(crate) fn query_configuration(
    execution: &QueryExecutionSettings,
    definition: &super::ContinuousQueryDefinition,
    bootstrap: bool,
    recovery: super::QueryRecoveryPolicy,
) -> anyhow::Result<QueryConfig> {
    // Native read contracts have no authority over graph-owned names. Inspection
    // supplies the actual name from the same graph publication as this definition.
    let mut config = crate::Query::cypher("")
        .query(definition.query.clone())
        .enable_bootstrap(bootstrap)
        .with_outbox_capacity(definition.outbox_capacity.get())
        .build()?
        .with_assigned_id(definition.id.clone());
    config.query_language = match definition.language {
        super::ComputationQueryLanguage::Cypher => crate::config::QueryLanguage::Cypher,
        super::ComputationQueryLanguage::Gql => crate::config::QueryLanguage::GQL,
    };
    config.inputs = execution
        .inputs
        .iter()
        .cloned()
        .map(crate::config::QueryInputConfig::from_execution)
        .collect::<anyhow::Result<_>>()?;
    config.result = execution.result.clone();
    config.middleware = execution.middleware.clone();
    config.joins = (!execution.joins.is_empty()).then(|| execution.joins.clone());
    config.recovery_policy = Some(match recovery {
        super::QueryRecoveryPolicy::Strict => crate::RecoveryPolicy::Strict,
        super::QueryRecoveryPolicy::AutoReset => crate::RecoveryPolicy::AutoReset,
    });
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_configuration_preserves_borrowed_and_shared_callers() -> anyhow::Result<()> {
        let source = super::super::ComponentId::generate();
        let config = crate::Query::cypher("query")
            .query("MATCH (n:Item) RETURN n.value AS value")
            .with_input(crate::config::QueryInputConfig::by_id(source))
            .build()?
            .with_assigned_id(super::super::ComponentId::generate());
        let expected = serde_json::to_value(QueryExecutionSettings::from_legacy_config(&config))?;
        let subscriptions =
            serde_json::to_value(QueryExecutionSettings::legacy_subscriptions(&config)?)?;
        let borrowed = &config;
        let shared = Arc::new(config.clone());
        let boxed = Box::new(config.clone());
        for settings in [
            QueryExecutionSettings::from_legacy_config(&borrowed),
            QueryExecutionSettings::from_legacy_config(&shared),
            QueryExecutionSettings::from_legacy_config(&boxed),
        ] {
            assert_eq!(serde_json::to_value(settings)?, expected);
        }
        for settings in [
            QueryExecutionSettings::legacy_subscriptions(&borrowed)?,
            QueryExecutionSettings::legacy_subscriptions(&shared)?,
            QueryExecutionSettings::legacy_subscriptions(&boxed)?,
        ] {
            assert_eq!(serde_json::to_value(settings)?, subscriptions);
        }
        Ok(())
    }
}
