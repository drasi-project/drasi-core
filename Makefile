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
       publish-all publish-all-dry-run \
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
	@echo "  publish-all               - Build, publish all architectures, and merge manifests"
	@echo "  publish-all-dry-run       - Dry run of publish-all"
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

# All architectures to build and publish
PUBLISH_TARGETS ?= x86_64-unknown-linux-gnu:linux-amd64 \
                   aarch64-unknown-linux-gnu:linux-arm64 \
                   x86_64-unknown-linux-musl:linux-musl-amd64 \
                   aarch64-unknown-linux-musl:linux-musl-arm64 \
                   x86_64-pc-windows-gnu:windows-amd64 \
                   x86_64-apple-darwin:darwin-amd64 \
                   aarch64-apple-darwin:darwin-arm64

# Build and publish all architectures
# Usage: make publish-all [PRE_RELEASE=dev.1] [REGISTRY=ghcr.io/myorg]
# Skips targets that fail to build (e.g., macOS targets on Linux)
# Each architecture is published with its own tag suffix (e.g., 0.1.8-linux-amd64)
# The client auto-appends the correct suffix when installing.
publish-all:
	@SUCCEEDED=0; FAILED=0; \
	for entry in $(PUBLISH_TARGETS); do \
		TARGET=$$(echo $$entry | cut -d: -f1); \
		SUFFIX=$$(echo $$entry | cut -d: -f2); \
		echo ""; \
		echo "=== Building and publishing $$TARGET ($$SUFFIX) ==="; \
		if cargo run -p xtask -- build-plugins --release --target $$TARGET && \
		   cargo run -p xtask -- publish-plugins --release --target $$TARGET \
		     $(if $(PRE_RELEASE),--pre-release $(PRE_RELEASE),) \
		     $(if $(REGISTRY),--registry $(REGISTRY),) \
		     --arch-suffix $$SUFFIX; then \
			SUCCEEDED=$$((SUCCEEDED + 1)); \
		else \
			echo "  ⚠ Skipping $$TARGET (build or publish failed)"; \
			FAILED=$$((FAILED + 1)); \
		fi; \
	done; \
	echo ""; \
	echo "=== Publish complete: $$SUCCEEDED succeeded, $$FAILED failed ==="; \
	if [ $$SUCCEEDED -eq 0 ]; then exit 1; fi

# Dry run of publish-all (builds but doesn't push)
publish-all-dry-run:
	@for entry in $(PUBLISH_TARGETS); do \
		TARGET=$$(echo $$entry | cut -d: -f1); \
		SUFFIX=$$(echo $$entry | cut -d: -f2); \
		echo ""; \
		echo "=== [DRY RUN] Building $$TARGET ($$SUFFIX) ==="; \
		cargo run -p xtask -- build-plugins --release --target $$TARGET || \
			echo "  ⚠ Skipping $$TARGET (build failed)"; \
		cargo run -p xtask -- publish-plugins --release --target $$TARGET \
		  $(if $(PRE_RELEASE),--pre-release $(PRE_RELEASE),) \
		  $(if $(REGISTRY),--registry $(REGISTRY),) \
		  --arch-suffix $$SUFFIX --dry-run || true; \
	done