// Copyright 2024 The Drasi Authors.
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

pub mod common;

#[cfg(feature = "decoder")]
pub mod decoder;

#[cfg(feature = "jq")]
pub mod jq;

#[cfg(feature = "map")]
pub mod map;

#[cfg(feature = "parse_json")]
pub mod parse_json;

#[cfg(feature = "promote")]
pub mod promote;

#[cfg(feature = "relabel")]
pub mod relabel;

#[cfg(feature = "unwind")]
pub mod unwind;

/// Existing middleware implementations enabled in this build. Their interfaces
/// depend only on shared data/index contracts, not on the query evaluator.
#[cfg(feature = "engine-adapter")]
pub fn source_middleware_factories(
) -> Vec<std::sync::Arc<dyn drasi_core_types::interface::SourceMiddlewareFactory>> {
    vec![
        #[cfg(feature = "jq")]
        std::sync::Arc::new(jq::JQFactory::new()),
        #[cfg(feature = "map")]
        std::sync::Arc::new(map::MapFactory::new()),
        #[cfg(feature = "unwind")]
        std::sync::Arc::new(unwind::UnwindFactory::new()),
        #[cfg(feature = "relabel")]
        std::sync::Arc::new(relabel::RelabelMiddlewareFactory::new()),
        #[cfg(feature = "decoder")]
        std::sync::Arc::new(decoder::DecoderFactory::new()),
        #[cfg(feature = "parse_json")]
        std::sync::Arc::new(parse_json::ParseJsonFactory::new()),
        #[cfg(feature = "promote")]
        std::sync::Arc::new(promote::PromoteMiddlewareFactory::new()),
    ]
}
