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

#![allow(unexpected_cfgs)]

//! Hermetic **test-fixture** identity provider plugin for Drasi.
//!
//! This crate exists solely to exercise the FFI identity-provider boundary in
//! integration tests (see `components/host-sdk/tests/integration_test.rs`). It returns
//! deterministic, canned credentials with **no external dependencies** (unlike the real
//! `drasi-identity-aws` / `drasi-identity-azure` plugins, which require live cloud
//! credentials), so it can be loaded as a real cdylib and driven across the FFI boundary
//! from a hermetic test.
//!
//! Every `get_credentials` call increments a process-global counter that the host test can
//! read back across the FFI boundary via the exported
//! [`drasi_test_identity_get_credentials_calls`] accessor. This lets a test assert that N
//! `get_credentials` calls made by a *source* plugin traversed **both** cdylib boundaries
//! (source plugin → host → this identity plugin) intact.

use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use drasi_lib::identity::{CredentialContext, Credentials, IdentityProvider};
use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;

/// The deterministic username returned by [`TestIdentityProvider::get_credentials`].
pub const TEST_USERNAME: &str = "test-user";
/// The deterministic password returned by [`TestIdentityProvider::get_credentials`].
pub const TEST_PASSWORD: &str = "test-pass";

/// Process-global count of `get_credentials` calls served by this plugin.
static GET_CREDENTIALS_CALLS: AtomicUsize = AtomicUsize::new(0);

/// Returns the number of `get_credentials` calls served since the last reset.
///
/// Exported so a host integration test can read the cdylib-resident counter across the
/// FFI boundary (resolved via `libloading::Library::get`).
#[no_mangle]
pub extern "C" fn drasi_test_identity_get_credentials_calls() -> usize {
    GET_CREDENTIALS_CALLS.load(Ordering::SeqCst)
}

/// Resets the `get_credentials` call counter to zero.
#[no_mangle]
pub extern "C" fn drasi_test_identity_reset() {
    GET_CREDENTIALS_CALLS.store(0, Ordering::SeqCst);
}

/// Test identity provider that returns canned [`Credentials::UsernamePassword`] and counts
/// how many times it has been invoked.
#[derive(Clone, Default)]
pub struct TestIdentityProvider;

#[async_trait]
impl IdentityProvider for TestIdentityProvider {
    async fn get_credentials(&self, _context: &CredentialContext) -> anyhow::Result<Credentials> {
        GET_CREDENTIALS_CALLS.fetch_add(1, Ordering::SeqCst);
        Ok(Credentials::UsernamePassword {
            username: TEST_USERNAME.to_string(),
            password: TEST_PASSWORD.to_string(),
        })
    }

    fn clone_box(&self) -> Box<dyn IdentityProvider> {
        Box::new(self.clone())
    }
}

/// Configuration DTO for the test identity provider plugin (intentionally empty).
#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TestIdentityProviderConfigDto {}

#[derive(utoipa::OpenApi)]
#[openapi(components(schemas(TestIdentityProviderConfigDto)))]
struct TestIdentityProviderSchemas;

/// Descriptor for the test identity provider plugin.
pub struct TestIdentityProviderDescriptor;

#[async_trait]
impl IdentityProviderPluginDescriptor for TestIdentityProviderDescriptor {
    fn kind(&self) -> &str {
        "test"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_json(&self) -> String {
        let api = TestIdentityProviderSchemas::openapi();
        serde_json::to_string(
            &api.components
                .as_ref()
                .expect("OpenAPI components missing")
                .schemas,
        )
        .expect("Failed to serialize config schema")
    }

    fn config_schema_name(&self) -> &str {
        "identity.test.TestIdentityProviderConfig"
    }

    async fn create_identity_provider(
        &self,
        _config_json: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn IdentityProvider>> {
        Ok(Box::new(TestIdentityProvider))
    }
}

// Dynamic plugin entry point
#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "identity-test",
    core_version = "0.4.0",
    lib_version = "0.4.0",
    plugin_version = "0.1.0",
    source_descriptors = [],
    reaction_descriptors = [],
    bootstrap_descriptors = [],
    identity_provider_descriptors = [TestIdentityProviderDescriptor],
);
