// Copyright 2026 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! AHP getting started: watch AI agent sessions with continuous queries.
//!
//! By default a scripted demo AHP host runs in-process. Set `AHP_URL`
//! (and optionally `AHP_TOKEN`) to watch a real AHP host instead.

mod demo_host;

use anyhow::Result;
use drasi_bootstrap_ahp::AhpBootstrapProvider;
use drasi_lib::{DrasiLib, Query};
use drasi_reaction_log::{LogReaction, QueryConfig, TemplateSpec};
use drasi_source_ahp::AhpSource;

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    // 1. Pick an AHP host: a real one from AHP_URL, or the built-in demo.
    let (url, demo) = match std::env::var("AHP_URL") {
        Ok(url) => (url, None),
        Err(_) => {
            let host = demo_host::start().await?;
            (host.url(), Some(host))
        }
    };
    let token = std::env::var("AHP_TOKEN").ok();
    println!("Watching AHP host at {url}");

    // 2. The AHP source streams sessions; the bootstrap provider loads what exists now.
    let mut bootstrap = AhpBootstrapProvider::builder().with_url(&url);
    let mut source = AhpSource::builder("agents").with_url(&url);
    if let Some(token) = token {
        bootstrap = bootstrap.with_bearer_token(&token);
        source = source.with_bearer_token(token);
    }
    let source = source.with_bootstrap_provider(bootstrap.build()?).build()?;

    // 3. Continuous queries over the agent graph.
    let waiting_on_me = Query::cypher("waiting-on-me")
        .query(
            "MATCH (s:Session)-[:NEEDS_INPUT]->(r:InputRequest)
             RETURN s.title AS session, r.message AS question",
        )
        .from_source("agents")
        .build();

    let failed = Query::cypher("failed-sessions")
        .query(
            "MATCH (s:Session)-[:USES_AGENT]->(a:Agent)
             WHERE s.hasError = true
             RETURN s.title AS session, a.displayName AS agent",
        )
        .from_source("agents")
        .build();

    let big_changes = Query::cypher("big-changes")
        .query(
            "MATCH (s:Session)
             WHERE s.filesChanged >= 20
             RETURN s.title AS session, s.filesChanged AS files,
                    s.additions AS additions, s.deletions AS deletions",
        )
        .from_source("agents")
        .build();

    // 4. Print every change to the query results.
    let reaction = LogReaction::builder("console")
        .with_queries(vec![
            "waiting-on-me".into(),
            "failed-sessions".into(),
            "big-changes".into(),
        ])
        .with_route(
            "waiting-on-me",
            templates(
                "🙋 {{after.session}} needs you: {{after.question}}",
                "🙋 {{after.session}} changed its question: {{after.question}}",
                "✅ {{before.session}} is no longer waiting",
            ),
        )
        .with_route(
            "failed-sessions",
            templates(
                "❌ {{after.session}} ({{after.agent}}) failed",
                "❌ {{after.session}} ({{after.agent}}) failed",
                "🔁 {{before.session}} recovered",
            ),
        )
        .with_route(
            "big-changes",
            templates(
                "📦 {{after.session}} changed {{after.files}} files (+{{after.additions}} / -{{after.deletions}})",
                "📦 {{after.session}} now at {{after.files}} files (+{{after.additions}} / -{{after.deletions}})",
                "📦 {{before.session}} is back under the threshold",
            ),
        )
        .build()?;

    let core = DrasiLib::builder()
        .with_id("ahp-getting-started")
        .with_source(source)
        .with_query(waiting_on_me)
        .with_query(failed)
        .with_query(big_changes)
        .with_reaction(reaction)
        .build()
        .await?;
    core.start().await?;

    if let Some(host) = &demo {
        tokio::spawn(demo_host::simulate(host.clone()));
    }
    println!("Press Ctrl+C to stop");
    tokio::signal::ctrl_c().await?;
    core.stop().await?;
    Ok(())
}

fn templates(added: &str, updated: &str, deleted: &str) -> QueryConfig {
    QueryConfig {
        added: Some(TemplateSpec::new(added)),
        updated: Some(TemplateSpec::new(updated)),
        deleted: Some(TemplateSpec::new(deleted)),
    }
}
