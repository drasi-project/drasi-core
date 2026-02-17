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

use super::{Credentials, IdentityProvider};
use anyhow::Result;
use async_trait::async_trait;

/// Identity provider for traditional username/password authentication.
#[derive(Clone)]
pub struct PasswordIdentityProvider {
    username: String,
    password: String,
}

impl PasswordIdentityProvider {
    /// Create a new password identity provider.
    pub fn new(username: impl Into<String>, password: impl Into<String>) -> Self {
        Self {
            username: username.into(),
            password: password.into(),
        }
    }
}

#[async_trait]
impl IdentityProvider for PasswordIdentityProvider {
    async fn get_credentials(&self) -> Result<Credentials> {
        Ok(Credentials::UsernamePassword {
            username: self.username.clone(),
            password: self.password.clone(),
        })
    }

    fn clone_box(&self) -> Box<dyn IdentityProvider> {
        Box::new(self.clone())
    }
}
