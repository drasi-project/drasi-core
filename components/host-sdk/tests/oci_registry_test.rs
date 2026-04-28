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

//! Integration tests for the OCI registry client.
//!
//! These tests hit the real GHCR registry (ghcr.io/drasi-project) and are
//! marked `#[ignore]` so they don't run in normal `cargo test`.
//!
//! Run with:
//! ```sh
//! cargo test -p drasi-host-sdk --features registry --test oci_registry_test -- --ignored --nocapture
//! ```

#[cfg(feature = "registry")]
mod tests {
    use drasi_host_sdk::registry::{OciRegistryClient, RegistryConfig};

    fn client() -> OciRegistryClient {
        OciRegistryClient::new(RegistryConfig::default())
    }

    // ── list_tags ────────────────────────────────────────────────────────

    #[tokio::test]
    #[ignore] // requires network access to GHCR
    async fn list_tags_returns_tags_for_known_plugin() {
        let c = client();
        let tags = c.list_tags("source/postgres").await.unwrap();

        assert!(!tags.is_empty(), "expected tags for source/postgres");
        println!("source/postgres: {} tags", tags.len());

        // Should contain at least one platform-specific tag
        let has_platform = tags.iter().any(|t| t.contains("linux-amd64"));
        assert!(has_platform, "expected at least one linux-amd64 tag");
    }

    #[tokio::test]
    #[ignore]
    async fn list_tags_returns_error_for_nonexistent_package() {
        let c = client();
        let result = c
            .list_tags("ghcr.io/drasi-project/does-not-exist-test-xyz")
            .await;

        assert!(result.is_err(), "expected error for non-existent package");
        let err = format!("{:#}", result.unwrap_err());
        println!("non-existent package error: {err}");
    }

    /// Regression test: GHCR returns `{"tags": null}` on the second pagination
    /// page (when all tags fit in the first page). The oci-client crate cannot
    /// deserialize null into Vec<String>, so list_tags_all must handle it.
    #[tokio::test]
    #[ignore]
    async fn list_tags_handles_null_tags_from_ghcr_pagination() {
        let c = client();

        // The directory package has ~23 tags, well under the 1000 page size.
        // Page 1 returns all tags; page 2 returns {"tags": null}.
        // Before the fix, this would fail with:
        //   "invalid type: null, expected a sequence at line 1 column 58"
        let tags = c
            .list_tags("ghcr.io/drasi-project/drasi-plugin-directory")
            .await
            .unwrap();

        assert!(!tags.is_empty(), "expected directory tags");
        println!("directory package: {} tags", tags.len());

        // Verify the tags look like plugin directory entries (type.kind format)
        let has_dot_format = tags.iter().any(|t| t.contains('.'));
        assert!(
            has_dot_format,
            "expected tags in type.kind format, got: {:?}",
            &tags[..tags.len().min(5)]
        );
    }

    /// Verify that list_tags returns all tags, not just the first page.
    /// source/postgres has >100 tags (versions + .sig tags), which exceeds
    /// GHCR's default page size.
    #[tokio::test]
    #[ignore]
    async fn list_tags_fetches_all_pages() {
        let c = client();
        let tags = c.list_tags("source/postgres").await.unwrap();

        // As of writing, source/postgres has ~106 tags (above GHCR's default
        // 100-per-page). If pagination didn't work, we'd get at most 100.
        println!("source/postgres total tags: {}", tags.len());
        // Use a conservative threshold — the exact count will grow over time
        assert!(
            tags.len() > 20,
            "expected more than 20 tags, got {}",
            tags.len()
        );
    }

    // ── search_plugins ──────────────────────────────────────────────────

    /// This is the exact code path that `drasi-server plugin install-all` uses.
    /// It lists the drasi-plugin-directory package, then fetches version tags
    /// for each discovered plugin.
    #[tokio::test]
    #[ignore]
    async fn search_plugins_discovers_all_plugins() {
        let c = client();
        let results = c.search_plugins("*").await.unwrap();

        assert!(!results.is_empty(), "expected at least one plugin");
        println!("discovered {} plugins:", results.len());
        for r in &results {
            println!("  {} — {} versions", r.reference, r.versions.len());
        }

        // Should discover well-known plugin types
        let refs: Vec<&str> = results.iter().map(|r| r.reference.as_str()).collect();
        assert!(
            refs.contains(&"source/postgres"),
            "expected source/postgres in results"
        );
        assert!(
            refs.contains(&"reaction/log"),
            "expected reaction/log in results"
        );

        // Each plugin should have at least one version
        for r in &results {
            assert!(!r.versions.is_empty(), "{} has no versions", r.reference);
        }
    }

    #[tokio::test]
    #[ignore]
    async fn search_plugins_filters_by_kind() {
        let c = client();
        let results = c.search_plugins("postgres").await.unwrap();

        assert!(!results.is_empty(), "expected postgres matches");
        println!("'postgres' matches: {}", results.len());
        for r in &results {
            println!("  {}", r.reference);
        }

        // Every result should relate to postgres
        for r in &results {
            assert!(
                r.reference.contains("postgres"),
                "unexpected non-postgres result: {}",
                r.reference
            );
        }

        // Should include both source and bootstrap
        let refs: Vec<&str> = results.iter().map(|r| r.reference.as_str()).collect();
        assert!(refs.contains(&"source/postgres"));
        assert!(refs.contains(&"bootstrap/postgres"));
    }

    #[tokio::test]
    #[ignore]
    async fn search_plugins_exact_match() {
        let c = client();
        let results = c.search_plugins("source/postgres").await.unwrap();

        assert_eq!(results.len(), 1, "expected exactly one exact match");
        assert_eq!(results[0].reference, "source/postgres");
        println!("source/postgres: {} versions", results[0].versions.len());

        // Verify version info includes platform details
        for v in &results[0].versions {
            assert!(
                !v.platforms.is_empty(),
                "version {} has no platforms",
                v.version
            );
            println!("  v{} — {:?}", v.version, v.platforms);
        }
    }

    #[tokio::test]
    #[ignore]
    async fn search_plugins_no_match_returns_empty() {
        let c = client();
        let results = c
            .search_plugins("nonexistent-plugin-xyz-12345")
            .await
            .unwrap();

        assert!(results.is_empty(), "expected no matches");
    }

    // ── expand_reference ────────────────────────────────────────────────

    #[test]
    fn expand_reference_short_to_full() {
        let c = client();
        let full = c.expand_reference("source/postgres").unwrap();
        assert_eq!(full, "ghcr.io/drasi-project/source/postgres");
    }

    #[test]
    fn expand_reference_already_full() {
        let c = client();
        let full = c
            .expand_reference("ghcr.io/drasi-project/source/postgres")
            .unwrap();
        assert_eq!(full, "ghcr.io/drasi-project/source/postgres");
    }
}
