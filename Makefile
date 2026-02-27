# Makefile for Drasi Core

# RUSTFLAGS for Clippy linting (matching ci-lint.yml workflow)
RUSTFLAGS := -Dwarnings \
	-W clippy::print_stdout \
	-W clippy::unwrap_used \
	-A unused \
	-A clippy::module_inception \
	-A clippy::ptr_arg \
	-A clippy::type_complexity

.PHONY: clippy clippy-fix help build-test-plugins test-host-sdk \
       build-plugins build-plugins-release list-plugins \
       publish-plugins publish-plugins-dry-run publish-plugins-release \
       merge-manifests merge-manifests-dry-run

# Default target
help:
	@echo "Available targets:"
	@echo "  clippy                    - Run cargo clippy with same configuration as CI"
	@echo "  build-test-plugins        - Build cdylib plugins needed for host-sdk integration tests"
	@echo "  test-host-sdk             - Build test plugins and run host-sdk integration tests"
	@echo "  build-plugins             - Build all dynamic plugins (debug)"
	@echo "  build-plugins-release     - Build all dynamic plugins (release)"
	@echo "  list-plugins              - List all discovered dynamic plugin crates"
	@echo "  publish-plugins-dry-run   - Show what would be published (no push)"
	@echo "  publish-plugins           - Publish plugins to OCI registry (per-arch tags)"
	@echo "  publish-plugins-release   - Publish release plugins (per-arch tags)"
	@echo "  merge-manifests           - Create multi-arch manifest index from per-arch tags"
	@echo "  merge-manifests-dry-run   - Show what would be merged (no push)"
	@echo "  help                      - Show this help message"

clippy:
	RUSTFLAGS="$(RUSTFLAGS)" cargo clippy --all-targets --all-features

clippy-fix:
	RUSTFLAGS="$(RUSTFLAGS)" cargo clippy --all-targets --all-features --fix

# Build the cdylib plugins required by host-sdk integration tests.
# These are built individually to avoid feature unification issues.
build-test-plugins:
	@echo "=== Building cdylib test plugins ==="
	cargo build --lib -p drasi-source-mock --features drasi-source-mock/dynamic-plugin
	cargo build --lib -p drasi-reaction-log --features drasi-reaction-log/dynamic-plugin
	@echo "=== Test plugins built ==="

# Build test plugins, then run host-sdk integration tests.
test-host-sdk: build-test-plugins
	@echo "=== Running host-sdk integration tests ==="
	cargo test -p drasi-host-sdk --test integration_test
	@echo "=== host-sdk integration tests passed ==="

# === Plugin Build & Publish (via xtask) ===

# Build all dynamic plugins (debug)
build-plugins:
	cargo run -p xtask -- build-plugins

# Build all dynamic plugins (release)
build-plugins-release:
	cargo run -p xtask -- build-plugins --release

# List all discovered dynamic plugin crates
list-plugins:
	cargo run -p xtask -- list-plugins

# Show what would be published (dry run)
# Usage: make publish-plugins-dry-run [PRE_RELEASE=dev.1] [ARCH_SUFFIX=linux-amd64]
publish-plugins-dry-run:
	cargo run -p xtask -- publish-plugins --dry-run $(if $(PRE_RELEASE),--pre-release $(PRE_RELEASE),) $(if $(ARCH_SUFFIX),--arch-suffix $(ARCH_SUFFIX),)

# Publish built plugins to OCI registry (requires OCI_REGISTRY_PASSWORD or GHCR_TOKEN)
# Usage: make publish-plugins [PRE_RELEASE=dev.1] [REGISTRY=ghcr.io/myorg] [ARCH_SUFFIX=linux-amd64]
publish-plugins:
	cargo run -p xtask -- publish-plugins $(if $(PRE_RELEASE),--pre-release $(PRE_RELEASE),) $(if $(REGISTRY),--registry $(REGISTRY),) $(if $(ARCH_SUFFIX),--arch-suffix $(ARCH_SUFFIX),)

# Publish release-built plugins
# Usage: make publish-plugins-release [PRE_RELEASE=dev.1] [REGISTRY=ghcr.io/myorg] [ARCH_SUFFIX=linux-amd64]
publish-plugins-release:
	cargo run -p xtask -- publish-plugins --release $(if $(PRE_RELEASE),--pre-release $(PRE_RELEASE),) $(if $(REGISTRY),--registry $(REGISTRY),) $(if $(ARCH_SUFFIX),--arch-suffix $(ARCH_SUFFIX),)

# Create multi-arch manifest index from per-arch tags
# Usage: make merge-manifests [PRE_RELEASE=dev.1] [REGISTRY=ghcr.io/drasi-project]
# Default architectures: linux-amd64, linux-arm64, windows-amd64, darwin-amd64, darwin-arm64
MERGE_ARCHS ?= linux-amd64 linux-arm64 windows-amd64 darwin-amd64 darwin-arm64
merge-manifests:
	cargo run -p xtask -- merge-manifests $(if $(PRE_RELEASE),--pre-release $(PRE_RELEASE),) $(if $(REGISTRY),--registry $(REGISTRY),) $(foreach arch,$(MERGE_ARCHS),--arch $(arch))

merge-manifests-dry-run:
	cargo run -p xtask -- merge-manifests --dry-run $(if $(PRE_RELEASE),--pre-release $(PRE_RELEASE),) $(if $(REGISTRY),--registry $(REGISTRY),) $(foreach arch,$(MERGE_ARCHS),--arch $(arch))