// Copyright 2025 The Drasi Authors.
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

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use crate::channels::QueryResult;
use crate::reactions::Reaction;
use crate::recovery::ReactionRecoveryPolicy;
use crate::sources::Source;

use super::{CatchupContext, QueryConsumer};

pub struct ReactionConsumer(pub Arc<dyn Reaction>);

#[async_trait]
impl QueryConsumer for ReactionConsumer {
    fn consumer_id(&self) -> &str {
        self.0.id()
    }

    fn consumed_query_ids(&self) -> Vec<String> {
        self.0.query_ids()
    }

    async fn enqueue_query_result(&self, result: QueryResult) -> Result<()> {
        self.0.enqueue_query_result(result).await
    }

    fn is_durable(&self) -> bool {
        self.0.is_durable()
    }

    fn needs_snapshot_on_fresh_start(&self) -> bool {
        self.0.needs_snapshot_on_fresh_start()
    }

    fn default_recovery_policy(&self) -> ReactionRecoveryPolicy {
        self.0.default_recovery_policy()
    }

    async fn catch_up(&self, ctx: CatchupContext) -> Result<()> {
        self.0.bootstrap(ctx).await
    }
}

pub struct SourceConsumer(pub Arc<dyn Source>);

#[async_trait]
impl QueryConsumer for SourceConsumer {
    fn consumer_id(&self) -> &str {
        self.0.id()
    }

    fn consumed_query_ids(&self) -> Vec<String> {
        self.0.subscribed_query_ids()
    }

    async fn enqueue_query_result(&self, result: QueryResult) -> Result<()> {
        self.0.enqueue_query_result(result).await
    }
}
