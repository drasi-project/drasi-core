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

//! Plugin fetcher supporting multiple URI schemes.
//!
//! Fetches plugin binaries from:
//! - `file://` — copies from local filesystem
//! - `http://` / `https://` — downloads via HTTP GET
//! - Bare references (no scheme) — delegates to OCI registry
//!
//! # Examples
//!
//! ```text
//! # Local file
//! file:///opt/drasi/plugins/libdrasi_source_custom.so
//!
//! # HTTP download
//! https://releases.example.com/plugins/libdrasi_source_custom.so
//!
//! # OCI registry (default)
//! source/postgres:0.1.8
//! ghcr.io/drasi-project/source/postgres:0.1.8
//! ```

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// The source type from which a plugin was fetched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginSourceType {
    /// Copied from a local file path.
    File,
    /// Downloaded via HTTP/HTTPS.
    Http,
    /// Pulled from an OCI registry.
    Oci,
}

/// Result of a successful plugin fetch operation.
#[derive(Debug, Clone)]
pub struct FetchedPlugin {
    /// Path to the plugin binary in the destination directory.
    pub path: PathBuf,
    /// Filename of the plugin binary.
    pub filename: String,
    /// How the plugin was obtained.
    pub source_type: PluginSourceType,
    /// The original URI/reference used to fetch the plugin.
    pub source_uri: String,
}

/// Parse a reference string and determine its source type.
pub fn parse_source_type(reference: &str) -> PluginSourceType {
    if reference.starts_with("file://") {
        PluginSourceType::File
    } else if reference.starts_with("http://") || reference.starts_with("https://") {
        PluginSourceType::Http
    } else {
        PluginSourceType::Oci
    }
}

/// Fetch a plugin from a `file://` URI.
///
/// Copies the file to `dest_dir`, preserving the original filename.
/// On Unix, sets executable permissions (0o755).
pub async fn fetch_from_file(uri: &str, dest_dir: &Path) -> Result<FetchedPlugin> {
    let path_str = uri.strip_prefix("file://").context("Invalid file:// URI")?;
    let source_path = PathBuf::from(path_str);

    if !source_path.exists() {
        bail!("Plugin file not found: {}", source_path.display());
    }
    if !source_path.is_file() {
        bail!("Not a file: {}", source_path.display());
    }

    let filename = source_path
        .file_name()
        .context("Cannot determine filename")?
        .to_string_lossy()
        .to_string();

    tokio::fs::create_dir_all(dest_dir).await?;
    let dest_path = dest_dir.join(&filename);

    tokio::fs::copy(&source_path, &dest_path)
        .await
        .with_context(|| {
            format!(
                "Failed to copy {} to {}",
                source_path.display(),
                dest_path.display()
            )
        })?;

    // Set executable permissions on Unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o755);
        tokio::fs::set_permissions(&dest_path, perms).await?;
    }

    // Also copy the sidecar metadata.json if it exists next to the source
    let metadata_source = source_path.with_extension("metadata.json");
    if metadata_source.exists() {
        let metadata_dest = dest_path.with_extension("metadata.json");
        let _ = tokio::fs::copy(&metadata_source, &metadata_dest).await;
    }

    log::info!("Copied plugin from {} to {}", uri, dest_path.display());

    Ok(FetchedPlugin {
        path: dest_path,
        filename,
        source_type: PluginSourceType::File,
        source_uri: uri.to_string(),
    })
}

/// Fetch a plugin from an HTTP/HTTPS URL.
///
/// Downloads the file to `dest_dir` using the filename from the URL path.
/// On Unix, sets executable permissions (0o755).
pub async fn fetch_from_http(url: &str, dest_dir: &Path) -> Result<FetchedPlugin> {
    let parsed = url::Url::parse(url).context("Invalid HTTP URL")?;
    let filename = parsed
        .path_segments()
        .and_then(|s| s.last())
        .filter(|s| !s.is_empty())
        .context("Cannot determine filename from URL")?
        .to_string();

    tokio::fs::create_dir_all(dest_dir).await?;
    let dest_path = dest_dir.join(&filename);

    log::info!("Downloading plugin from {}...", url);

    let response = reqwest::get(url)
        .await
        .with_context(|| format!("HTTP request failed for {url}"))?;

    if !response.status().is_success() {
        bail!(
            "HTTP {} for {}: {}",
            response.status().as_u16(),
            url,
            response.status().canonical_reason().unwrap_or("Unknown")
        );
    }

    let bytes = response
        .bytes()
        .await
        .context("Failed to read response body")?;

    tokio::fs::write(&dest_path, &bytes)
        .await
        .with_context(|| format!("Failed to write to {}", dest_path.display()))?;

    // Set executable permissions on Unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o755);
        tokio::fs::set_permissions(&dest_path, perms).await?;
    }

    let size_mb = bytes.len() as f64 / (1024.0 * 1024.0);
    log::info!(
        "Downloaded plugin ({:.1} MB) to {}",
        size_mb,
        dest_path.display()
    );

    Ok(FetchedPlugin {
        path: dest_path,
        filename,
        source_type: PluginSourceType::Http,
        source_uri: url.to_string(),
    })
}

/// Metadata extracted from a plugin binary's `drasi_plugin_metadata()` export.
#[derive(Debug, Clone, Default)]
pub struct PluginBinaryMetadata {
    /// Plugin's own version.
    pub plugin_version: String,
    /// drasi-plugin-sdk version.
    pub sdk_version: String,
    /// drasi-core version.
    pub core_version: String,
    /// drasi-lib version.
    pub lib_version: String,
    /// Rust target triple.
    pub target_triple: String,
}

/// Load a plugin binary just enough to read its metadata, then unload it.
///
/// Returns `None` if the binary cannot be loaded or doesn't export metadata.
pub fn read_plugin_metadata(path: &Path) -> Option<PluginBinaryMetadata> {
    use drasi_plugin_sdk::ffi::metadata::PluginMetadata;

    let lib = match unsafe { libloading::Library::new(path) } {
        Ok(lib) => lib,
        Err(e) => {
            log::warn!("Could not load plugin to read metadata: {e}");
            return None;
        }
    };

    let meta_fn = match unsafe {
        lib.get::<unsafe extern "C" fn() -> *const PluginMetadata>(b"drasi_plugin_metadata")
    } {
        Ok(f) => f,
        Err(_) => {
            log::warn!("Plugin does not export drasi_plugin_metadata");
            return None;
        }
    };

    let meta_ptr = unsafe { meta_fn() };
    if meta_ptr.is_null() {
        log::warn!("drasi_plugin_metadata returned null");
        return None;
    }

    let meta = unsafe { &*meta_ptr };
    Some(PluginBinaryMetadata {
        plugin_version: unsafe { meta.plugin_version.to_string() },
        sdk_version: unsafe { meta.sdk_version.to_string() },
        core_version: unsafe { meta.core_version.to_string() },
        lib_version: unsafe { meta.lib_version.to_string() },
        target_triple: unsafe { meta.target_triple.to_string() },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_source_type_file() {
        assert_eq!(
            parse_source_type("file:///path/to/plugin.so"),
            PluginSourceType::File
        );
    }

    #[test]
    fn test_parse_source_type_http() {
        assert_eq!(
            parse_source_type("https://example.com/plugin.so"),
            PluginSourceType::Http
        );
        assert_eq!(
            parse_source_type("http://example.com/plugin.so"),
            PluginSourceType::Http
        );
    }

    #[test]
    fn test_parse_source_type_oci() {
        assert_eq!(
            parse_source_type("source/postgres:0.1.8"),
            PluginSourceType::Oci
        );
        assert_eq!(
            parse_source_type("ghcr.io/drasi-project/source/postgres:0.1.8"),
            PluginSourceType::Oci
        );
        assert_eq!(parse_source_type("source/postgres"), PluginSourceType::Oci);
    }

    #[tokio::test]
    async fn test_fetch_from_file_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let result = fetch_from_file("file:///nonexistent/plugin.so", dir.path()).await;
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("not found"),
            "Should report file not found"
        );
    }

    #[tokio::test]
    async fn test_fetch_from_file_success() {
        let src_dir = tempfile::tempdir().unwrap();
        let dest_dir = tempfile::tempdir().unwrap();

        // Create a fake plugin file
        let src_file = src_dir.path().join("libdrasi_source_test.so");
        tokio::fs::write(&src_file, b"fake-plugin-binary")
            .await
            .unwrap();

        let uri = format!("file://{}", src_file.display());
        let result = fetch_from_file(&uri, dest_dir.path()).await.unwrap();

        assert_eq!(result.filename, "libdrasi_source_test.so");
        assert_eq!(result.source_type, PluginSourceType::File);
        assert!(result.path.exists());
        assert_eq!(
            tokio::fs::read(&result.path).await.unwrap(),
            b"fake-plugin-binary"
        );
    }

    #[tokio::test]
    async fn test_fetch_from_file_copies_metadata_json() {
        let src_dir = tempfile::tempdir().unwrap();
        let dest_dir = tempfile::tempdir().unwrap();

        let src_file = src_dir.path().join("libdrasi_source_test.so");
        tokio::fs::write(&src_file, b"fake-plugin").await.unwrap();
        let meta_file = src_dir.path().join("libdrasi_source_test.metadata.json");
        tokio::fs::write(&meta_file, b"{\"kind\":\"test\"}")
            .await
            .unwrap();

        let uri = format!("file://{}", src_file.display());
        let result = fetch_from_file(&uri, dest_dir.path()).await.unwrap();

        let dest_meta = dest_dir.path().join("libdrasi_source_test.metadata.json");
        assert!(dest_meta.exists(), "Sidecar metadata.json should be copied");
    }

    #[tokio::test]
    async fn test_fetch_from_http_invalid_url() {
        let dir = tempfile::tempdir().unwrap();
        let result = fetch_from_http("not-a-url", dir.path()).await;
        assert!(result.is_err());
    }
}
