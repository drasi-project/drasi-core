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

//! # ApplicationIdentityProvider Example
//!
//! Demonstrates how a host application can plug its own credential-acquisition
//! code into DrasiLib via [`ApplicationIdentityProvider`]. This is the
//! identity-provider counterpart of `ApplicationSource`, `ApplicationReaction`,
//! and `ApplicationBootstrapProvider`: instead of running a separate identity
//! plugin (Azure, AWS, etc.), the host supplies a closure that returns
//! credentials.
//!
//! Run with:
//! ```bash
//! cargo run
//! ```

use std::sync::Arc;

use anyhow::Result;
use drasi_lib::identity::{
    ApplicationIdentityProvider, CredentialContext, Credentials, IdentityProvider,
};

#[tokio::main]
async fn main() -> Result<()> {
    println!("ApplicationIdentityProvider example\n");

    // -----------------------------------------------------------------------
    // 1. Build a provider from an async closure.
    //
    //    In a real host this is where you'd call into your existing auth
    //    machinery (Azure DefaultAzureCredential, AWS SDK, HashiCorp Vault,
    //    a token cache, etc.).
    // -----------------------------------------------------------------------
    let provider = ApplicationIdentityProvider::new(|ctx: &CredentialContext| {
        // Snapshot context properties so the future can be 'static.
        let host = ctx.get("hostname").unwrap_or("default").to_string();
        let database = ctx.get("database").map(str::to_string);

        async move {
            // Pretend we did an async token exchange here.
            let username = match database.as_deref() {
                Some(db) => format!("svc@{host}/{db}"),
                None => format!("svc@{host}"),
            };
            Ok(Credentials::Token {
                username,
                token: "eyJ...example.token".to_string(),
            })
        }
    });

    // -----------------------------------------------------------------------
    // 2. Wrap as `Arc<dyn IdentityProvider>` so DrasiLib can inject it into
    //    sources/reactions. The `with_identity_provider` builder method on
    //    `DrasiLib::builder()` accepts exactly this type — wiring is the
    //    same as for any other identity provider:
    //
    //        DrasiLib::builder()
    //            .with_identity_provider(provider.clone())
    //            .build()
    //            .await?;
    // -----------------------------------------------------------------------
    let provider: Arc<dyn IdentityProvider> = Arc::new(provider);

    // -----------------------------------------------------------------------
    // 3. Show that the closure is invoked with whatever context the consumer
    //    (a source or reaction) supplies.
    // -----------------------------------------------------------------------
    let prod_ctx = CredentialContext::new()
        .with_property("hostname", "prod-db.internal")
        .with_property("database", "orders");
    let staging_ctx = CredentialContext::new().with_property("hostname", "staging-db.internal");

    let prod_creds = provider.get_credentials(&prod_ctx).await?;
    let staging_creds = provider.get_credentials(&staging_ctx).await?;

    println!("prod credentials   : {prod_creds:?}");
    println!("staging credentials: {staging_creds:?}");

    // -----------------------------------------------------------------------
    // 4. `new_sync` is available for callbacks that don't need to await
    //    (handy for tests or static credentials).
    // -----------------------------------------------------------------------
    let static_provider = ApplicationIdentityProvider::new_sync(|_ctx| {
        Ok(Credentials::UsernamePassword {
            username: "alice".into(),
            password: "s3cret".into(),
        })
    });

    let static_creds = static_provider
        .get_credentials(&CredentialContext::new())
        .await?;
    println!("static credentials : {static_creds:?}");

    Ok(())
}
