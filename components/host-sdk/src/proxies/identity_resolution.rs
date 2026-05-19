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

//! Shared helper for resolving the per-instance identity provider with
//! fallback to the runtime-context provider.

use drasi_lib::identity::IdentityProvider;
use std::sync::{Arc, Mutex};

/// Resolve the identity provider used by a proxy at `initialize` time.
///
/// Priority order:
/// 1. The per-instance provider stashed via `set_identity_provider` (if any).
/// 2. The provider supplied by the runtime context.
///
/// On a poisoned mutex, recovers the inner value and logs an error tagged
/// with `component_label` (typically `"Source 'foo'"` or `"Reaction 'bar'"`)
/// so an unexpected fallback is diagnosable.
pub(crate) fn resolve_identity_provider(
    per_instance: &Mutex<Option<Arc<dyn IdentityProvider>>>,
    context_provider: Option<Arc<dyn IdentityProvider>>,
    component_label: &str,
) -> Option<Arc<dyn IdentityProvider>> {
    per_instance
        .lock()
        .map(|guard| guard.clone())
        .unwrap_or_else(|poisoned| {
            log::error!(
                "{}: identity_provider mutex poisoned; recovering inner value",
                component_label
            );
            poisoned.into_inner().clone()
        })
        .or(context_provider)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use drasi_lib::identity::{CredentialContext, Credentials};

    struct MockProvider {
        tag: &'static str,
    }

    #[async_trait]
    impl IdentityProvider for MockProvider {
        async fn get_credentials(
            &self,
            _context: &CredentialContext,
        ) -> anyhow::Result<Credentials> {
            Ok(Credentials::UsernamePassword {
                username: self.tag.to_string(),
                password: String::new(),
            })
        }

        fn clone_box(&self) -> Box<dyn IdentityProvider> {
            Box::new(MockProvider { tag: self.tag })
        }
    }

    fn tag_of_sync(p: &Arc<dyn IdentityProvider>) -> String {
        // Resolve the credentials on a fresh single-threaded runtime so the
        // helper can be called from synchronous test bodies.
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let creds = rt
            .block_on(p.get_credentials(&CredentialContext::default()))
            .unwrap();
        match creds {
            Credentials::UsernamePassword { username, .. } => username,
            _ => unreachable!("mock returns UsernamePassword"),
        }
    }

    #[test]
    fn per_instance_provider_takes_precedence_over_context() {
        let per_instance: Mutex<Option<Arc<dyn IdentityProvider>>> =
            Mutex::new(Some(Arc::new(MockProvider {
                tag: "per-instance",
            })));
        let context: Option<Arc<dyn IdentityProvider>> =
            Some(Arc::new(MockProvider { tag: "context" }));

        let resolved = resolve_identity_provider(&per_instance, context, "Reaction 'r1'")
            .expect("resolved provider");
        assert_eq!(tag_of_sync(&resolved), "per-instance");
    }

    #[test]
    fn falls_back_to_context_when_no_per_instance() {
        let per_instance: Mutex<Option<Arc<dyn IdentityProvider>>> = Mutex::new(None);
        let context: Option<Arc<dyn IdentityProvider>> =
            Some(Arc::new(MockProvider { tag: "context" }));

        let resolved = resolve_identity_provider(&per_instance, context, "Reaction 'r1'")
            .expect("resolved provider");
        assert_eq!(tag_of_sync(&resolved), "context");
    }

    #[test]
    fn returns_none_when_neither_provider_set() {
        let per_instance: Mutex<Option<Arc<dyn IdentityProvider>>> = Mutex::new(None);
        let resolved = resolve_identity_provider(&per_instance, None, "Reaction 'r1'");
        assert!(resolved.is_none());
    }

    #[test]
    fn poisoned_mutex_recovers_inner_value() {
        let per_instance: Arc<Mutex<Option<Arc<dyn IdentityProvider>>>> =
            Arc::new(Mutex::new(Some(Arc::new(MockProvider {
                tag: "per-instance",
            }))));

        // Poison the mutex by panicking inside a held lock on another thread.
        let m = per_instance.clone();
        let _ = std::thread::spawn(move || {
            let _guard = m.lock().unwrap();
            panic!("intentional panic to poison the mutex");
        })
        .join();

        assert!(per_instance.is_poisoned());

        // Even though poisoned, the per-instance provider should still be
        // recovered and take precedence over the context provider.
        let context: Option<Arc<dyn IdentityProvider>> =
            Some(Arc::new(MockProvider { tag: "context" }));
        let resolved = resolve_identity_provider(&per_instance, context, "Reaction 'r1'")
            .expect("resolved provider after poison recovery");
        assert_eq!(tag_of_sync(&resolved), "per-instance");
    }

    #[test]
    fn poisoned_mutex_with_no_inner_falls_back_to_context() {
        let per_instance: Arc<Mutex<Option<Arc<dyn IdentityProvider>>>> =
            Arc::new(Mutex::new(None));

        let m = per_instance.clone();
        let _ = std::thread::spawn(move || {
            let _guard = m.lock().unwrap();
            panic!("intentional panic to poison the mutex");
        })
        .join();

        assert!(per_instance.is_poisoned());

        let context: Option<Arc<dyn IdentityProvider>> =
            Some(Arc::new(MockProvider { tag: "context" }));
        let resolved = resolve_identity_provider(&per_instance, context, "Source 's1'")
            .expect("falls back to context");
        assert_eq!(tag_of_sync(&resolved), "context");
    }
}
