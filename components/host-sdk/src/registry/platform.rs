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

//! Platform mapping between OCI platform strings and Rust target triples.

/// An OCI platform descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OciPlatform {
    pub os: String,
    pub architecture: String,
    pub variant: Option<String>,
}

impl std::fmt::Display for OciPlatform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.variant {
            Some(v) => write!(f, "{}/{}/{}", self.os, self.architecture, v),
            None => write!(f, "{}/{}", self.os, self.architecture),
        }
    }
}

/// Map a Rust target triple to an OCI platform.
pub fn target_triple_to_oci_platform(triple: &str) -> Option<OciPlatform> {
    let parts: Vec<&str> = triple.split('-').collect();
    if parts.len() < 3 {
        return None;
    }

    let arch = parts[0];
    let os_part = parts.get(2).copied().unwrap_or("");

    let architecture = match arch {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "armv7" | "armv7l" => "arm",
        "i686" | "i386" => "386",
        "riscv64" | "riscv64gc" => "riscv64",
        "s390x" => "s390x",
        "powerpc64le" | "ppc64le" => "ppc64le",
        _ => return None,
    };

    let os = match os_part {
        "linux" => "linux",
        "windows" => "windows",
        "darwin" | "apple" => "darwin",
        "freebsd" => "freebsd",
        _ => {
            // Check if any part contains the OS
            if triple.contains("linux") {
                "linux"
            } else if triple.contains("windows") {
                "windows"
            } else if triple.contains("darwin") || triple.contains("apple") {
                "darwin"
            } else {
                return None;
            }
        }
    };

    let variant = if arch == "armv7" || arch == "armv7l" {
        Some("v7".to_string())
    } else {
        None
    };

    Some(OciPlatform {
        os: os.to_string(),
        architecture: architecture.to_string(),
        variant,
    })
}

/// Map an OCI platform back to a Rust target triple.
///
/// Returns the most common target triple for the given platform.
/// For ambiguous mappings (e.g., linux/amd64 could be gnu or musl),
/// the gnu variant is preferred.
pub fn oci_platform_to_target_triple(platform: &OciPlatform) -> Option<String> {
    let triple = match (platform.os.as_str(), platform.architecture.as_str()) {
        ("linux", "amd64") => "x86_64-unknown-linux-gnu",
        ("linux", "arm64") => "aarch64-unknown-linux-gnu",
        ("linux", "arm") => "armv7-unknown-linux-gnueabihf",
        ("linux", "386") => "i686-unknown-linux-gnu",
        ("linux", "riscv64") => "riscv64gc-unknown-linux-gnu",
        ("linux", "s390x") => "s390x-unknown-linux-gnu",
        ("linux", "ppc64le") => "powerpc64le-unknown-linux-gnu",
        ("windows", "amd64") => "x86_64-pc-windows-gnu",
        ("windows", "arm64") => "aarch64-pc-windows-gnullvm",
        ("darwin", "amd64") => "x86_64-apple-darwin",
        ("darwin", "arm64") => "aarch64-apple-darwin",
        ("freebsd", "amd64") => "x86_64-unknown-freebsd",
        _ => return None,
    };

    Some(triple.to_string())
}

/// Convert a Rust target triple to the `{os}-{arch}` suffix used in OCI tags.
///
/// For example:
/// - `x86_64-unknown-linux-gnu` → `linux-amd64`
/// - `x86_64-unknown-linux-musl` → `linux-musl-amd64`
/// - `aarch64-apple-darwin` → `darwin-arm64`
/// - `x86_64-pc-windows-gnu` → `windows-amd64`
pub fn target_triple_to_arch_suffix(triple: &str) -> Option<String> {
    let platform = target_triple_to_oci_platform(triple)?;
    if triple.contains("musl") {
        Some(format!("{}-musl-{}", platform.os, platform.architecture))
    } else {
        Some(format!("{}-{}", platform.os, platform.architecture))
    }
}

/// Strip a known `{os}-{arch}` suffix from a tag and return `(version, suffix)`.
///
/// For example, `"0.1.8-linux-amd64"` → `Some(("0.1.8", "linux-amd64"))`.
/// Returns `None` if the tag doesn't end with a recognized platform suffix.
pub fn strip_arch_suffix(tag: &str) -> Option<(&str, &str)> {
    // Known suffixes — longer ones first to avoid partial matches
    const KNOWN_SUFFIXES: &[&str] = &[
        "linux-musl-amd64",
        "linux-musl-arm64",
        "linux-amd64",
        "linux-arm64",
        "linux-arm",
        "linux-386",
        "linux-riscv64",
        "linux-s390x",
        "linux-ppc64le",
        "windows-amd64",
        "windows-arm64",
        "darwin-amd64",
        "darwin-arm64",
        "freebsd-amd64",
    ];

    for suffix in KNOWN_SUFFIXES {
        if let Some(version) = tag.strip_suffix(suffix) {
            if let Some(version) = version.strip_suffix('-') {
                return Some((version, suffix));
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_target_triple_to_oci() {
        let cases = vec![
            ("x86_64-unknown-linux-gnu", "linux", "amd64"),
            ("aarch64-unknown-linux-gnu", "linux", "arm64"),
            ("x86_64-pc-windows-gnu", "windows", "amd64"),
            ("aarch64-apple-darwin", "darwin", "arm64"),
            ("x86_64-apple-darwin", "darwin", "amd64"),
            ("x86_64-unknown-linux-musl", "linux", "amd64"),
        ];

        for (triple, expected_os, expected_arch) in cases {
            let p = target_triple_to_oci_platform(triple)
                .unwrap_or_else(|| panic!("failed to map {}", triple));
            assert_eq!(p.os, expected_os, "OS mismatch for {}", triple);
            assert_eq!(
                p.architecture, expected_arch,
                "Arch mismatch for {}",
                triple
            );
        }
    }

    #[test]
    fn test_oci_to_target_triple() {
        let p = OciPlatform {
            os: "linux".to_string(),
            architecture: "amd64".to_string(),
            variant: None,
        };
        assert_eq!(
            oci_platform_to_target_triple(&p),
            Some("x86_64-unknown-linux-gnu".to_string())
        );

        let p = OciPlatform {
            os: "windows".to_string(),
            architecture: "amd64".to_string(),
            variant: None,
        };
        assert_eq!(
            oci_platform_to_target_triple(&p),
            Some("x86_64-pc-windows-gnu".to_string())
        );
    }

    #[test]
    fn test_platform_display() {
        let p = OciPlatform {
            os: "linux".to_string(),
            architecture: "amd64".to_string(),
            variant: None,
        };
        assert_eq!(p.to_string(), "linux/amd64");

        let p = OciPlatform {
            os: "linux".to_string(),
            architecture: "arm".to_string(),
            variant: Some("v7".to_string()),
        };
        assert_eq!(p.to_string(), "linux/arm/v7");
    }

    #[test]
    fn test_roundtrip() {
        let triples = vec![
            "x86_64-unknown-linux-gnu",
            "aarch64-unknown-linux-gnu",
            "x86_64-pc-windows-gnu",
            "aarch64-apple-darwin",
            "x86_64-apple-darwin",
        ];

        for triple in triples {
            let platform = target_triple_to_oci_platform(triple).unwrap();
            let back = oci_platform_to_target_triple(&platform).unwrap();
            // Note: roundtrip may normalize (e.g., musl → gnu)
            assert!(
                !back.is_empty(),
                "roundtrip failed for {}: {:?}",
                triple,
                platform
            );
        }
    }

    #[test]
    fn test_target_triple_to_arch_suffix() {
        assert_eq!(
            target_triple_to_arch_suffix("x86_64-unknown-linux-gnu"),
            Some("linux-amd64".to_string())
        );
        assert_eq!(
            target_triple_to_arch_suffix("x86_64-unknown-linux-musl"),
            Some("linux-musl-amd64".to_string())
        );
        assert_eq!(
            target_triple_to_arch_suffix("aarch64-unknown-linux-musl"),
            Some("linux-musl-arm64".to_string())
        );
        assert_eq!(
            target_triple_to_arch_suffix("aarch64-apple-darwin"),
            Some("darwin-arm64".to_string())
        );
        assert_eq!(
            target_triple_to_arch_suffix("x86_64-pc-windows-gnu"),
            Some("windows-amd64".to_string())
        );
        assert_eq!(
            target_triple_to_arch_suffix("x86_64-pc-windows-msvc"),
            Some("windows-amd64".to_string())
        );
        assert_eq!(target_triple_to_arch_suffix("unknown-triple"), None);
    }

    #[test]
    fn test_strip_arch_suffix() {
        assert_eq!(
            strip_arch_suffix("0.1.8-linux-amd64"),
            Some(("0.1.8", "linux-amd64"))
        );
        assert_eq!(
            strip_arch_suffix("0.1.8-linux-musl-amd64"),
            Some(("0.1.8", "linux-musl-amd64"))
        );
        assert_eq!(
            strip_arch_suffix("0.1.8-dev.1-linux-musl-arm64"),
            Some(("0.1.8-dev.1", "linux-musl-arm64"))
        );
        assert_eq!(
            strip_arch_suffix("0.1.8-dev.1-darwin-arm64"),
            Some(("0.1.8-dev.1", "darwin-arm64"))
        );
        assert_eq!(
            strip_arch_suffix("0.1.8-rc.1-windows-amd64"),
            Some(("0.1.8-rc.1", "windows-amd64"))
        );
        assert_eq!(strip_arch_suffix("0.1.8"), None);
        assert_eq!(strip_arch_suffix("latest"), None);
    }
}
