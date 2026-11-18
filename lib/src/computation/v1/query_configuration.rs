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

/// The labels a query declares it reads, carried in every bootstrap request
/// it sends so providers select only the tables or endpoints that produce
/// them: the node and relation labels its query text references and its
/// inputs name, without synthetic join relations, which no source produces.
///
/// A query whose middleware or input pipelines can rename or derive labels,
/// or that matches a node or relation without a label, does not restrict its
/// requests (`None`): its labels do not bound the source elements it reads.
pub(crate) fn bootstrap_labels(
    execution: &QueryExecutionSettings,
    definition: &super::ContinuousQueryDefinition,
) -> anyhow::Result<Option<drasi_computation_contracts::BootstrapLabels>> {
    if !execution.middleware.is_empty()
        || execution
            .inputs
            .iter()
            .any(|input| !input.pipeline.is_empty())
    {
        return Ok(None);
    }
    let language = match definition.language {
        super::ComputationQueryLanguage::Cypher => crate::config::QueryLanguage::Cypher,
        super::ComputationQueryLanguage::Gql => crate::config::QueryLanguage::GQL,
    };
    if crate::queries::LabelExtractor::matches_unlabeled(&definition.query, &language)? {
        return Ok(None);
    }
    let labels = crate::queries::LabelExtractor::extract_labels(&definition.query, &language)?;
    let synthetic: std::collections::BTreeSet<&str> = execution
        .joins
        .iter()
        .map(|join| join.id.as_str())
        .collect();
    let nodes = labels.node_labels.into_iter().chain(
        execution
            .inputs
            .iter()
            .flat_map(|input| input.nodes.clone()),
    );
    let relations = labels
        .relation_labels
        .into_iter()
        .chain(
            execution
                .inputs
                .iter()
                .flat_map(|input| input.relations.clone()),
        )
        .filter(|label| !synthetic.contains(label.as_str()));
    Ok(Some(drasi_computation_contracts::BootstrapLabels::new(
        nodes, relations,
    )?))
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
        bootstrap_provider: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn definition(query: &str) -> super::super::ContinuousQueryDefinition {
        super::super::ContinuousQueryDefinition {
            graph_id: "graph".into(),
            id: super::super::ComponentId::generate(),
            query: query.into(),
            language: super::super::ComputationQueryLanguage::Cypher,
            output_stream: super::super::StreamId::try_new("query/out").expect("stream"),
            outbox_capacity: std::num::NonZeroUsize::new(16).expect("capacity"),
        }
    }

    #[test]
    fn bootstrap_labels_are_the_querys_source_labels() -> anyhow::Result<()> {
        let definition =
            definition("MATCH (p:Person)-[:KNOWS]->(f:Person), (o:Order) RETURN p, f, o");
        let mut input = drasi_computation_contracts::QueryInputConfig::new(
            super::super::ComponentId::generate(),
        );
        input.nodes = vec!["Customer".into()];
        let execution = QueryExecutionSettings {
            inputs: vec![input],
            joins: vec![drasi_computation_contracts::QueryJoinConfig {
                id: "KNOWS".into(),
                keys: Vec::new(),
            }],
            ..Default::default()
        };
        let labels = bootstrap_labels(&execution, &definition)?.expect("restricted");
        assert_eq!(
            labels.nodes.iter().map(String::as_str).collect::<Vec<_>>(),
            ["Customer", "Order", "Person"]
        );
        assert!(
            labels.relations.is_empty(),
            "a synthetic join relation is produced by the query, not a source"
        );
        Ok(())
    }

    #[test]
    fn an_unlabeled_pattern_leaves_bootstrap_unrestricted() -> anyhow::Result<()> {
        let execution = QueryExecutionSettings::default();
        assert_eq!(
            bootstrap_labels(&execution, &definition("MATCH (n) RETURN n"))?,
            None
        );
        assert_eq!(
            bootstrap_labels(
                &execution,
                &definition("MATCH (a:Person)-[r]->(b:Person) RETURN a, b")
            )?,
            None
        );
        Ok(())
    }

    #[test]
    fn label_mapping_middleware_leaves_bootstrap_unrestricted() -> anyhow::Result<()> {
        let definition = definition("MATCH (p:Projected) RETURN p");
        let mut input = drasi_computation_contracts::QueryInputConfig::new(
            super::super::ComponentId::generate(),
        );
        input.pipeline = vec!["rename".into()];
        let execution = QueryExecutionSettings {
            inputs: vec![input],
            ..Default::default()
        };
        assert_eq!(bootstrap_labels(&execution, &definition)?, None);
        Ok(())
    }

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
