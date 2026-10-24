// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_computation_contracts::QueryOutputMetadata;
use drasi_core::evaluation::{
    context::{QueryPartEvaluationContext, QueryVariables},
    variable_value::VariableValue,
};

fn variables(value: Value) -> QueryVariables {
    value
        .as_object()
        .expect("fixture object")
        .iter()
        .map(|(key, value)| {
            (
                key.clone().into_boxed_str(),
                VariableValue::from(value.clone()),
            )
        })
        .collect()
}
fn input(
    query: &str,
    sequence: u64,
    results: Vec<QueryPartEvaluationContext>,
) -> Result<InputEnvelope> {
    let envelope = QueryChangeCodec::encode_evaluation(
        None,
        &id(query),
        SystemMetadata::new(stream(query), sequence),
        &results,
        QueryOutputMetadata {
            query_id: query.into(),
            source_id: None,
            timestamp: chrono::Utc::now(),
            metadata: Default::default(),
            profiling: None,
        },
    )?
    .context("expected query changes")?;
    Ok(InputEnvelope {
        port: PortId::try_new("in")?,
        envelope,
    })
}
fn added(query: &str, sequence: u64, data: Value) -> Result<InputEnvelope> {
    input(
        query,
        sequence,
        vec![QueryPartEvaluationContext::Adding {
            after: variables(data),
            row_signature: sequence,
        }],
    )
}
fn template(db: &Database, kind: &str, side: &str, before: &str, after: &str) -> String {
    db.scoped_sql(&format!("EXEC {{{{schema}}}}.BindParameters {kind},{{{{param {side}.u}}}},{{{{param {side}.nullable}}}},{{{{param {side}.text}}}},{{{{param (json {side}.payload)}}}},{before},{after}"))
}

pub(super) async fn parameters_and_failure(dynamic: bool) -> Result<()> {
    let mut db = Database::new().await?;
    let native = plugins(dynamic)?;
    let factory = &native[2].factories()[0];
    let data =
        json!({"u":u64::MAX,"nullable":null,"text":"雪💾 O'Brien","payload":{"a":[null,true,"λ"]}});
    let add = |kind: &str| {
        template(
            &db,
            kind,
            "after",
            "{{param after.nullable}}",
            "{{param (json after)}}",
        )
    };
    let update = template(
        &db,
        "{{param operation}}",
        "after",
        "{{param (json before)}}",
        "{{param (json after)}}",
    );
    let delete = template(
        &db,
        "{{param operation}}",
        "before",
        "{{param (json before)}}",
        "{{param before.nullable}}",
    );
    let mut sink = factory.create_component(id("sink"),json!({"settings":{
        "connection":db.settings()["connection"],"queries":["source.q","other.q","unmatched"],
        "routes":{"source.q":{"added":{"template":add("N'full'")}},
            "q":{"added":{"template":add("N'short'")},"updated":{"template":update}}},
        "default_template":{"added":{"template":add("N'default'")},"deleted":{"template":delete}}
    }}))?;
    sink.start().await?;
    let proof = async {
        for (index,query) in ["source.q","other.q","unmatched"].into_iter().enumerate() {
            sink.handle(added(query,index as u64+1,data.clone())?).await?;
        }
        let before = variables(data.clone());
        let mut changed = data.clone(); changed["text"]=json!("updated 雪");
        sink.handle(input("source.q",4,vec![QueryPartEvaluationContext::Updating {
            before:before.clone(),after:variables(changed.clone()),row_signature:1,
        }])?).await?;
        sink.handle(input("source.q",5,vec![QueryPartEvaluationContext::Removing { before:before.clone(),row_signature:1 }])?).await?;
        sink.handle(input("source.q",6,vec![QueryPartEvaluationContext::Aggregation {
            before:Some(before.clone()),after:variables(changed),grouping_keys:vec!["u".into()],
            default_before:false,default_after:false,row_signature:1,
        }])?).await?;
        let rows: Value = serde_json::from_str(&db.sql("SET NOCOUNT ON; SELECT kind,CONVERT(varchar(20),unsigned_value) AS u,
            null_value AS nullable,unicode_value AS text,json_value AS payload,before_value AS before_json,after_value AS after_json
            FROM {{schema}}.Parameters ORDER BY CASE kind WHEN N'full' THEN 1 WHEN N'short' THEN 2 WHEN N'default' THEN 3 WHEN N'DELETE' THEN 5 ELSE 4 END
            FOR JSON PATH,INCLUDE_NULL_VALUES")?)?;
        let rows = rows.as_array().context("parameter effect rows")?;
        ensure!(rows.len()==6,"missing/duplicate effects or unread result sets");
        ensure!(rows[0]["kind"]=="full" && rows[1]["kind"]=="short" && rows[2]["kind"]=="default","full/short/default routing changed");
        for row in rows {
            ensure!(row["u"]=="18446744073709551615" && row["nullable"].is_null(),"exact unsigned/null binding changed");
            ensure!(serde_json::from_str::<Value>(row["payload"].as_str().context("JSON value")?)?==data["payload"],"JSON parameter altered");
            ensure!(row["text"]=="雪💾 O'Brien" || row["text"]=="updated 雪","Unicode/quotes altered");
            if row["kind"]=="UPDATE" || row["kind"]=="DELETE" {
                ensure!(serde_json::from_str::<Value>(row["before_json"].as_str().context("missing before context")?)?==data,
                    "update/delete/aggregation before context changed");
            }
        }
        let mut missing = data.clone(); missing.as_object_mut().context("fixture object")?.remove("text");
        let invalid = input("source.q",7,vec![
            QueryPartEvaluationContext::Adding { after:before.clone(),row_signature:7 },
            QueryPartEvaluationContext::Adding { after:variables(missing),row_signature:8 },
        ])?;
        ensure!(sink.handle(invalid).await.is_err(),"strict missing field silently succeeded");
        ensure!(db.sql("SET NOCOUNT ON; SELECT COUNT(*) FROM {{schema}}.Parameters")?=="6",
            "partial input executed before the complete batch rendered");
        Ok::<_,anyhow::Error>(())
    }.await;
    let cleanup = sink.stop().await;
    proof?;
    cleanup?;

    let mut scalar = factory.create_component(id("scalars"),json!({"settings":{
        "connection":db.settings()["connection"],"queries":["scalars"],
        "default_template":{"added":{"template":db.scoped_sql("EXEC {{schema}}.BindScalars {{param after.flag}},{{param after.signed}},{{param after.floating}},{{param (json after.payload)}}")}}
    }}))?;
    scalar.start().await?;
    let proof = async {
        scalar.handle(added("scalars",1,json!({
            "flag":true,"signed":i64::MIN,"floating":1.25,"payload":[null,{"unicode":"雪"}],
        }))?).await?;
        let row:Value=serde_json::from_str(&db.sql("SET NOCOUNT ON; SELECT flag,CONVERT(varchar(20),signed_value) AS signed,
            floating_value AS floating,json_value AS payload FROM {{schema}}.Scalars FOR JSON PATH,WITHOUT_ARRAY_WRAPPER")?)?;
        ensure!(row["flag"]==true && row["signed"]=="-9223372036854775808" && row["floating"]==1.25,
            "boolean/signed/floating parameter binding changed");
        ensure!(serde_json::from_str::<Value>(row["payload"].as_str().context("array JSON parameter")?)?==json!([null,{"unicode":"雪"}]),
            "array JSON parameter changed");
        Ok::<_,anyhow::Error>(())
    }.await;
    let cleanup = scalar.stop().await;
    proof?;
    cleanup?;

    let mut failing = factory.create_component(id("failure"),json!({"settings":{
        "connection":db.settings()["connection"],"queries":["q"],
        "default_template":{"added":{"template":db.scoped_sql("EXEC {{schema}}.PartialFailure {{param after.id}}")}}
    }}))?;
    failing.start().await?;
    ensure!(
        failing
            .handle(input(
                "q",
                1,
                vec![
                    QueryPartEvaluationContext::Adding {
                        after: variables(json!({"id":91})),
                        row_signature: 91
                    },
                    QueryPartEvaluationContext::Adding {
                        after: variables(json!({"id":92})),
                        row_signature: 92
                    },
                ]
            )?)
            .await
            .is_err(),
        "procedure THROW was treated as handled"
    );
    ensure!(
        db.sql("SET NOCOUNT ON; SELECT COUNT(*) FROM {{schema}}.Effects WHERE id=92")? == "0",
        "later command in a partially failed batch executed after the error"
    );
    ensure!(
        failing
            .handle(added("q", 2, json!({"id":91}))?)
            .await
            .is_err(),
        "interrupted sink accepted more work without cleanup"
    );
    ensure!(db.sql("SET NOCOUNT ON; SELECT COUNT(*) FROM {{schema}}.Effects WHERE operation=N'BEFORE-ERROR' AND id=91")?=="1",
        "executed failing procedure was lost or blindly retried");
    failing.stop().await?;
    db.remove()
}

pub(super) async fn retained_work(dynamic: bool) -> Result<()> {
    let mut db = Database::new().await?;
    let native = plugins(dynamic)?;
    let factory = &native[2].factories()[0];
    for cancellation in [false, true] {
        db.sql("DELETE FROM {{schema}}.Signals WHERE name=N'release-procedure'")?;
        let row = if cancellation { 202 } else { 201 };
        let mut sink = factory.create_component(id("retained"),json!({"settings":{
            "connection":db.settings()["connection"],"queries":["q"],"command_timeout_ms":if cancellation {15000}else{1000},
            "shutdown_timeout_ms":50,"default_template":{"added":{"template":db.scoped_sql("EXEC {{schema}}.DelayedEffect {{param after.id}}")}}
        }}))?;
        sink.start().await?;
        let proof = async {
            let mut handle = Box::pin(sink.handle(added("q",1,json!({"id":row}))?));
            let entered = format!("SET NOCOUNT ON; SELECT COUNT(*) FROM {{{{schema}}}}.Effects WHERE operation=N'ENTERED' AND id={row}");
            tokio::select! {
                result=&mut handle => anyhow::bail!("procedure did not reach retained server work before observation: {}",result.is_ok()),
                observation=db.wait_sql(&entered,"1") => observation?,
            }
            if !cancellation {
                ensure!(tokio::time::timeout(Duration::from_secs(3),&mut handle).await?.is_err(),"command timeout reported success");
            }
            drop(handle);
            let failed = tokio::time::timeout(Duration::from_secs(2),sink.stop()).await?;
            ensure!(failed.is_err(),"stop reported success while procedure was still waiting");
            ensure!(db.sql(&format!("SET NOCOUNT ON; SELECT COUNT(*) FROM {{{{schema}}}}.Effects WHERE operation=N'COMPLETED' AND id={row}"))?=="0",
                "test did not retain an incomplete server-side effect");
            Ok::<_,anyhow::Error>(())
        }.await;
        // Release server work even if a native assertion failed, then await cleanup.
        db.sql("INSERT INTO {{schema}}.Signals VALUES(N'release-procedure')")?;
        db.wait_sql(&format!("SET NOCOUNT ON; SELECT COUNT(*) FROM {{{{schema}}}}.Effects WHERE operation=N'COMPLETED' AND id={row}"),"1").await?;
        let cleanup = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match sink.stop().await {
                    Ok(()) => return Ok::<_, anyhow::Error>(()),
                    Err(error) if format!("{error:#}").contains("timed out") => {
                        tokio::time::sleep(Duration::from_millis(50)).await
                    }
                    Err(error) => return Err(error),
                }
            }
        })
        .await?;
        proof?;
        cleanup?;
        ensure!(
            db.sql(&format!(
                "SET NOCOUNT ON; SELECT COUNT(*) FROM {{{{schema}}}}.Effects WHERE id={row}"
            ))? == "2",
            "cancelled/timed-out procedure was replayed or lost"
        );
    }
    db.remove()
}
