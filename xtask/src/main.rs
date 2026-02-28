use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

#[derive(Deserialize)]
struct CargoMetadata {
    packages: Vec<Package>,
    target_directory: PathBuf,
    #[allow(dead_code)]
    workspace_root: PathBuf,
}

#[derive(Deserialize, Clone)]
struct Package {
    name: String,
    version: String,
    manifest_path: PathBuf,
    features: std::collections::HashMap<String, Vec<String>>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    license: Option<String>,
}

struct DiscoveryResult {
    plugins: Vec<PluginInfo>,
    target_directory: PathBuf,
    sdk_version: String,
    core_version: String,
    lib_version: String,
}

struct PluginInfo {
    package: Package,
    plugin_type: String,
    kind: String,
}

/// Metadata JSON written alongside each built plugin binary for OCI publishing.
#[derive(serde::Serialize, serde::Deserialize)]
struct PluginArtifactMetadata {
    name: String,
    kind: String,
    #[serde(rename = "type")]
    plugin_type: String,
    version: String,
    sdk_version: String,
    core_version: String,
    lib_version: String,
    target_triple: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    license: Option<String>,
}

/// Parse plugin type and kind from crate name.
/// e.g., "drasi-source-postgres" → ("source", "postgres")
///       "drasi-reaction-storedproc-mssql" → ("reaction", "storedproc-mssql")
///       "drasi-bootstrap-mssql" → ("bootstrap", "mssql")
fn parse_plugin_type_kind(crate_name: &str) -> Option<(String, String)> {
    let stripped = crate_name.strip_prefix("drasi-")?;
    for prefix in &["source-", "reaction-", "bootstrap-"] {
        if let Some(kind) = stripped.strip_prefix(prefix) {
            let plugin_type = prefix.trim_end_matches('-');
            return Some((plugin_type.to_string(), kind.to_string()));
        }
    }
    None
}

fn host_target_triple() -> String {
    let output = Command::new("rustc")
        .args(["-vV"])
        .output()
        .expect("failed to run rustc -vV");
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if let Some(triple) = line.strip_prefix("host: ") {
            return triple.trim().to_string();
        }
    }
    panic!("could not determine host target triple from rustc -vV");
}

/// Extract the OS portion from a target triple (e.g., "linux", "darwin", "windows").
fn extract_os(triple: &str) -> &str {
    let parts: Vec<&str> = triple.split('-').collect();
    // Triples: arch-vendor-os or arch-vendor-os-env
    // e.g., x86_64-unknown-linux-gnu, aarch64-apple-darwin, x86_64-pc-windows-gnu
    for part in &parts {
        match *part {
            "linux" | "darwin" | "windows" => return part,
            _ => {}
        }
    }
    "unknown"
}

fn discover_dynamic_plugins() -> DiscoveryResult {
    let output = Command::new("cargo")
        .args(["metadata", "--format-version", "1"])
        .output()
        .expect("failed to run cargo metadata");

    if !output.status.success() {
        eprintln!("{}", String::from_utf8_lossy(&output.stderr));
        std::process::exit(1);
    }

    let metadata: CargoMetadata =
        serde_json::from_slice(&output.stdout).expect("failed to parse cargo metadata");

    let sdk_version = metadata
        .packages
        .iter()
        .find(|p| p.name == "drasi-plugin-sdk")
        .map(|p| p.version.clone())
        .unwrap_or_else(|| "unknown".to_string());
    let core_version = metadata
        .packages
        .iter()
        .find(|p| p.name == "drasi-core")
        .map(|p| p.version.clone())
        .unwrap_or_else(|| "unknown".to_string());
    let lib_version = metadata
        .packages
        .iter()
        .find(|p| p.name == "drasi-lib")
        .map(|p| p.version.clone())
        .unwrap_or_else(|| "unknown".to_string());

    let plugins = metadata
        .packages
        .into_iter()
        .filter(|p| p.features.contains_key("dynamic-plugin"))
        .filter_map(|p| {
            let (plugin_type, kind) = parse_plugin_type_kind(&p.name)?;
            Some(PluginInfo {
                package: p,
                plugin_type,
                kind,
            })
        })
        .collect();

    DiscoveryResult {
        plugins,
        target_directory: metadata.target_directory,
        sdk_version,
        core_version,
        lib_version,
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    let subcommand = args.get(1).map(|s| s.as_str());

    match subcommand {
        Some("build-plugins") => build_plugins(&args[2..]),
        Some("list-plugins") => list_plugins(),
        Some("publish-plugins") => publish_plugins(&args[2..]),
        _ => {
            eprintln!("Usage: cargo xtask <command>");
            eprintln!();
            eprintln!("Commands:");
            eprintln!("  build-plugins [OPTIONS]    Build all dynamic plugins as cdylib shared libraries");
            eprintln!("  list-plugins               List all discovered dynamic plugin crates");
            eprintln!("  publish-plugins [OPTIONS]   Publish built plugins as OCI artifacts");
            eprintln!();
            eprintln!("build-plugins options:");
            eprintln!("  --release             Build in release mode");
            eprintln!("  --jobs N              Number of parallel jobs");
            eprintln!("  --target TRIPLE       Cross-compile target triple");
            eprintln!();
            eprintln!("publish-plugins options:");
            eprintln!("  --registry <URL>      OCI registry (default: ghcr.io/drasi-project)");
            eprintln!("  --plugins-dir <DIR>   Directory with built plugins (default: auto-detect)");
            eprintln!("  --release             Look in release build directory");
            eprintln!("  --target <TRIPLE>     Target triple for cross-compiled plugins");
            eprintln!("  --tag <TAG>           Override version tag for all plugins");
            eprintln!("  --pre-release <LABEL> Append pre-release label (e.g., dev.1 → 0.1.8-dev.1)");
            eprintln!("  --arch-suffix <SUFFIX> Append architecture suffix to tag (e.g., linux-amd64 → 0.1.8-linux-amd64)");
            eprintln!("  --dry-run             Show what would be published without pushing");
            std::process::exit(1);
        }
    }
}

fn list_plugins() {
    let result = discover_dynamic_plugins();
    if result.plugins.is_empty() {
        println!("No dynamic plugins found.");
        return;
    }
    println!("Dynamic plugins ({}):", result.plugins.len());
    println!(
        "  SDK: {}, Core: {}, Lib: {}",
        result.sdk_version, result.core_version, result.lib_version
    );
    println!();
    for p in &result.plugins {
        println!(
            "  {}/{} v{} ({})",
            p.plugin_type,
            p.kind,
            p.package.version,
            p.package.manifest_path.display()
        );
    }
}

fn parse_jobs(args: &[String]) -> usize {
    for (i, arg) in args.iter().enumerate() {
        if arg == "--jobs" || arg == "-j" {
            if let Some(n) = args.get(i + 1) {
                return n.parse().unwrap_or_else(|_| {
                    eprintln!("Invalid --jobs value: {}", n);
                    std::process::exit(1);
                });
            }
        }
    }
    thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

fn plugin_lib_name(crate_name: &str, target: Option<&str>) -> String {
    let base = crate_name.replace('-', "_");
    let is_windows = target
        .map(|t| t.contains("windows"))
        .unwrap_or(cfg!(target_os = "windows"));
    if is_windows {
        base
    } else {
        format!("lib{base}")
    }
}

fn plugin_lib_ext(target: Option<&str>) -> &'static str {
    let triple = target.unwrap_or("");
    if triple.contains("windows") {
        "dll"
    } else if triple.contains("apple") || triple.contains("darwin") {
        "dylib"
    } else if !triple.is_empty() {
        "so"
    } else if cfg!(target_os = "macos") {
        "dylib"
    } else if cfg!(target_os = "windows") {
        "dll"
    } else {
        "so"
    }
}

fn parse_target(args: &[String]) -> Option<String> {
    for (i, arg) in args.iter().enumerate() {
        if arg == "--target" {
            if let Some(t) = args.get(i + 1) {
                return Some(t.clone());
            }
        }
    }
    None
}

fn parse_flag_value(args: &[String], flag: &str) -> Option<String> {
    for (i, arg) in args.iter().enumerate() {
        if arg == flag {
            return args.get(i + 1).cloned();
        }
    }
    None
}

fn build_plugins(args: &[String]) {
    let release = args.iter().any(|a| a == "--release");
    let jobs = parse_jobs(args);
    let target = parse_target(args);
    let result = discover_dynamic_plugins();

    if result.plugins.is_empty() {
        println!("No dynamic plugins found.");
        return;
    }

    let mode = if release { "release" } else { "debug" };
    let target_dir = result.target_directory;

    let build_dir = match &target {
        Some(t) => target_dir.join(t).join(mode),
        None => target_dir.join(mode),
    };
    let plugins_dir = build_dir.join("plugins");

    let target_triple = target.clone().unwrap_or_else(host_target_triple);

    // Check if the target is buildable on this host
    if let Some(ref t) = target {
        let host = host_target_triple();
        let host_os = extract_os(&host);
        let target_os = extract_os(t);

        if host_os != target_os {
            // Cross-OS compilation requires `cross` + Docker, only works on Linux hosts
            if !host.contains("linux") {
                eprintln!(
                    "Cannot build {} on {} — cross-OS compilation requires a Linux host with `cross` + Docker.",
                    t, host
                );
                eprintln!("Skipping this target.");
                std::process::exit(1);
            }
        }
    }

    // Determine whether to use `cross` instead of `cargo`.
    // `cross` only works reliably on Linux hosts (it uses Docker with Linux containers).
    // On macOS/Windows hosts, use `cargo` — macOS cross-arch builds work via `rustup target add`.
    let use_cross = if let Some(ref t) = target {
        let host = host_target_triple();
        if t != &host && host.contains("linux") {
            // Only use cross on Linux hosts, and only if cross is installed
            Command::new("cross")
                .arg("--version")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        } else {
            false
        }
    } else {
        false
    };
    let build_tool = if use_cross { "cross" } else { "cargo" };

    println!(
        "=== Building {} cdylib plugins ({}{}, {}, {} parallel jobs) ===",
        result.plugins.len(),
        mode,
        target.as_ref().map(|t| format!(", {t}")).unwrap_or_default(),
        build_tool,
        jobs
    );

    let failed = Arc::new(AtomicBool::new(false));
    let target_dir = Arc::new(target_dir);
    let target = Arc::new(target);
    let build_tool_str = build_tool.to_string();
    let plugins: Vec<_> = result
        .plugins
        .iter()
        .map(|p| (p.package.name.clone(), p.package.manifest_path.clone()))
        .collect();

    if use_cross {
        // cross must run from workspace root using -p, and sequentially
        // (each invocation starts a Docker container)
        for (name, _manifest) in &plugins {
            if failed.load(Ordering::Relaxed) {
                break;
            }
            println!("  Building {}...", name);

            let feature_flag = format!("{}/dynamic-plugin", name);
            let mut cmd = Command::new(&build_tool_str);
            cmd.args(["build", "--lib", "-p", name, "--features", &feature_flag]);

            if let Some(t) = target.as_ref() {
                cmd.args(["--target", t]);
            }
            if release {
                cmd.arg("--release");
            }

            let status = cmd.status().expect("failed to run cross build");
            if !status.success() {
                eprintln!("Failed to build {}", name);
                failed.store(true, Ordering::Relaxed);
            } else {
                println!("  Built {}", name);
            }
        }
    } else {
        // cargo: parallel builds with --manifest-path
        let build_tool = Arc::new(build_tool_str);

        for chunk in plugins.chunks(jobs) {
            if failed.load(Ordering::Relaxed) {
                break;
            }

            let handles: Vec<_> = chunk
                .iter()
                .map(|(name, manifest)| {
                    let name = name.clone();
                    let manifest = manifest.clone();
                    let failed = Arc::clone(&failed);
                    let target_dir = Arc::clone(&target_dir);
                    let target = Arc::clone(&target);
                    let build_tool = Arc::clone(&build_tool);

                    thread::spawn(move || {
                        println!("  Building {}...", name);

                        let mut cmd = Command::new(build_tool.as_str());
                        cmd.args([
                            "build",
                            "--lib",
                            "--manifest-path",
                            manifest.to_str().expect("invalid manifest path"),
                            "--target-dir",
                            target_dir.to_str().expect("invalid target dir"),
                            "--features",
                            "dynamic-plugin",
                        ]);

                        if let Some(t) = target.as_ref() {
                            cmd.args(["--target", t]);
                        }

                        if release {
                            cmd.arg("--release");
                        }

                        let status = cmd.status().expect("failed to run build command");
                        if !status.success() {
                            eprintln!("Failed to build {}", name);
                            failed.store(true, Ordering::Relaxed);
                        } else {
                            println!("  Built {}", name);
                        }
                    })
                })
                .collect();

            for h in handles {
                h.join().expect("build thread panicked");
            }
        }
    }

    if failed.load(Ordering::Relaxed) {
        eprintln!("=== Plugin build failed ===");
        std::process::exit(1);
    }

    // Move plugin shared libraries to plugins/ subdirectory and generate metadata
    fs::create_dir_all(&plugins_dir).expect("failed to create plugins directory");

    let lib_ext = plugin_lib_ext(target.as_deref());

    for info in &result.plugins {
        let name = &info.package.name;
        let lib_name = plugin_lib_name(name, target.as_deref());
        let src = build_dir.join(format!("{lib_name}.{lib_ext}"));
        let dst = plugins_dir.join(format!("{lib_name}.{lib_ext}"));

        if src.exists() {
            fs::copy(&src, &dst).unwrap_or_else(|e| {
                eprintln!("Failed to copy {} to plugins/: {}", lib_name, e);
                0
            });
            let _ = fs::remove_file(&src);
        }

        // Generate metadata.json alongside the plugin binary
        let metadata = PluginArtifactMetadata {
            name: name.clone(),
            kind: info.kind.clone(),
            plugin_type: info.plugin_type.clone(),
            version: info.package.version.clone(),
            sdk_version: result.sdk_version.clone(),
            core_version: result.core_version.clone(),
            lib_version: result.lib_version.clone(),
            target_triple: target_triple.clone(),
            description: info.package.description.clone(),
            license: info.package.license.clone(),
        };
        let metadata_path = plugins_dir.join(format!("{lib_name}.metadata.json"));
        let metadata_json =
            serde_json::to_string_pretty(&metadata).expect("failed to serialize metadata");
        fs::write(&metadata_path, metadata_json).unwrap_or_else(|e| {
            eprintln!("Failed to write metadata for {}: {}", name, e);
        });

        clean_build_artifacts(&build_dir, &lib_name);
    }

    println!("=== cdylib plugins output to {} ===", plugins_dir.display());
}

fn clean_build_artifacts(build_dir: &Path, lib_name: &str) {
    let rlib = build_dir.join(format!("{lib_name}.rlib"));
    if rlib.exists() {
        let _ = fs::remove_file(&rlib);
    }

    let d_file = build_dir.join(format!("{lib_name}.d"));
    if d_file.exists() {
        let _ = fs::remove_file(&d_file);
    }

    let deps_dir = build_dir.join("deps");
    if deps_dir.is_dir() {
        if let Ok(entries) = fs::read_dir(&deps_dir) {
            for entry in entries.flatten() {
                let fname = entry.file_name();
                let fname = fname.to_string_lossy();
                if fname.starts_with(lib_name)
                    && (fname.ends_with(".rlib") || fname.ends_with(".d"))
                {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
    }
}

// ---------- OCI Publish ----------

const MEDIA_TYPE_PLUGIN_BINARY: &str = "application/vnd.drasi.plugin.v1+binary";
const MEDIA_TYPE_PLUGIN_METADATA: &str = "application/vnd.drasi.plugin.v1+metadata";
const MEDIA_TYPE_PLUGIN_CONFIG: &str = "application/vnd.drasi.plugin.v1+config";

const DEFAULT_REGISTRY: &str = "ghcr.io/drasi-project";

struct PublishablePlugin {
    metadata: PluginArtifactMetadata,
    binary_path: PathBuf,
    metadata_path: PathBuf,
}

fn discover_publishable_plugins(plugins_dir: &Path) -> Vec<PublishablePlugin> {
    let mut plugins = Vec::new();

    let entries = match fs::read_dir(plugins_dir) {
        Ok(entries) => entries,
        Err(e) => {
            eprintln!(
                "Failed to read plugins directory {}: {}",
                plugins_dir.display(),
                e
            );
            return plugins;
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.file_name().unwrap_or_default().to_string_lossy();

        if name.ends_with(".metadata.json") {
            let metadata_content = match fs::read_to_string(&path) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("Failed to read {}: {}", path.display(), e);
                    continue;
                }
            };

            let metadata: PluginArtifactMetadata = match serde_json::from_str(&metadata_content) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("Failed to parse {}: {}", path.display(), e);
                    continue;
                }
            };

            let stem = name.strip_suffix(".metadata.json").unwrap();
            let ext = if metadata.target_triple.contains("windows") {
                "dll"
            } else if metadata.target_triple.contains("apple")
                || metadata.target_triple.contains("darwin")
            {
                "dylib"
            } else {
                "so"
            };
            let binary_path = plugins_dir.join(format!("{stem}.{ext}"));

            if !binary_path.exists() {
                eprintln!(
                    "Warning: binary not found for {}: expected {}",
                    name,
                    binary_path.display()
                );
                continue;
            }

            plugins.push(PublishablePlugin {
                metadata,
                binary_path,
                metadata_path: path,
            });
        }
    }

    plugins.sort_by(|a, b| a.metadata.name.cmp(&b.metadata.name));
    plugins
}

fn publish_plugins(args: &[String]) {
    let registry = parse_flag_value(args, "--registry")
        .unwrap_or_else(|| DEFAULT_REGISTRY.to_string());
    let tag_override = parse_flag_value(args, "--tag");
    let pre_release = parse_flag_value(args, "--pre-release");
    let arch_suffix = parse_flag_value(args, "--arch-suffix");
    let dry_run = args.iter().any(|a| a == "--dry-run");

    if tag_override.is_some() && pre_release.is_some() {
        eprintln!("Error: --tag and --pre-release are mutually exclusive");
        std::process::exit(1);
    }

    // Determine plugins directory
    let plugins_dir = if let Some(dir) = parse_flag_value(args, "--plugins-dir") {
        PathBuf::from(dir)
    } else {
        let release = args.iter().any(|a| a == "--release");
        let target = parse_target(args);
        let mode = if release { "release" } else { "debug" };

        let target_dir = PathBuf::from("target");
        match &target {
            Some(t) => target_dir.join(t).join(mode).join("plugins"),
            None => target_dir.join(mode).join("plugins"),
        }
    };

    if !plugins_dir.exists() {
        eprintln!(
            "Plugins directory does not exist: {}\nRun 'cargo xtask build-plugins' first.",
            plugins_dir.display()
        );
        std::process::exit(1);
    }

    let plugins = discover_publishable_plugins(&plugins_dir);
    if plugins.is_empty() {
        eprintln!("No publishable plugins found in {}", plugins_dir.display());
        std::process::exit(1);
    }

    println!(
        "=== Publishing {} plugins to {} ===",
        plugins.len(),
        registry
    );
    if let Some(ref label) = pre_release {
        println!("  Pre-release label: {}", label);
    }

    for p in &plugins {
        let tag = make_tag(&p.metadata.version, tag_override.as_deref(), pre_release.as_deref(), arch_suffix.as_deref());
        let reference = format!(
            "{}/{}/{}:{}",
            registry, p.metadata.plugin_type, p.metadata.kind, tag
        );
        println!(
            "  {}/{} v{} ({}) → {}",
            p.metadata.plugin_type,
            p.metadata.kind,
            p.metadata.version,
            p.metadata.target_triple,
            reference
        );
    }

    if dry_run {
        println!("\n=== Dry run — no artifacts pushed ===");
        return;
    }

    let username = std::env::var("OCI_REGISTRY_USERNAME").unwrap_or_default();
    let password = std::env::var("OCI_REGISTRY_PASSWORD")
        .or_else(|_| std::env::var("GHCR_TOKEN"))
        .unwrap_or_default();

    if password.is_empty() {
        eprintln!(
            "Error: OCI_REGISTRY_PASSWORD or GHCR_TOKEN env var required for authentication"
        );
        std::process::exit(1);
    }

    let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");

    rt.block_on(async {
        let client_config = oci_client::client::ClientConfig {
            protocol: oci_client::client::ClientProtocol::Https,
            ..Default::default()
        };
        let client = oci_client::Client::new(client_config);

        let auth = if username.is_empty() {
            oci_client::secrets::RegistryAuth::Basic(String::new(), password.clone())
        } else {
            oci_client::secrets::RegistryAuth::Basic(username.clone(), password.clone())
        };

        let mut success_count = 0;
        let mut fail_count = 0;

        for p in &plugins {
            let tag = make_tag(
                &p.metadata.version,
                tag_override.as_deref(),
                pre_release.as_deref(),
                arch_suffix.as_deref(),
            );
            let reference_str = format!(
                "{}/{}/{}:{}",
                registry, p.metadata.plugin_type, p.metadata.kind, tag
            );

            match publish_single_plugin(&client, &auth, &reference_str, p).await {
                Ok(url) => {
                    println!("  ✓ {} → {}", reference_str, url);
                    success_count += 1;
                }
                Err(e) => {
                    eprintln!("  ✗ {} — {}", reference_str, e);
                    fail_count += 1;
                }
            }
        }

        println!(
            "\n=== Published: {} succeeded, {} failed ===",
            success_count, fail_count
        );

        // Update plugin directory with entries for each successfully published plugin
        if success_count > 0 {
            println!("\n=== Updating plugin directory ===");
            let mut dir_entries: Vec<(String, String)> = plugins
                .iter()
                .map(|p| (p.metadata.plugin_type.clone(), p.metadata.kind.clone()))
                .collect();
            dir_entries.sort();
            dir_entries.dedup();

            for (ptype, kind) in &dir_entries {
                let dir_tag = format!("{}.{}", ptype, kind);
                let dir_ref = format!(
                    "{}/drasi-plugin-directory:{}",
                    registry, dir_tag
                );
                match publish_directory_entry(&client, &auth, &dir_ref).await {
                    Ok(_) => println!("  ✓ directory entry: {}", dir_tag),
                    Err(e) => eprintln!("  ✗ directory entry: {} — {}", dir_tag, e),
                }
            }
        }

        if fail_count > 0 {
            std::process::exit(1);
        }
    });
}

/// Build the OCI tag from the plugin version, optional override, pre-release label, and arch suffix.
fn make_tag(version: &str, tag_override: Option<&str>, pre_release: Option<&str>, arch_suffix: Option<&str>) -> String {
    let base = if let Some(tag) = tag_override {
        tag.to_string()
    } else if let Some(label) = pre_release {
        format!("{}-{}", version, label)
    } else {
        version.to_string()
    };
    match arch_suffix {
        Some(suffix) => format!("{}-{}", base, suffix),
        None => base,
    }
}

async fn publish_single_plugin(
    client: &oci_client::Client,
    auth: &oci_client::secrets::RegistryAuth,
    reference_str: &str,
    plugin: &PublishablePlugin,
) -> Result<String, Box<dyn std::error::Error>> {
    use oci_client::client::{Config, ImageLayer};

    let reference: oci_client::Reference = reference_str.parse()?;

    let binary_data = fs::read(&plugin.binary_path)?;
    let binary_size = binary_data.len();

    let metadata_json = fs::read(&plugin.metadata_path)?;

    let binary_layer = ImageLayer::new(
        bytes::Bytes::from(binary_data),
        MEDIA_TYPE_PLUGIN_BINARY.to_string(),
        None,
    );

    let metadata_layer = ImageLayer::new(
        bytes::Bytes::from(metadata_json),
        MEDIA_TYPE_PLUGIN_METADATA.to_string(),
        None,
    );

    let layers = vec![binary_layer, metadata_layer];

    let config = Config::new(
        bytes::Bytes::from(b"{}".to_vec()),
        MEDIA_TYPE_PLUGIN_CONFIG.to_string(),
        None,
    );

    let mut annotations = BTreeMap::new();
    annotations.insert(
        "org.opencontainers.image.title".to_string(),
        plugin.metadata.name.clone(),
    );
    annotations.insert(
        "org.opencontainers.image.version".to_string(),
        plugin.metadata.version.clone(),
    );
    annotations.insert(
        "io.drasi.plugin.kind".to_string(),
        plugin.metadata.kind.clone(),
    );
    annotations.insert(
        "io.drasi.plugin.type".to_string(),
        plugin.metadata.plugin_type.clone(),
    );
    annotations.insert(
        "io.drasi.plugin.sdk-version".to_string(),
        plugin.metadata.sdk_version.clone(),
    );
    annotations.insert(
        "io.drasi.plugin.core-version".to_string(),
        plugin.metadata.core_version.clone(),
    );
    annotations.insert(
        "io.drasi.plugin.lib-version".to_string(),
        plugin.metadata.lib_version.clone(),
    );
    annotations.insert(
        "io.drasi.plugin.target-triple".to_string(),
        plugin.metadata.target_triple.clone(),
    );

    let manifest =
        oci_client::manifest::OciImageManifest::build(&layers, &config, Some(annotations));

    let response = client
        .push(&reference, &layers, config, auth, Some(manifest))
        .await?;

    println!(
        "    Pushed {} ({:.1} MB)",
        plugin.metadata.name,
        binary_size as f64 / 1_048_576.0
    );

    Ok(response.manifest_url)
}

/// Publish a zero-byte directory entry to the plugin directory package.
/// Each entry is a tag like "source.postgres" on the drasi-plugin-directory package.
async fn publish_directory_entry(
    client: &oci_client::Client,
    auth: &oci_client::secrets::RegistryAuth,
    reference_str: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    use oci_client::client::{Config, ImageLayer};

    let reference: oci_client::Reference = reference_str.parse()?;

    // Minimal non-empty layer (GHCR rejects zero-length blobs)
    let layer = ImageLayer::new(
        bytes::Bytes::from_static(b"{}"),
        "application/vnd.drasi.plugin.directory.v1".to_string(),
        None,
    );

    let config = Config::new(
        bytes::Bytes::from(b"{}".to_vec()),
        "application/vnd.oci.image.config.v1+json".to_string(),
        None,
    );

    let manifest =
        oci_client::manifest::OciImageManifest::build(&[layer.clone()], &config, None);

    let response = client
        .push(&reference, &[layer], config, auth, Some(manifest))
        .await?;

    Ok(response.manifest_url)
}

// ---------- Platform Mapping ----------

/// Map a Rust target triple to an OCI platform (os, architecture).
#[allow(dead_code)]
fn triple_to_platform(triple: &str) -> Option<(String, String)> {
    let arch = if triple.contains("x86_64") {
        "amd64"
    } else if triple.contains("aarch64") {
        "arm64"
    } else if triple.contains("armv7") {
        "arm"
    } else {
        return None;
    };

    let os = if triple.contains("linux") {
        "linux"
    } else if triple.contains("windows") {
        "windows"
    } else if triple.contains("darwin") || triple.contains("apple") {
        "darwin"
    } else {
        return None;
    };

    Some((os.to_string(), arch.to_string()))
}

/// Map a target triple to the arch-suffix format used in tags (e.g., "linux-amd64").
#[allow(dead_code)]
fn triple_to_arch_suffix(triple: &str) -> Option<String> {
    triple_to_platform(triple).map(|(os, arch)| format!("{}-{}", os, arch))
}
