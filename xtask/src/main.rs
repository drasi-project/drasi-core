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

#![allow(clippy::print_stdout, clippy::print_stderr)]

use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Deserialize)]
struct CargoMetadata {
    packages: Vec<Package>,
    target_directory: PathBuf,
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
    #[serde(default)]
    publish: Option<Vec<String>>,
    #[serde(default)]
    dependencies: Vec<Dependency>,
}

#[derive(Deserialize, Clone)]
struct Dependency {
    name: String,
    req: String,
    kind: Option<String>,
    path: Option<PathBuf>,
}

struct DiscoveryResult {
    plugins: Vec<PluginInfo>,
    build_batches: Vec<Vec<String>>,
    target_directory: PathBuf,
    workspace_root: PathBuf,
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
    for prefix in &[
        "source-",
        "reaction-",
        "bootstrap-",
        "identity-",
        "secret-store-",
    ] {
        if let Some(kind) = stripped.strip_prefix(prefix) {
            let plugin_type = prefix.trim_end_matches('-');
            return Some((plugin_type.to_string(), kind.to_string()));
        }
    }
    None
}

fn is_dynamic_plugin(package: &Package) -> bool {
    package.features.contains_key("dynamic-plugin")
        && parse_plugin_type_kind(&package.name).is_some()
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

fn load_cargo_metadata(no_deps: bool) -> CargoMetadata {
    let mut command = Command::new("cargo");
    command.args(["metadata", "--format-version", "1"]);
    if no_deps {
        command.arg("--no-deps");
    }

    let output = command.output().expect("failed to run cargo metadata");

    if !output.status.success() {
        eprintln!("{}", String::from_utf8_lossy(&output.stderr));
        std::process::exit(1);
    }

    serde_json::from_slice(&output.stdout).expect("failed to parse cargo metadata")
}

fn discover_dynamic_plugins() -> DiscoveryResult {
    let metadata = load_cargo_metadata(false);

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

    let plugin_names: BTreeSet<String> = metadata
        .packages
        .iter()
        .filter(|p| is_dynamic_plugin(p))
        .map(|p| p.name.clone())
        .collect();
    let build_batches = plugin_build_batches(&metadata.packages, &plugin_names);

    let plugins = metadata
        .packages
        .into_iter()
        .filter(is_dynamic_plugin)
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
        build_batches,
        target_directory: metadata.target_directory,
        workspace_root: metadata.workspace_root,
        sdk_version,
        core_version,
        lib_version,
    }
}

fn is_publishable(package: &Package) -> bool {
    match &package.publish {
        Some(registries) => !registries.is_empty(),
        None => true,
    }
}

fn find_dependency_path(
    graph: &BTreeMap<String, BTreeSet<String>>,
    start: &str,
    target: &str,
) -> Option<Vec<String>> {
    let mut pending = vec![(start.to_string(), vec![start.to_string()])];
    let mut visited = BTreeSet::new();

    while let Some((current, path)) = pending.pop() {
        if current == target {
            return Some(path);
        }
        if !visited.insert(current.clone()) {
            continue;
        }

        if let Some(dependencies) = graph.get(&current) {
            for dependency in dependencies.iter().rev() {
                let mut next_path = path.clone();
                next_path.push(dependency.clone());
                pending.push((dependency.clone(), next_path));
            }
        }
    }

    None
}

/// Groups plugins that can share a Cargo invocation without duplicate FFI exports.
///
/// Cargo unifies features across a build graph. If dependency-connected plugins
/// enable `dynamic-plugin` together, both can export the same `drasi_plugin_*`
/// symbols. Normal and build dependency paths therefore split batches; dev
/// dependencies do not participate in `cargo build --lib` and are ignored.
fn plugin_build_batches(packages: &[Package], plugin_names: &BTreeSet<String>) -> Vec<Vec<String>> {
    let mut graph: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for package in packages {
        let dependencies = graph.entry(package.name.clone()).or_default();
        dependencies.extend(
            package
                .dependencies
                .iter()
                .filter(|dependency| dependency.path.is_some())
                .filter(|dependency| {
                    matches!(
                        dependency.kind.as_deref(),
                        None | Some("normal") | Some("build")
                    )
                })
                .map(|dependency| dependency.name.clone()),
        );
    }

    let conflicts = |left: &str, right: &str| {
        find_dependency_path(&graph, left, right).is_some()
            || find_dependency_path(&graph, right, left).is_some()
    };

    let mut batches: Vec<Vec<String>> = Vec::new();
    for plugin in plugin_names {
        if let Some(batch) = batches
            .iter_mut()
            .find(|batch| batch.iter().all(|other| !conflicts(plugin, other)))
        {
            batch.push(plugin.clone());
        } else {
            batches.push(vec![plugin.clone()]);
        }
    }
    batches
}

fn versioned_dev_dependency_cycles(packages: &[Package]) -> Vec<String> {
    let publishable: BTreeSet<String> = packages
        .iter()
        .filter(|package| is_publishable(package))
        .map(|package| package.name.clone())
        .collect();

    let mut normal_graph: BTreeMap<String, BTreeSet<String>> = publishable
        .iter()
        .map(|name| (name.clone(), BTreeSet::new()))
        .collect();

    for package in packages
        .iter()
        .filter(|package| publishable.contains(&package.name))
    {
        let dependencies = normal_graph
            .get_mut(&package.name)
            .expect("publishable package should be in dependency graph");
        for dependency in &package.dependencies {
            if dependency.path.is_some()
                && publishable.contains(&dependency.name)
                && matches!(
                    dependency.kind.as_deref(),
                    None | Some("normal") | Some("build")
                )
            {
                dependencies.insert(dependency.name.clone());
            }
        }
    }

    let mut cycles = Vec::new();
    for package in packages
        .iter()
        .filter(|package| publishable.contains(&package.name))
    {
        for dependency in &package.dependencies {
            if dependency.kind.as_deref() != Some("dev")
                || dependency.path.is_none()
                || dependency.req == "*"
                || !publishable.contains(&dependency.name)
            {
                continue;
            }

            if let Some(return_path) =
                find_dependency_path(&normal_graph, &dependency.name, &package.name)
            {
                cycles.push(format!(
                    "{} --dev {}--> {}",
                    package.name,
                    dependency.req,
                    return_path.join(" -> ")
                ));
            }
        }
    }

    cycles.sort();
    cycles.dedup();
    cycles
}

fn check_publish_dependency_cycles() {
    let metadata = load_cargo_metadata(true);
    let cycles = versioned_dev_dependency_cycles(&metadata.packages);
    if cycles.is_empty() {
        println!("No versioned dev-dependency cycles found in publishable packages.");
        return;
    }

    eprintln!("Versioned dev-dependency cycles found in publishable packages:");
    for cycle in cycles {
        eprintln!("  {cycle}");
    }
    eprintln!(
        "Move cross-crate tests to a publish-false test crate or use a path-only dev-dependency."
    );
    std::process::exit(1);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    let subcommand = args.get(1).map(|s| s.as_str());

    match subcommand {
        Some("build-plugins") => build_plugins(&args[2..]),
        Some("check-publish-dependency-cycles") => check_publish_dependency_cycles(),
        Some("list-plugins") => list_plugins(),
        Some("publish-plugins") => publish_plugins(&args[2..]),
        _ => {
            eprintln!("Usage: cargo xtask <command>");
            eprintln!();
            eprintln!("Commands:");
            eprintln!(
                "  build-plugins [OPTIONS]    Build all dynamic plugins as cdylib shared libraries"
            );
            eprintln!(
                "  check-publish-dependency-cycles  Check publishable packages for dev-dependency cycles"
            );
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
            eprintln!(
                "  --plugins-dir <DIR>   Directory with built plugins (default: auto-detect)"
            );
            eprintln!("  --release             Look in release build directory");
            eprintln!("  --target <TRIPLE>     Target triple for cross-compiled plugins");
            eprintln!("  --tag <TAG>           Override version tag for all plugins");
            eprintln!(
                "  --pre-release <LABEL> Append pre-release label (e.g., dev.1 → 0.1.8-dev.1)"
            );
            eprintln!("  --arch-suffix <SUFFIX> Append architecture suffix to tag (e.g., linux-amd64 → 0.1.8-linux-amd64)");
            eprintln!("  --dry-run             Show what would be published without pushing");
            eprintln!("  --sign                Sign each published artifact with cosign (requires cosign in PATH)");
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
                    eprintln!("Invalid --jobs value: {n}");
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

/// Strip a trailing glibc-version suffix from a `cargo-zigbuild` target triple.
///
/// `cargo-zigbuild` accepts targets like `x86_64-unknown-linux-gnu.2.28` to pin
/// the glibc floor, but it still writes artifacts to the base-triple directory
/// (`target/x86_64-unknown-linux-gnu/...`) and Rust/OCI metadata should record
/// the canonical triple. This returns the base triple with any `.<major>.<minor>`
/// glibc suffix removed; non-glibc targets are returned unchanged.
fn strip_glibc_suffix(target: &str) -> &str {
    // Only `*-linux-gnu` targets carry a glibc version suffix.
    if let Some(idx) = target.find("-linux-gnu.") {
        // Keep everything up to and including "-linux-gnu".
        let end = idx + "-linux-gnu".len();
        &target[..end]
    } else {
        target
    }
}

/// Returns true when the target triple carries a `cargo-zigbuild` glibc-version
/// suffix (e.g. `x86_64-unknown-linux-gnu.2.28`), which requires building with
/// `cargo zigbuild` rather than plain `cargo`/`cross`.
fn has_glibc_suffix(target: &str) -> bool {
    target.contains("-linux-gnu.")
}

/// Build a `Command` from a build-tool string, splitting on whitespace so that
/// multi-word tools like `"cargo zigbuild"` become the program `cargo` with a
/// leading `zigbuild` subcommand argument.
fn build_command(build_tool: &str) -> Command {
    let mut parts = build_tool.split_whitespace();
    let program = parts.next().unwrap_or("cargo");
    let mut cmd = Command::new(program);
    for extra in parts {
        cmd.arg(extra);
    }
    cmd
}

struct PluginBuildOptions<'a> {
    build_tool: &'a str,
    use_zigbuild: bool,
    workspace_root: &'a Path,
    target_directory: &'a Path,
    plugins: &'a [String],
    target: Option<&'a str>,
    release: bool,
    jobs: usize,
}

fn plugin_build_command(options: PluginBuildOptions<'_>) -> Command {
    let mut cmd = build_command(options.build_tool);
    cmd.current_dir(options.workspace_root);
    if !options.use_zigbuild {
        cmd.arg("build");
    }
    cmd.arg("--lib");
    for plugin in options.plugins {
        cmd.args(["-p", plugin]);
    }

    let features = options
        .plugins
        .iter()
        .map(|name| format!("{name}/dynamic-plugin"))
        .collect::<Vec<_>>()
        .join(",");
    cmd.args(["--features", &features]);

    let target_directory = options
        .target_directory
        .strip_prefix(options.workspace_root)
        .unwrap_or(options.target_directory);
    cmd.arg("--target-dir").arg(target_directory);

    if let Some(target) = options.target {
        cmd.args(["--target", target]);
    }
    if options.release {
        cmd.arg("--release");
    }
    cmd.args(["--jobs", &options.jobs.to_string()]);
    cmd
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
    // The build target may carry a `cargo-zigbuild` glibc-version suffix
    // (e.g. `x86_64-unknown-linux-gnu.2.28`). That suffix is passed through to
    // the build tool to pin the glibc floor, but artifact paths and metadata use
    // the canonical base triple.
    let build_target = parse_target(args);
    let target = build_target
        .as_deref()
        .map(|t| strip_glibc_suffix(t).to_string());
    let use_zigbuild = build_target
        .as_deref()
        .map(has_glibc_suffix)
        .unwrap_or(false);
    let result = discover_dynamic_plugins();

    if result.plugins.is_empty() {
        println!("No dynamic plugins found.");
        return;
    }

    let mode = if release { "release" } else { "debug" };
    let target_dir = result.target_directory.clone();

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
                    "Cannot build {t} on {host} — cross-OS compilation requires a Linux host with `cross` + Docker."
                );
                eprintln!("Skipping this target.");
                std::process::exit(1);
            }
        }
    }

    // Determine whether to use `cross` instead of `cargo`.
    // `cross` only works reliably on Linux hosts (it uses Docker with Linux containers).
    // On macOS/Windows hosts, use `cargo` — macOS cross-arch builds work via `rustup target add`.
    // When a glibc-pinned target is requested we use `cargo zigbuild` instead of
    // either, which sets a deterministic glibc floor for the whole link.
    let use_cross = if use_zigbuild {
        false
    } else if let Some(ref t) = target {
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
    let build_tool = if use_zigbuild {
        "cargo zigbuild"
    } else if use_cross {
        "cross"
    } else {
        "cargo"
    };

    println!(
        "=== Building {} cdylib plugins ({}{}, {}, {} parallel jobs) ===",
        result.plugins.len(),
        mode,
        target
            .as_ref()
            .map(|t| format!(", {t}"))
            .unwrap_or_default(),
        build_tool,
        jobs
    );

    // The value passed to `--target` on the build command. For zigbuild this is
    // the glibc-suffixed triple; otherwise it matches the canonical `target`.
    let cmd_target = build_target.clone().or_else(|| target.clone());
    println!(
        "=== Split into {} dependency-safe Cargo batches ===",
        result.build_batches.len()
    );
    for (index, batch) in result.build_batches.iter().enumerate() {
        println!(
            "=== Building batch {}/{} ({} plugins) ===",
            index + 1,
            result.build_batches.len(),
            batch.len()
        );
        let mut cmd = plugin_build_command(PluginBuildOptions {
            build_tool,
            use_zigbuild,
            workspace_root: &result.workspace_root,
            target_directory: &result.target_directory,
            plugins: batch,
            target: cmd_target.as_deref(),
            release,
            jobs,
        });
        if !cmd.status().expect("failed to run plugin build").success() {
            eprintln!("=== Plugin build failed in batch {} ===", index + 1);
            std::process::exit(1);
        }
    }

    // Move plugin shared libraries to plugins/ subdirectory and generate metadata
    fs::create_dir_all(&plugins_dir).expect("failed to create plugins directory");

    let lib_ext = plugin_lib_ext(target.as_deref());
    let mut missing_binaries = Vec::new();

    for info in &result.plugins {
        let name = &info.package.name;
        let lib_name = plugin_lib_name(name, target.as_deref());
        let src = build_dir.join(format!("{lib_name}.{lib_ext}"));
        let dst = plugins_dir.join(format!("{lib_name}.{lib_ext}"));

        if src.exists() {
            fs::copy(&src, &dst).unwrap_or_else(|e| {
                eprintln!("Failed to copy {lib_name} to plugins/: {e}");
                0
            });
            let _ = fs::remove_file(&src);
        } else {
            eprintln!(
                "ERROR: expected cdylib not found after build: {}",
                src.display()
            );
            missing_binaries.push(name.clone());
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
            eprintln!("Failed to write metadata for {name}: {e}");
        });

        clean_build_artifacts(&build_dir, &lib_name);
    }

    if !missing_binaries.is_empty() {
        eprintln!(
            "\n=== {} of {} plugin binaries missing after build ===",
            missing_binaries.len(),
            result.plugins.len()
        );
        for name in &missing_binaries {
            eprintln!("  - {name}");
        }
        eprintln!("\nContents of build directory ({}):", build_dir.display());
        match fs::read_dir(&build_dir) {
            Ok(entries) => {
                let mut found_any = false;
                for entry in entries.flatten() {
                    let fname = entry.file_name();
                    let fname = fname.to_string_lossy();
                    if fname.ends_with(".so")
                        || fname.ends_with(".dll")
                        || fname.ends_with(".dylib")
                        || fname.ends_with(".rlib")
                    {
                        eprintln!("  {fname}");
                        found_any = true;
                    }
                }
                if !found_any {
                    eprintln!("  (no library files found)");
                }
            }
            Err(e) => eprintln!("  Failed to list directory: {e}"),
        }
        std::process::exit(1);
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

            let stem = name
                .strip_suffix(".metadata.json")
                .expect("filename must end with .metadata.json");
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
    let registry =
        parse_flag_value(args, "--registry").unwrap_or_else(|| DEFAULT_REGISTRY.to_string());
    let tag_override = parse_flag_value(args, "--tag");
    let pre_release = parse_flag_value(args, "--pre-release");
    let arch_suffix = parse_flag_value(args, "--arch-suffix");
    let dry_run = args.iter().any(|a| a == "--dry-run");
    let sign = args.iter().any(|a| a == "--sign");

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

    // Auto-detect arch suffix from plugin metadata if not provided
    let arch_suffix = arch_suffix.or_else(|| {
        plugins
            .first()
            .and_then(|p| triple_to_arch_suffix(&p.metadata.target_triple))
    });

    if arch_suffix.is_none() {
        eprintln!(
            "Warning: no --arch-suffix provided and could not auto-detect from plugin metadata."
        );
        eprintln!("Tags will not include a platform suffix. Use --arch-suffix to specify one.");
    }

    println!(
        "=== Publishing {} plugins to {} ===",
        plugins.len(),
        registry
    );
    if let Some(ref label) = pre_release {
        println!("  Pre-release label: {label}");
    }

    for p in &plugins {
        let tag = make_tag(
            &p.metadata.version,
            tag_override.as_deref(),
            pre_release.as_deref(),
            arch_suffix.as_deref(),
        );
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
        eprintln!("Error: OCI_REGISTRY_PASSWORD or GHCR_TOKEN env var required for authentication");
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

        let mut publish_result = PublishBatchResult::default();

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
                    publish_result.record_pushed_artifact(
                        sign,
                        &reference_str,
                        &p.metadata.target_triple,
                        &url,
                        cosign_sign_and_verify,
                    );
                }
                Err(e) => {
                    eprintln!("  ✗ {reference_str} — {e}");
                    publish_result.record_failure();
                }
            }
        }

        println!(
            "\n=== Published: {} succeeded, {} failed ===",
            publish_result.success_count, publish_result.fail_count
        );

        // Update plugin directory with entries for each successfully published plugin
        if publish_result.should_update_directory() {
            println!("\n=== Updating plugin directory ===");
            let mut dir_entries: Vec<(String, String)> = plugins
                .iter()
                .map(|p| (p.metadata.plugin_type.clone(), p.metadata.kind.clone()))
                .collect();
            dir_entries.sort();
            dir_entries.dedup();

            for (ptype, kind) in &dir_entries {
                let dir_tag = format!("{ptype}.{kind}");
                let dir_ref = format!("{registry}/drasi-plugin-directory:{dir_tag}");
                match publish_directory_entry(&client, &auth, &dir_ref).await {
                    Ok(_) => println!("  ✓ directory entry: {dir_tag}"),
                    Err(e) => eprintln!("  ✗ directory entry: {dir_tag} — {e}"),
                }
            }
        } else if publish_result.has_failures() {
            eprintln!("\n=== Skipping plugin directory update because publishing had failures ===");
        }

        if publish_result.has_failures() {
            std::process::exit(1);
        }
    });
}

#[derive(Debug, Default, PartialEq, Eq)]
struct PublishBatchResult {
    success_count: usize,
    fail_count: usize,
}

impl PublishBatchResult {
    fn record_pushed_artifact<F>(
        &mut self,
        sign: bool,
        reference: &str,
        target_triple: &str,
        manifest_url: &str,
        sign_and_verify: F,
    ) -> bool
    where
        F: FnOnce(&str) -> Result<(), String>,
    {
        if sign {
            let digest_ref = digest_reference(reference, manifest_url);
            if let Err(e) = sign_and_verify(&digest_ref) {
                eprintln!("  ✗ {reference} ({target_triple}) — {e}");
                self.record_failure();
                return false;
            }
        }

        println!("  ✓ {reference} → {manifest_url}");
        self.success_count += 1;
        true
    }

    fn record_failure(&mut self) {
        self.fail_count += 1;
    }

    fn has_failures(&self) -> bool {
        self.fail_count > 0
    }

    fn should_update_directory(&self) -> bool {
        self.success_count > 0 && !self.has_failures()
    }
}

fn digest_reference(reference: &str, manifest_url: &str) -> String {
    if let Some((_, digest)) = manifest_url.rsplit_once("/manifests/") {
        let repo = reference.split(':').next().unwrap_or(reference);
        format!("{repo}@{digest}")
    } else {
        reference.to_string()
    }
}

/// Build the OCI tag from the plugin version, optional override, pre-release label, and arch suffix.
fn make_tag(
    version: &str,
    tag_override: Option<&str>,
    pre_release: Option<&str>,
    arch_suffix: Option<&str>,
) -> String {
    let base = if let Some(tag) = tag_override {
        tag.to_string()
    } else if let Some(label) = pre_release {
        format!("{version}-{label}")
    } else {
        version.to_string()
    };
    match arch_suffix {
        Some(suffix) => format!("{base}-{suffix}"),
        None => base,
    }
}

/// Sign and verify an OCI artifact with cosign after publishing.
///
/// Uses cosign keyless signing by default. A successful publish requires both
/// the signing command and the verification postcondition to pass.
///
/// Supports:
/// - Keyless mode (default): uses ambient OIDC credentials (GitHub Actions, etc.)
/// - Key-based mode: set `COSIGN_KEY` env var to a private key path or KMS reference
/// - Public-key verification override: set `COSIGN_VERIFY_KEY` to a public key path or KMS reference
///
/// Returns an error on failure so the publish batch exits non-zero.
fn cosign_sign_and_verify(reference: &str) -> Result<(), String> {
    let max_attempts = env_usize("COSIGN_MAX_ATTEMPTS", 3).max(1);
    let retry_delay = Duration::from_secs(env_u64("COSIGN_RETRY_DELAY_SECONDS", 10));

    cosign_sign_and_verify_with_program(reference, "cosign", max_attempts, retry_delay)
}

fn cosign_sign_and_verify_with_program(
    reference: &str,
    program: &str,
    max_attempts: usize,
    retry_delay: Duration,
) -> Result<(), String> {
    cosign_sign_with_program(reference, program, max_attempts, retry_delay)?;
    cosign_verify_with_program(reference, program, max_attempts, retry_delay)
}

fn cosign_sign_with_program(
    reference: &str,
    program: &str,
    max_attempts: usize,
    retry_delay: Duration,
) -> Result<(), String> {
    run_cosign_with_retries("signing", reference, max_attempts, retry_delay, || {
        let mut cmd = Command::new(program);
        cmd.arg("sign").arg("--yes").arg(reference);
        append_cosign_key_arg(&mut cmd);
        cmd
    })
}

const DEFAULT_COSIGN_CERTIFICATE_IDENTITY: &str =
    "https://github.com/drasi-project/drasi-core/.github/workflows/publish-plugins.yml@refs/heads/main";
const DEFAULT_COSIGN_CERTIFICATE_OIDC_ISSUER: &str = "https://token.actions.githubusercontent.com";

struct CosignVerifyArgs {
    args: Vec<String>,
    cleanup_path: Option<PathBuf>,
}

impl Drop for CosignVerifyArgs {
    fn drop(&mut self) {
        if let Some(path) = &self.cleanup_path {
            let _ = fs::remove_file(path);
        }
    }
}

fn cosign_verify_with_program(
    reference: &str,
    program: &str,
    max_attempts: usize,
    retry_delay: Duration,
) -> Result<(), String> {
    let verify_args = cosign_verify_args(program)?;
    run_cosign_with_retries(
        "verifying signature",
        reference,
        max_attempts,
        retry_delay,
        || {
            let mut cmd = Command::new(program);
            cmd.arg("verify");
            cmd.args(&verify_args.args);
            cmd.arg(reference);
            cmd
        },
    )
}

fn cosign_verify_args(program: &str) -> Result<CosignVerifyArgs, String> {
    if let Ok(key) = std::env::var("COSIGN_VERIFY_KEY") {
        return Ok(CosignVerifyArgs {
            args: vec!["--key".to_string(), key],
            cleanup_path: None,
        });
    }

    if let Ok(key) = std::env::var("COSIGN_KEY") {
        if cosign_key_supports_direct_verify(&key) {
            return Ok(CosignVerifyArgs {
                args: vec!["--key".to_string(), key],
                cleanup_path: None,
            });
        }

        let public_key = derive_cosign_public_key(program, &key)?;
        let public_key_path = write_temp_cosign_public_key(&public_key)?;
        return Ok(CosignVerifyArgs {
            args: vec![
                "--key".to_string(),
                public_key_path.to_string_lossy().into_owned(),
            ],
            cleanup_path: Some(public_key_path),
        });
    }

    let issuer = std::env::var("COSIGN_VERIFY_CERTIFICATE_OIDC_ISSUER")
        .unwrap_or_else(|_| DEFAULT_COSIGN_CERTIFICATE_OIDC_ISSUER.to_string());
    if let Ok(identity_regexp) = std::env::var("COSIGN_VERIFY_CERTIFICATE_IDENTITY_REGEXP") {
        Ok(CosignVerifyArgs {
            args: vec![
                "--certificate-identity-regexp".to_string(),
                identity_regexp,
                "--certificate-oidc-issuer".to_string(),
                issuer,
            ],
            cleanup_path: None,
        })
    } else {
        let identity = std::env::var("COSIGN_VERIFY_CERTIFICATE_IDENTITY")
            .unwrap_or_else(|_| DEFAULT_COSIGN_CERTIFICATE_IDENTITY.to_string());
        Ok(CosignVerifyArgs {
            args: vec![
                "--certificate-identity".to_string(),
                identity,
                "--certificate-oidc-issuer".to_string(),
                issuer,
            ],
            cleanup_path: None,
        })
    }
}

fn derive_cosign_public_key(program: &str, key: &str) -> Result<String, String> {
    let output = Command::new(program)
        .arg("public-key")
        .arg("--key")
        .arg(key)
        .output()
        .map_err(|e| format!("failed to derive cosign public key: {e}"))?;

    if !output.status.success() {
        return Err(format!(
            "failed to derive cosign public key: {}",
            command_error_message(&output)
        ));
    }

    let public_key = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if public_key.is_empty() {
        return Err("failed to derive cosign public key: empty public key output".to_string());
    }

    Ok(public_key)
}

fn write_temp_cosign_public_key(public_key: &str) -> Result<PathBuf, String> {
    let mut path = std::env::temp_dir();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| format!("failed to create temporary cosign public key path: {e}"))?
        .as_nanos();
    path.push(format!(
        "drasi-cosign-public-key-{}-{now}.pem",
        std::process::id()
    ));
    fs::write(&path, public_key)
        .map_err(|e| format!("failed to write temporary cosign public key: {e}"))?;
    Ok(path)
}

fn cosign_key_supports_direct_verify(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    [
        "awskms://",
        "azurekms://",
        "gcpkms://",
        "hashivault://",
        "k8s://",
        "pkcs11://",
    ]
    .iter()
    .any(|prefix| key.starts_with(prefix))
}

fn run_cosign_with_retries<F>(
    action: &str,
    reference: &str,
    max_attempts: usize,
    retry_delay: Duration,
    mut build_command: F,
) -> Result<(), String>
where
    F: FnMut() -> Command,
{
    let max_attempts = max_attempts.max(1);
    let mut last_error = String::new();

    for attempt in 1..=max_attempts {
        print!("  🔏 {action} {reference} (attempt {attempt}/{max_attempts})...");
        let _ = std::io::Write::flush(&mut std::io::stdout());

        match build_command().output() {
            Ok(output) if output.status.success() => {
                println!(" ✓");
                return Ok(());
            }
            Ok(output) => {
                last_error = command_error_message(&output);
                eprintln!(" ✗ {last_error}");
                if action == "signing"
                    && (last_error.contains("expired_token")
                        || last_error.contains("retrieving ID token"))
                {
                    eprintln!("    hint: keyless signing requires GitHub Actions OIDC or `cosign login`. For local signing, set COSIGN_KEY=path/to/key.pem");
                }
            }
            Err(e) => {
                last_error = if e.kind() == std::io::ErrorKind::NotFound
                    || e.kind() == std::io::ErrorKind::PermissionDenied
                {
                    "cosign not found in PATH (install: https://docs.sigstore.dev/cosign/system_config/installation/)".to_string()
                } else {
                    format!("failed to run cosign: {e}")
                };
                eprintln!(" ✗ {last_error}");
            }
        }

        if attempt < max_attempts {
            eprintln!(
                "    retrying {action} for {reference} in {}s",
                retry_delay.as_secs()
            );
            thread::sleep(retry_delay);
        }
    }

    Err(format!(
        "{action} failed for {reference} after {max_attempts} attempt(s): {last_error}"
    ))
}

fn command_error_message(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !stderr.is_empty() {
        return stderr;
    }

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !stdout.is_empty() {
        return stdout;
    }

    format!("cosign exited with {}", output.status)
}

fn append_cosign_key_arg(cmd: &mut Command) {
    if let Ok(key) = std::env::var("COSIGN_KEY") {
        cmd.arg("--key").arg(key);
    }
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
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
        oci_client::manifest::OciImageManifest::build(std::slice::from_ref(&layer), &config, None);

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
/// Musl triples get a distinct suffix (e.g., "linux-musl-amd64").
fn triple_to_arch_suffix(triple: &str) -> Option<String> {
    triple_to_platform(triple).map(|(os, arch)| {
        if triple.contains("musl") {
            format!("{os}-musl-{arch}")
        } else {
            format!("{os}-{arch}")
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn package(name: &str, publishable: bool, dependencies: Vec<Dependency>) -> Package {
        Package {
            name: name.to_string(),
            version: "0.1.0".to_string(),
            manifest_path: PathBuf::from(format!("{name}/Cargo.toml")),
            features: std::collections::HashMap::new(),
            description: None,
            license: None,
            publish: if publishable { None } else { Some(Vec::new()) },
            dependencies,
        }
    }

    fn dependency(name: &str, req: &str, kind: Option<&str>) -> Dependency {
        Dependency {
            name: name.to_string(),
            req: req.to_string(),
            kind: kind.map(str::to_string),
            path: Some(PathBuf::from(name)),
        }
    }

    struct EnvGuard {
        saved: Vec<(&'static str, Option<String>)>,
    }

    impl EnvGuard {
        fn set(vars: &[(&'static str, &str)], unset: &[&'static str]) -> Self {
            let mut saved = Vec::new();
            for name in vars
                .iter()
                .map(|(name, _)| *name)
                .chain(unset.iter().copied())
            {
                if saved.iter().any(|(saved_name, _)| saved_name == &name) {
                    continue;
                }
                saved.push((name, std::env::var(name).ok()));
            }
            for name in unset {
                std::env::remove_var(name);
            }
            for (name, value) in vars {
                std::env::set_var(name, value);
            }
            Self { saved }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (name, value) in self.saved.drain(..).rev() {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
    }

    fn test_temp_dir(name: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after Unix epoch")
            .as_nanos();
        dir.push(format!("drasi-xtask-{name}-{}-{now}", std::process::id()));
        fs::create_dir_all(&dir).expect("test temp dir should be created");
        dir
    }

    #[cfg(unix)]
    fn mock_cosign_script(dir: &Path, body: &str) -> PathBuf {
        let script = dir.join("cosign-mock.sh");
        fs::write(&script, body).expect("mock cosign script should be written");
        let mut perms = fs::metadata(&script)
            .expect("mock cosign script metadata should be readable")
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script, perms).expect("mock cosign script should be executable");
        script
    }

    #[cfg(unix)]
    fn read_calls(path: &Path) -> String {
        fs::read_to_string(path).unwrap_or_default()
    }

    #[test]
    fn detects_versioned_dev_dependency_cycle() {
        let packages = vec![
            package(
                "drasi-lib",
                true,
                vec![dependency("drasi-plugin-sdk", "^0.11.0", Some("dev"))],
            ),
            package(
                "drasi-plugin-sdk",
                true,
                vec![dependency("drasi-lib", "^0.9.0", None)],
            ),
        ];

        assert_eq!(
            versioned_dev_dependency_cycles(&packages),
            ["drasi-lib --dev ^0.11.0--> drasi-plugin-sdk -> drasi-lib"]
        );
    }

    #[test]
    fn ignores_path_only_dev_dependency_cycle() {
        let packages = vec![
            package(
                "drasi-lib",
                true,
                vec![dependency("drasi-plugin-sdk", "*", Some("dev"))],
            ),
            package(
                "drasi-plugin-sdk",
                true,
                vec![dependency("drasi-lib", "^0.9.0", None)],
            ),
        ];

        assert!(versioned_dev_dependency_cycles(&packages).is_empty());
    }

    #[test]
    fn ignores_cycles_between_unpublished_packages() {
        let packages = vec![
            package(
                "drasi-source-open511",
                false,
                vec![dependency("drasi-bootstrap-open511", "^0.1.0", Some("dev"))],
            ),
            package(
                "drasi-bootstrap-open511",
                false,
                vec![dependency("drasi-source-open511", "^0.1.1", None)],
            ),
        ];

        assert!(versioned_dev_dependency_cycles(&packages).is_empty());
    }

    #[test]
    fn ignores_acyclic_versioned_dev_dependency() {
        let packages = vec![
            package(
                "drasi-source",
                true,
                vec![dependency("drasi-bootstrap", "^0.1.0", Some("dev"))],
            ),
            package("drasi-bootstrap", true, Vec::new()),
        ];

        assert!(versioned_dev_dependency_cycles(&packages).is_empty());
    }

    #[test]
    fn detects_return_path_through_build_dependency() {
        let packages = vec![
            package(
                "drasi-lib",
                true,
                vec![dependency("drasi-plugin-sdk", "^0.11.0", Some("dev"))],
            ),
            package(
                "drasi-plugin-sdk",
                true,
                vec![dependency("codegen", "^1.0.0", Some("build"))],
            ),
            package(
                "codegen",
                true,
                vec![dependency("drasi-lib", "^0.9.0", None)],
            ),
        ];

        assert_eq!(
            versioned_dev_dependency_cycles(&packages),
            ["drasi-lib --dev ^0.11.0--> drasi-plugin-sdk -> codegen -> drasi-lib"]
        );
    }

    #[test]
    fn strips_glibc_suffix_from_gnu_triples() {
        assert_eq!(
            strip_glibc_suffix("x86_64-unknown-linux-gnu.2.28"),
            "x86_64-unknown-linux-gnu"
        );
        assert_eq!(
            strip_glibc_suffix("aarch64-unknown-linux-gnu.2.17"),
            "aarch64-unknown-linux-gnu"
        );
    }

    #[test]
    fn leaves_non_suffixed_triples_unchanged() {
        for t in [
            "x86_64-unknown-linux-gnu",
            "aarch64-unknown-linux-musl",
            "x86_64-pc-windows-gnu",
            "aarch64-apple-darwin",
        ] {
            assert_eq!(strip_glibc_suffix(t), t);
        }
    }

    #[test]
    fn detects_glibc_suffix() {
        assert!(has_glibc_suffix("x86_64-unknown-linux-gnu.2.28"));
        assert!(has_glibc_suffix("aarch64-unknown-linux-gnu.2.17"));

        assert!(!has_glibc_suffix("x86_64-unknown-linux-gnu"));
        assert!(!has_glibc_suffix("x86_64-unknown-linux-musl"));
        assert!(!has_glibc_suffix("x86_64-pc-windows-gnu"));
    }

    #[test]
    fn build_command_splits_multiword_tools() {
        // Single-word tool: program only, no leading args.
        let cargo = build_command("cargo");
        assert_eq!(cargo.get_program(), "cargo");
        assert_eq!(cargo.get_args().count(), 0);

        // Multi-word tool: program + subcommand arg.
        let zig = build_command("cargo zigbuild");
        assert_eq!(zig.get_program(), "cargo");
        let args: Vec<_> = zig.get_args().collect();
        assert_eq!(args, ["zigbuild"]);
    }

    #[cfg(unix)]
    #[test]
    fn cosign_sign_command_failure_is_error() {
        let _lock = env_lock();
        let dir = test_temp_dir("sign-failure");
        let calls = dir.join("calls");
        let script = mock_cosign_script(
            &dir,
            r#"#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >> "$MOCK_COSIGN_CALLS"
if [[ "$1" == "sign" ]]; then
    echo "signing failed" >&2
    exit 42
fi
echo "unexpected command: $*" >&2
exit 2
"#,
        );
        let reference =
            "ghcr.io/drasi-project/reaction/sse@sha256:60c70c051c53c423caa9e60d55f09d01fbdea853ff202450412bdc06cf262ad5";

        let _env = EnvGuard::set(
            &[("MOCK_COSIGN_CALLS", calls.to_str().unwrap())],
            &["COSIGN_KEY", "COSIGN_VERIFY_KEY"],
        );
        let error = cosign_sign_with_program(
            reference,
            script.to_str().unwrap(),
            1,
            std::time::Duration::from_secs(0),
        )
        .expect_err("failed signing command should fail publication");

        assert!(error.contains("signing failed"));
        assert!(error.contains(reference));
        assert_eq!(read_calls(&calls), format!("sign --yes {reference}\n"));
        fs::remove_dir_all(dir).expect("test temp dir should be removed");
    }

    #[cfg(unix)]
    #[test]
    fn cosign_verify_command_failure_is_error() {
        let _lock = env_lock();
        let dir = test_temp_dir("verify-failure");
        let calls = dir.join("calls");
        let script = mock_cosign_script(
            &dir,
            r#"#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >> "$MOCK_COSIGN_CALLS"
case "$1" in
    sign)
        exit 0
        ;;
    verify)
        echo "verification failed" >&2
        exit 43
        ;;
esac
echo "unexpected command: $*" >&2
exit 2
"#,
        );
        let reference = "ghcr.io/drasi-project/reaction/sse@sha256:abc";

        let _env = EnvGuard::set(
            &[("MOCK_COSIGN_CALLS", calls.to_str().unwrap())],
            &[
                "COSIGN_KEY",
                "COSIGN_VERIFY_KEY",
                "COSIGN_VERIFY_CERTIFICATE_IDENTITY",
                "COSIGN_VERIFY_CERTIFICATE_IDENTITY_REGEXP",
                "COSIGN_VERIFY_CERTIFICATE_OIDC_ISSUER",
            ],
        );
        let error = cosign_sign_and_verify_with_program(
            reference,
            script.to_str().unwrap(),
            1,
            std::time::Duration::from_secs(0),
        )
        .expect_err("failed verification should fail publication");

        assert!(error.contains("verification failed"));
        assert!(read_calls(&calls)
            .contains("sign --yes ghcr.io/drasi-project/reaction/sse@sha256:abc\n"));
        assert!(read_calls(&calls).contains("verify --certificate-identity https://github.com/drasi-project/drasi-core/.github/workflows/publish-plugins.yml@refs/heads/main --certificate-oidc-issuer https://token.actions.githubusercontent.com ghcr.io/drasi-project/reaction/sse@sha256:abc\n"));
        fs::remove_dir_all(dir).expect("test temp dir should be removed");
    }

    #[cfg(unix)]
    #[test]
    fn cosign_retry_exhaustion_retries_signing() {
        let _lock = env_lock();
        let dir = test_temp_dir("retry-exhaustion");
        let calls = dir.join("calls");
        let script = mock_cosign_script(
            &dir,
            r#"#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >> "$MOCK_COSIGN_CALLS"
echo "still failing" >&2
exit 44
"#,
        );

        let _env = EnvGuard::set(
            &[("MOCK_COSIGN_CALLS", calls.to_str().unwrap())],
            &["COSIGN_KEY", "COSIGN_VERIFY_KEY"],
        );
        let error = cosign_sign_with_program(
            "ghcr.io/drasi-project/reaction/sse@sha256:abc",
            script.to_str().unwrap(),
            2,
            std::time::Duration::from_secs(0),
        )
        .expect_err("retry exhaustion should fail");

        assert!(error.contains("after 2 attempt(s)"));
        assert_eq!(read_calls(&calls).lines().count(), 2);
        fs::remove_dir_all(dir).expect("test temp dir should be removed");
    }

    #[cfg(unix)]
    #[test]
    fn cosign_transient_signing_failure_recovers() {
        let _lock = env_lock();
        let dir = test_temp_dir("transient-recovery");
        let calls = dir.join("calls");
        let sign_count = dir.join("sign-count");
        let script = mock_cosign_script(
            &dir,
            r#"#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >> "$MOCK_COSIGN_CALLS"
case "$1" in
    sign)
        count=0
        if [[ -f "$MOCK_SIGN_COUNT" ]]; then
            count="$(cat "$MOCK_SIGN_COUNT")"
        fi
        count=$((count + 1))
        printf '%s\n' "$count" > "$MOCK_SIGN_COUNT"
        if [[ "$count" -eq 1 ]]; then
            echo "transient signing error" >&2
            exit 45
        fi
        exit 0
        ;;
    verify)
        exit 0
        ;;
esac
echo "unexpected command: $*" >&2
exit 2
"#,
        );

        let _env = EnvGuard::set(
            &[
                ("MOCK_COSIGN_CALLS", calls.to_str().unwrap()),
                ("MOCK_SIGN_COUNT", sign_count.to_str().unwrap()),
            ],
            &[
                "COSIGN_KEY",
                "COSIGN_VERIFY_KEY",
                "COSIGN_VERIFY_CERTIFICATE_IDENTITY",
                "COSIGN_VERIFY_CERTIFICATE_IDENTITY_REGEXP",
                "COSIGN_VERIFY_CERTIFICATE_OIDC_ISSUER",
            ],
        );
        cosign_sign_and_verify_with_program(
            "ghcr.io/drasi-project/reaction/sse@sha256:abc",
            script.to_str().unwrap(),
            3,
            std::time::Duration::from_secs(0),
        )
        .expect("transient signing failure should recover");

        let calls = read_calls(&calls);
        assert_eq!(
            calls
                .lines()
                .filter(|line| line.starts_with("sign "))
                .count(),
            2
        );
        assert_eq!(
            calls
                .lines()
                .filter(|line| line.starts_with("verify "))
                .count(),
            1
        );
        fs::remove_dir_all(dir).expect("test temp dir should be removed");
    }

    #[cfg(unix)]
    #[test]
    fn cosign_successful_keyless_verify_uses_exact_default_identity() {
        let _lock = env_lock();
        let dir = test_temp_dir("keyless-default");
        let calls = dir.join("calls");
        let script = mock_cosign_script(
            &dir,
            r#"#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >> "$MOCK_COSIGN_CALLS"
case "$1" in
    sign)
        exit 0
        ;;
    verify)
        expected="verify --certificate-identity https://github.com/drasi-project/drasi-core/.github/workflows/publish-plugins.yml@refs/heads/main --certificate-oidc-issuer https://token.actions.githubusercontent.com ghcr.io/drasi-project/reaction/sse@sha256:abc"
        if [[ "$*" != "$expected" ]]; then
            echo "unexpected keyless policy: $*" >&2
            exit 46
        fi
        exit 0
        ;;
esac
echo "unexpected command: $*" >&2
exit 2
"#,
        );

        let _env = EnvGuard::set(
            &[("MOCK_COSIGN_CALLS", calls.to_str().unwrap())],
            &[
                "COSIGN_KEY",
                "COSIGN_VERIFY_KEY",
                "COSIGN_VERIFY_CERTIFICATE_IDENTITY",
                "COSIGN_VERIFY_CERTIFICATE_IDENTITY_REGEXP",
                "COSIGN_VERIFY_CERTIFICATE_OIDC_ISSUER",
            ],
        );
        cosign_sign_and_verify_with_program(
            "ghcr.io/drasi-project/reaction/sse@sha256:abc",
            script.to_str().unwrap(),
            1,
            std::time::Duration::from_secs(0),
        )
        .expect("exact default keyless policy should verify");

        assert!(read_calls(&calls).contains("--certificate-identity https://github.com/drasi-project/drasi-core/.github/workflows/publish-plugins.yml@refs/heads/main"));
        fs::remove_dir_all(dir).expect("test temp dir should be removed");
    }

    #[cfg(unix)]
    #[test]
    fn cosign_unexpected_keyless_policy_is_rejected() {
        let _lock = env_lock();
        let dir = test_temp_dir("keyless-reject");
        let calls = dir.join("calls");
        let script = mock_cosign_script(
            &dir,
            r#"#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >> "$MOCK_COSIGN_CALLS"
case "$1" in
    sign)
        exit 0
        ;;
    verify)
        if [[ "$*" == *"@refs/heads/main"* ]]; then
            exit 0
        fi
        echo "unexpected workflow identity" >&2
        exit 47
        ;;
esac
echo "unexpected command: $*" >&2
exit 2
"#,
        );

        let _env = EnvGuard::set(
            &[
                ("MOCK_COSIGN_CALLS", calls.to_str().unwrap()),
                (
                    "COSIGN_VERIFY_CERTIFICATE_IDENTITY",
                    "https://github.com/drasi-project/drasi-core/.github/workflows/publish-plugins.yml@refs/heads/feature",
                ),
            ],
            &[
                "COSIGN_KEY",
                "COSIGN_VERIFY_KEY",
                "COSIGN_VERIFY_CERTIFICATE_IDENTITY_REGEXP",
                "COSIGN_VERIFY_CERTIFICATE_OIDC_ISSUER",
            ],
        );
        let error = cosign_sign_and_verify_with_program(
            "ghcr.io/drasi-project/reaction/sse@sha256:abc",
            script.to_str().unwrap(),
            1,
            std::time::Duration::from_secs(0),
        )
        .expect_err("unexpected workflow/ref should be rejected by verification");

        assert!(error.contains("unexpected workflow identity"));
        assert!(read_calls(&calls).contains("@refs/heads/feature"));
        fs::remove_dir_all(dir).expect("test temp dir should be removed");
    }

    #[cfg(unix)]
    #[test]
    fn cosign_local_private_key_derives_public_key_for_verify() {
        let _lock = env_lock();
        let dir = test_temp_dir("local-key");
        let calls = dir.join("calls");
        let private_key = dir.join("cosign.key");
        fs::write(&private_key, "PRIVATE").expect("private key should be written");
        let script = mock_cosign_script(
            &dir,
            r#"#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >> "$MOCK_COSIGN_CALLS"
case "$1" in
    sign)
        if [[ "$2" != "--yes" || "$4" != "--key" || "$5" != "$MOCK_PRIVATE_KEY" ]]; then
            echo "unexpected signing key args: $*" >&2
            exit 48
        fi
        exit 0
        ;;
    public-key)
        if [[ "$2" != "--key" || "$3" != "$MOCK_PRIVATE_KEY" ]]; then
            echo "unexpected public-key args: $*" >&2
            exit 49
        fi
        echo "PUBLIC"
        exit 0
        ;;
    verify)
        if [[ "$2" != "--key" || "$3" == "$MOCK_PRIVATE_KEY" ]]; then
            echo "verify did not use a derived public key file: $*" >&2
            exit 50
        fi
        grep -q "PUBLIC" "$3"
        exit 0
        ;;
esac
echo "unexpected command: $*" >&2
exit 2
"#,
        );

        let _env = EnvGuard::set(
            &[
                ("MOCK_COSIGN_CALLS", calls.to_str().unwrap()),
                ("MOCK_PRIVATE_KEY", private_key.to_str().unwrap()),
                ("COSIGN_KEY", private_key.to_str().unwrap()),
            ],
            &[
                "COSIGN_VERIFY_KEY",
                "COSIGN_VERIFY_CERTIFICATE_IDENTITY",
                "COSIGN_VERIFY_CERTIFICATE_IDENTITY_REGEXP",
                "COSIGN_VERIFY_CERTIFICATE_OIDC_ISSUER",
            ],
        );
        cosign_sign_and_verify_with_program(
            "ghcr.io/drasi-project/reaction/sse@sha256:abc",
            script.to_str().unwrap(),
            1,
            std::time::Duration::from_secs(0),
        )
        .expect("local private key should derive a public key for verification");

        let calls = read_calls(&calls);
        assert!(calls.contains("public-key --key"));
        assert!(calls.contains("verify --key"));
        fs::remove_dir_all(dir).expect("test temp dir should be removed");
    }

    #[cfg(unix)]
    #[test]
    fn cosign_kms_key_is_used_directly_for_verify() {
        let _lock = env_lock();
        let dir = test_temp_dir("kms-key");
        let calls = dir.join("calls");
        let script = mock_cosign_script(
            &dir,
            r#"#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >> "$MOCK_COSIGN_CALLS"
case "$1" in
    sign)
        exit 0
        ;;
    public-key)
        echo "KMS key should not derive a local public key" >&2
        exit 51
        ;;
    verify)
        if [[ "$2" != "--key" || "$3" != "awskms://example/key" ]]; then
            echo "verify did not use KMS key directly: $*" >&2
            exit 52
        fi
        exit 0
        ;;
esac
echo "unexpected command: $*" >&2
exit 2
"#,
        );

        let _env = EnvGuard::set(
            &[
                ("MOCK_COSIGN_CALLS", calls.to_str().unwrap()),
                ("COSIGN_KEY", "awskms://example/key"),
            ],
            &[
                "COSIGN_VERIFY_KEY",
                "COSIGN_VERIFY_CERTIFICATE_IDENTITY",
                "COSIGN_VERIFY_CERTIFICATE_IDENTITY_REGEXP",
                "COSIGN_VERIFY_CERTIFICATE_OIDC_ISSUER",
            ],
        );
        cosign_sign_and_verify_with_program(
            "ghcr.io/drasi-project/reaction/sse@sha256:abc",
            script.to_str().unwrap(),
            1,
            std::time::Duration::from_secs(0),
        )
        .expect("KMS key should be verified directly");

        let calls = read_calls(&calls);
        assert!(!calls.contains("public-key"));
        assert!(calls.contains("verify --key awskms://example/key"));
        fs::remove_dir_all(dir).expect("test temp dir should be removed");
    }

    #[test]
    fn signing_failure_is_not_counted_successful_and_skips_directory_update() {
        let mut result = PublishBatchResult::default();
        let ok = result.record_pushed_artifact(
            true,
            "ghcr.io/drasi-project/reaction/sse:0.3.8-darwin-arm64",
            "aarch64-apple-darwin",
            "https://ghcr.io/v2/drasi-project/reaction/sse/manifests/sha256:abc",
            |reference| {
                assert_eq!(reference, "ghcr.io/drasi-project/reaction/sse@sha256:abc");
                Err("signing failed".to_string())
            },
        );

        assert!(!ok);
        assert_eq!(
            result,
            PublishBatchResult {
                success_count: 0,
                fail_count: 1
            }
        );
        assert!(result.has_failures());
        assert!(!result.should_update_directory());
    }

    #[test]
    fn signed_artifact_success_allows_directory_update() {
        let mut result = PublishBatchResult::default();
        let ok = result.record_pushed_artifact(
            true,
            "ghcr.io/drasi-project/reaction/sse:0.3.8-darwin-arm64",
            "aarch64-apple-darwin",
            "https://ghcr.io/v2/drasi-project/reaction/sse/manifests/sha256:abc",
            |_| Ok(()),
        );

        assert!(ok);
        assert_eq!(
            result,
            PublishBatchResult {
                success_count: 1,
                fail_count: 0
            }
        );
        assert!(!result.has_failures());
        assert!(result.should_update_directory());
    }

    #[test]
    fn plugin_build_command_batches_packages_and_features() {
        let plugins = vec![
            "drasi-source-http".to_string(),
            "drasi-reaction-http".to_string(),
        ];
        let command = plugin_build_command(PluginBuildOptions {
            build_tool: "cargo zigbuild",
            use_zigbuild: true,
            workspace_root: Path::new("/workspace"),
            target_directory: Path::new("/workspace/target"),
            plugins: &plugins,
            target: Some("x86_64-unknown-linux-gnu.2.28"),
            release: true,
            jobs: 8,
        });
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();

        assert_eq!(
            args,
            [
                "zigbuild",
                "--lib",
                "-p",
                "drasi-source-http",
                "-p",
                "drasi-reaction-http",
                "--features",
                "drasi-source-http/dynamic-plugin,drasi-reaction-http/dynamic-plugin",
                "--target-dir",
                "target",
                "--target",
                "x86_64-unknown-linux-gnu.2.28",
                "--release",
                "--jobs",
                "8",
            ]
        );
        assert_eq!(command.get_current_dir(), Some(Path::new("/workspace")));
    }

    #[test]
    fn plugin_build_command_supports_plain_cargo_and_cross() {
        let plugins = vec!["drasi-source-http".to_string()];
        for build_tool in ["cargo", "cross"] {
            let command = plugin_build_command(PluginBuildOptions {
                build_tool,
                use_zigbuild: false,
                workspace_root: Path::new("/workspace"),
                target_directory: Path::new("/cache/drasi-target"),
                plugins: &plugins,
                target: None,
                release: false,
                jobs: 4,
            });
            let args: Vec<_> = command
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect();

            assert_eq!(command.get_program(), build_tool);
            assert_eq!(
                args,
                [
                    "build",
                    "--lib",
                    "-p",
                    "drasi-source-http",
                    "--features",
                    "drasi-source-http/dynamic-plugin",
                    "--target-dir",
                    "/cache/drasi-target",
                    "--jobs",
                    "4",
                ]
            );
            assert_eq!(command.get_current_dir(), Some(Path::new("/workspace")));
        }
    }

    #[test]
    fn plugin_build_batches_separate_transitive_plugin_dependencies() {
        let packages = vec![
            package(
                "drasi-bootstrap-example",
                true,
                vec![dependency("example-common", "*", None)],
            ),
            package(
                "example-common",
                true,
                vec![dependency("drasi-source-example", "*", Some("build"))],
            ),
            package("drasi-source-example", true, Vec::new()),
            package("drasi-reaction-example", true, Vec::new()),
        ];
        let plugin_names = BTreeSet::from([
            "drasi-bootstrap-example".to_string(),
            "drasi-reaction-example".to_string(),
            "drasi-source-example".to_string(),
        ]);

        assert_eq!(
            plugin_build_batches(&packages, &plugin_names),
            [
                vec![
                    "drasi-bootstrap-example".to_string(),
                    "drasi-reaction-example".to_string(),
                ],
                vec!["drasi-source-example".to_string()],
            ]
        );
    }

    #[test]
    fn plugin_build_batches_ignore_dev_dependencies() {
        let packages = vec![
            package(
                "drasi-bootstrap-example",
                true,
                vec![dependency("drasi-source-example", "*", Some("dev"))],
            ),
            package("drasi-source-example", true, Vec::new()),
        ];
        let plugin_names = BTreeSet::from([
            "drasi-bootstrap-example".to_string(),
            "drasi-source-example".to_string(),
        ]);

        assert_eq!(
            plugin_build_batches(&packages, &plugin_names),
            [vec![
                "drasi-bootstrap-example".to_string(),
                "drasi-source-example".to_string(),
            ]]
        );
    }

    #[test]
    fn plugin_build_batches_support_three_conflict_groups() {
        let packages = vec![
            package(
                "drasi-bootstrap-a",
                true,
                vec![
                    dependency("drasi-bootstrap-b", "*", None),
                    dependency("drasi-bootstrap-c", "*", None),
                ],
            ),
            package(
                "drasi-bootstrap-b",
                true,
                vec![dependency("drasi-bootstrap-d", "*", None)],
            ),
            package(
                "drasi-bootstrap-c",
                true,
                vec![dependency("drasi-bootstrap-d", "*", None)],
            ),
            package("drasi-bootstrap-d", true, Vec::new()),
        ];
        let plugin_names = BTreeSet::from([
            "drasi-bootstrap-a".to_string(),
            "drasi-bootstrap-b".to_string(),
            "drasi-bootstrap-c".to_string(),
            "drasi-bootstrap-d".to_string(),
        ]);

        assert_eq!(
            plugin_build_batches(&packages, &plugin_names),
            [
                vec!["drasi-bootstrap-a".to_string()],
                vec![
                    "drasi-bootstrap-b".to_string(),
                    "drasi-bootstrap-c".to_string(),
                ],
                vec!["drasi-bootstrap-d".to_string()],
            ]
        );
    }

    #[test]
    fn plugin_lib_ext_and_name_match_base_gnu_triple() {
        // After stripping the glibc suffix, the base -gnu triple yields a .so
        // and a lib-prefixed name (regression guard for the zigbuild path).
        let base = strip_glibc_suffix("x86_64-unknown-linux-gnu.2.28");
        assert_eq!(plugin_lib_ext(Some(base)), "so");
        assert_eq!(
            plugin_lib_name("drasi-source-kubernetes", Some(base)),
            "libdrasi_source_kubernetes"
        );
    }
}
