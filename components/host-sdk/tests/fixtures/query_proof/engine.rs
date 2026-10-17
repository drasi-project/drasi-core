// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::wire;
use anyhow::Result;
use drasi_computation_contracts::*;
use drasi_core::{
    evaluation::functions::FunctionRegistry,
    interface::{ElementIndex, FutureQueue, ResultIndex},
    query::{QueryBuilder, QueryEvaluator},
};
use drasi_functions_cypher::CypherFunctionSet;
use drasi_query_cypher::CypherParser;
use std::{
    collections::HashMap,
    num::NonZeroUsize,
    sync::{Arc, OnceLock},
};

pub fn codec() -> &'static BinaryEnvelopeCodec {
    static CODEC: OnceLock<BinaryEnvelopeCodec> = OnceLock::new();
    CODEC.get_or_init(|| {
        let mut codec = BinaryEnvelopeCodec::new(NonZeroUsize::new(wire::LIMIT).expect("limit"));
        codec
            .register_schema(GraphChangeCodec::schema())
            .expect("graph schema");
        codec
            .register_schema(QueryChangeCodec::schema())
            .expect("query schema");
        codec
    })
}

pub async fn build(
    text: &str,
    elements: Arc<dyn ElementIndex>,
    results: Arc<dyn ResultIndex>,
    futures: Arc<dyn FutureQueue>,
) -> Result<QueryEvaluator> {
    let functions = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
    Ok(
        QueryBuilder::new(text, Arc::new(CypherParser::new(functions.clone())))
            .with_function_registry(functions)
            .with_element_index(elements)
            .with_result_index(results)
            .with_future_queue(futures)
            .try_build_evaluator()
            .await?,
    )
}

pub async fn evaluate(query: &QueryEvaluator, input: &[u8]) -> Result<Vec<u8>> {
    let input = codec().decode(input)?;
    let changes = GraphChangeCodec::decode_changes(&input)?;
    let result = query.evaluate_changes(changes).await?;
    let output = QueryChangeCodec::encode_evaluation(
        Some(&input),
        &ComponentId::try_new("query")?,
        SystemMetadata::new(StreamId::try_new("query/out")?, input.system().sequence()),
        &result,
        QueryOutputMetadata {
            query_id: "query".into(),
            source_id: Some("source".into()),
            timestamp: input.system().timestamp().expect("proof input timestamp"),
            metadata: HashMap::new(),
            profiling: None,
        },
    )?;
    let output = output
        .as_ref()
        .map(|value| codec().encode(value))
        .transpose()?;
    wire::encode(&output.map(|value| serde_bytes::ByteBuf::from(value.to_vec())))
}
