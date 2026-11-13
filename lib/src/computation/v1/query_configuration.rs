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

    fn execution_settings(&self) -> anyhow::Result<QueryExecutionSettings> {
        Ok(QueryExecutionSettings {
            joins: self.joins.clone().unwrap_or_default(),
            middleware: self.middleware.clone(),
            inputs: self
                .inputs
                .iter()
                .map(crate::config::QueryInputConfig::to_execution_input)
                .collect::<anyhow::Result<_>>()?,
            result: self.result.clone(),
            source_transactions: None,
        })
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
) -> anyhow::Result<crate::config::ResolvedQueryConfig> {
    let query_language = match definition.language {
        super::ComputationQueryLanguage::Cypher => crate::config::QueryLanguage::Cypher,
        super::ComputationQueryLanguage::Gql => crate::config::QueryLanguage::GQL,
    };
    let recovery_policy = Some(match recovery {
        super::QueryRecoveryPolicy::Strict => crate::RecoveryPolicy::Strict,
        super::QueryRecoveryPolicy::AutoReset => crate::RecoveryPolicy::AutoReset,
    });
    Ok(crate::config::ResolvedQueryConfig {
        id: definition.id.clone(),
        query: definition.query.clone(),
        query_language,
        middleware: execution.middleware.clone(),
        inputs: execution.inputs.clone(),
        result: execution.result.clone(),
        joins: (!execution.joins.is_empty()).then(|| execution.joins.clone()),
        enable_bootstrap: bootstrap,
        priority_queue_capacity: None,
        dispatch_buffer_capacity: None,
        dispatch_mode: None,
        storage_backend: None,
        recovery_policy,
        outbox_capacity: definition.outbox_capacity.get(),
        bootstrap_timeout_secs: 300,
        source_transactions: execution.source_transactions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_configuration_preserves_borrowed_and_shared_callers() -> anyhow::Result<()> {
        let source = super::super::ComponentId::generate();
        let config = crate::Query::cypher(
            drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
        )
        .query("MATCH (n:Item) RETURN n.value AS value")
        .with_input(crate::config::QueryInputConfig::by_id(source))
        .build()?
        .with_assigned_id(super::super::ComponentId::generate());
        let expected = serde_json::to_value(QueryExecutionSettings::from_configuration(&config)?)?;
        let subscriptions = serde_json::to_value(QueryExecutionSettings::subscriptions(&config)?)?;
        let borrowed = &config;
        let shared = Arc::new(config.clone());
        let boxed = Box::new(config.clone());
        for settings in [
            QueryExecutionSettings::from_configuration(&borrowed)?,
            QueryExecutionSettings::from_configuration(&shared)?,
            QueryExecutionSettings::from_configuration(&boxed)?,
        ] {
            assert_eq!(serde_json::to_value(settings)?, expected);
        }
        for settings in [
            QueryExecutionSettings::subscriptions(&borrowed)?,
            QueryExecutionSettings::subscriptions(&shared)?,
            QueryExecutionSettings::subscriptions(&boxed)?,
        ] {
            assert_eq!(serde_json::to_value(settings)?, subscriptions);
        }
        Ok(())
    }
}
