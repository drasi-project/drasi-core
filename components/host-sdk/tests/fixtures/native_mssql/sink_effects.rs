// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Authored, not yet qualified (CW-78): the native SQL Server sink is an
//! external consumer whose operations each commit with a destination cursor
//! row in `<schema>.DrasiDeliveryProgress`. The fixture must pre-create that
//! table and grant the application login SELECT, INSERT, UPDATE and DELETE on
//! it before these scenarios can run.

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
        &ResultNodeProjector::try_new(&format!("test/native-mssql-sink/{query}"), None)?,
        &results,
        QueryOutputMetadata {
            query_id: id(query),
            input_origin: None,
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
    let (mut sink, _sink_state) = crate::create_sink(
        factory,
        id("sink"),
        crate::sink_settings(
            &db,
            json!({
                "outputs":{"source.q":{"component":"source.q","port":"out"},"other.q":{"component":"other.q","port":"out"},"unmatched":{"component":"unmatched","port":"out"}},
                "routes":{"source.q":{"added":{"template":add("N'full'")}},
                    "q":{"added":{"template":add("N'short'")},"updated":{"template":update}}},
                "defaultTemplate":{"added":{"template":add("N'default'")},"deleted":{"template":delete}}
            }),
        ),
    )
    .await?;
    sink.initialize(&crate::hooks::initialize_ctx()).await?;
    sink.start(&crate::hooks::start_ctx()).await?;
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
            if row["kind"]=="REPLACE" || row["kind"]=="DELETE" {
                ensure!(serde_json::from_str::<Value>(row["before_json"].as_str().context("missing before context")?)?==data,
                    "update/delete/aggregation before context changed");
            }
        }
        // Operations commit one at a time: the first change of this input
        // commits with its cursor; the second fails to render (permanently).
        let mut missing = data.clone(); missing.as_object_mut().context("fixture object")?.remove("text");
        let invalid = input("source.q",7,vec![
            QueryPartEvaluationContext::Adding { after:before.clone(),row_signature:7 },
            QueryPartEvaluationContext::Adding { after:variables(missing),row_signature:8 },
        ])?;
        ensure!(sink.handle(invalid).await.is_err(),"strict missing field silently succeeded");
        ensure!(db.sql("SET NOCOUNT ON; SELECT COUNT(*) FROM {{schema}}.Parameters")?=="7",
            "the completed operation prefix was not committed exactly once");
        Ok::<_,anyhow::Error>(())
    }.await;
    let cleanup = sink.stop(&crate::hooks::stop_ctx()).await;
    proof?;
    cleanup?;

    let (mut scalar, _scalar_state) = crate::create_sink(
        factory,
        id("scalars"),
        crate::sink_settings(&db, json!({
            "outputs":{"scalars":{"component":"scalars","port":"out"}},
            "defaultTemplate":{"added":{"template":db.scoped_sql("EXEC {{schema}}.BindScalars {{param after.flag}},{{param after.signed}},{{param after.floating}},{{param (json after.payload)}}")}}
        })),
    )
    .await?;
    scalar.initialize(&crate::hooks::initialize_ctx()).await?;
    scalar.start(&crate::hooks::start_ctx()).await?;
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
    let cleanup = scalar.stop(&crate::hooks::stop_ctx()).await;
    proof?;
    cleanup?;

    let (mut failing, _failing_state) = crate::create_sink(
        factory,
        id("failure"),
        crate::sink_settings(&db, json!({
            "outputs":{"q":{"component":"q","port":"out"}},
            "defaultTemplate":{"added":{"template":db.scoped_sql("EXEC {{schema}}.PartialFailure {{param after.id}}")}}
        })),
    )
    .await?;
    failing.initialize(&crate::hooks::initialize_ctx()).await?;
    failing.start(&crate::hooks::start_ctx()).await?;
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
        "a later operation ran after the failed one"
    );
    // The failed operation's transaction rolled back its partial effect.
    ensure!(db.sql("SET NOCOUNT ON; SELECT COUNT(*) FROM {{schema}}.Effects WHERE operation=N'BEFORE-ERROR' AND id=91")?=="0",
        "the failed procedure's effect escaped its transaction");
    ensure!(
        db.sql("SET NOCOUNT ON; SELECT COUNT(*) FROM {{schema}}.DrasiDeliveryProgress WHERE completed > 0")? == "0",
        "the cursor advanced for a failed operation"
    );
    failing.stop(&crate::hooks::stop_ctx()).await?;
    db.remove()
}

/// Exactly-once across an interrupted delivery: the host drops a call while
/// the procedure runs; the connection goes with it, SQL Server rolls the
/// transaction back, Stop succeeds immediately (nothing is retained), and a
/// redelivery after restart commits the effect once.
pub(super) async fn retained_work(dynamic: bool) -> Result<()> {
    let mut db = Database::new().await?;
    let native = plugins(dynamic)?;
    let factory = &native[2].factories()[0];
    db.sql("DELETE FROM {{schema}}.Signals WHERE name=N'release-procedure'")?;
    let row = 201;
    let (mut sink, _sink_state) = crate::create_sink(
        factory,
        id("retained"),
        crate::sink_settings(&db, json!({
            "outputs":{"q":{"component":"q","port":"out"}},"commandTimeoutMs":15000,"retryBudgetMs":0,
            "defaultTemplate":{"added":{"template":db.scoped_sql("EXEC {{schema}}.DelayedEffect {{param after.id}}")}}
        })),
    )
    .await?;
    sink.initialize(&crate::hooks::initialize_ctx()).await?;
    sink.start(&crate::hooks::start_ctx()).await?;
    let proof = async {
        let mut handle = Box::pin(sink.handle(added("q",1,json!({"id":row}))?));
        // The uncommitted ENTERED row is visible only to a dirty read.
        let entered = format!("SET NOCOUNT ON; SELECT COUNT(*) FROM {{{{schema}}}}.Effects WITH (NOLOCK) WHERE operation=N'ENTERED' AND id={row}");
        tokio::select! {
            result=&mut handle => anyhow::bail!("procedure did not reach server work before observation: {}",result.is_ok()),
            observation=db.wait_sql(&entered,"1") => observation?,
        }
        drop(handle);
        tokio::time::timeout(Duration::from_secs(2),sink.stop(&crate::hooks::stop_ctx())).await??;
        Ok::<_,anyhow::Error>(())
    }.await;
    db.sql("INSERT INTO {{schema}}.Signals VALUES(N'release-procedure')")?;
    // The abandoned session ends and its transaction rolls back.
    db.wait_sql(
        &format!("SET NOCOUNT ON; SELECT COUNT(*) FROM {{{{schema}}}}.Effects WHERE id={row}"),
        "0",
    )
    .await?;
    proof?;
    sink.start(&crate::hooks::start_ctx()).await?;
    sink.handle(added("q", 1, json!({"id":row}))?).await?;
    sink.stop(&crate::hooks::stop_ctx()).await?;
    ensure!(
        db.sql(&format!(
            "SET NOCOUNT ON; SELECT COUNT(*) FROM {{{{schema}}}}.Effects WHERE id={row}"
        ))? == "2",
        "the interrupted procedure was lost or committed twice"
    );
    db.remove()
}
