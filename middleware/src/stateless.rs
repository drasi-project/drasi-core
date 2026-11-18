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

//! Middleware whose output depends only on the change it is given.
//!
//! A [`StatelessMiddleware`] has no element-index parameter, so a host without
//! an index (a native transformer, for example) can run it. Implementing the
//! trait is the declaration that a kind never reads stored elements; `unwind`,
//! which looks up the previous image of an element, does not implement it.

use std::sync::Arc;

use async_trait::async_trait;
use drasi_core_types::{
    interface::{MiddlewareError, MiddlewareSetupError},
    models::{SourceChange, SourceMiddlewareConfig},
};

/// A middleware that transforms one change without consulting stored state.
#[async_trait]
pub trait StatelessMiddleware: Send + Sync {
    async fn apply(&self, change: SourceChange) -> Result<Vec<SourceChange>, MiddlewareError>;
}

/// Builds one [`StatelessMiddleware`] kind from its declaration.
pub trait StatelessMiddlewareFactory: Send + Sync {
    /// The `kind` a middleware declaration names to select this factory.
    fn kind(&self) -> &'static str;
    fn build_stateless(
        &self,
        config: &SourceMiddlewareConfig,
    ) -> Result<Box<dyn StatelessMiddleware>, MiddlewareSetupError>;
}

/// The stateless middleware kinds compiled into this build. Hosts that publish
/// a fixed kind list must select from it by name rather than publishing this
/// list, because Cargo feature unification can add kinds to a build.
pub fn stateless_middleware_factories() -> Vec<Arc<dyn StatelessMiddlewareFactory>> {
    vec![
        #[cfg(feature = "jq")]
        Arc::new(crate::jq::JQFactory::new()),
        #[cfg(feature = "map")]
        Arc::new(crate::map::MapFactory::new()),
        #[cfg(feature = "relabel")]
        Arc::new(crate::relabel::RelabelMiddlewareFactory::new()),
        #[cfg(feature = "decoder")]
        Arc::new(crate::decoder::DecoderFactory::new()),
        #[cfg(feature = "parse_json")]
        Arc::new(crate::parse_json::ParseJsonFactory::new()),
        #[cfg(feature = "promote")]
        Arc::new(crate::promote::PromoteMiddlewareFactory::new()),
    ]
}
