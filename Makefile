# Makefile for Drasi Core

# RUSTFLAGS for Clippy linting
# Lint configuration lives in [workspace.lints] in Cargo.toml;
# RUSTFLAGS just ensures warnings are errors.
RUSTFLAGS := -Dwarnings

.PHONY: clippy clippy-fix help build-test-plugins test-host-sdk check-native-dependencies \
       build-native-plugins build-native-standard-plugins build-native-network-plugins test-native-network \
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
	@echo "  build-native-plugins      - Build all nine independent native plugin libraries"
	@echo "  build-native-standard-plugins - Build counter, middleware, capture and arithmetic plugins"
	@echo "  build-native-network-plugins - Build the five independent native network plugins"
	@echo "  test-native-network       - Build native network plugins and run wire/loopback tests"
	@echo "  check-native-dependencies - Reject runtime/query-engine dependencies in native plugin builds"
	@echo "  build-plugins             - Build all dynamic plugins (debug)"
	@echo "  build-plugins-release     - Build all dynamic plugins (release)"
	@echo "  list-plugins              - List all discovered dynamic plugin crates"
	@echo "  publish-plugins-dry-run   - Show what would be published (no push)"
	@echo "  publish-plugins           - Publish plugins to OCI registry (per-arch tags, SIGN=1 to cosign)"
	@echo "  publish-plugins-release   - Publish release plugins (per-arch tags, SIGN=1 to cosign)"
	@echo "  publish-all               - Build, publish all architectures, and merge manifests (SIGN=1 to cosign)"
	@echo "  publish-all-dry-run       - Dry run of publish-all"
	@echo "  merge-manifests           - Create multi-arch manifest index from per-arch tags"
	@echo "  merge-manifests-dry-run   - Show what would be merged (no push)"
	@echo "  help                      - Show this help message"

# Features to enable for clippy linting.
# We avoid --all-features because bundled-jq triggers a flaky jq source build.
# The jq feature (system libjq) covers the same Rust code.
CLIPPY_FEATURES := drasi-middleware/all,drasi-host-sdk/registry,drasi-host-sdk/fetcher,drasi-host-sdk/watcher,drasi-lib/middleware-jq

clippy:
	RUSTFLAGS="$(RUSTFLAGS)" cargo clippy --workspace --all-targets --features "$(CLIPPY_FEATURES)"

clippy-fix:
	RUSTFLAGS="$(RUSTFLAGS)" cargo clippy --workspace --all-targets --features "$(CLIPPY_FEATURES)" --fix

# Build the cdylib plugins required by host-sdk integration tests.
# Legacy plugins are built individually to avoid feature unification issues.
# Plugins are copied to target/debug/plugins/ to match xtask build-plugins layout.
PLUGIN_TARGET_DIR := $(if $(CARGO_TARGET_DIR),$(CARGO_TARGET_DIR),target)
PLUGIN_OUT_DIR := $(PLUGIN_TARGET_DIR)/debug/plugins
NATIVE_STANDARD_PACKAGES := drasi-computation-source-counter \
                           drasi-computation-transformer-middleware \
                           drasi-computation-sink-capture \
                           drasi-computation-transformer-arithmetic
NATIVE_NETWORK_PACKAGES := drasi-computation-source-http \
                          drasi-computation-source-grpc \
                          drasi-computation-sink-http \
                          drasi-computation-sink-grpc \
                          drasi-computation-sink-sse
TEST_PLUGIN_NAMES := drasi_source_mock drasi_reaction_log drasi_reaction_sse \
                     drasi_reaction_snapshot_test drasi_identity_test drasi_bootstrap_scriptfile \
                     $(subst -,_,$(NATIVE_STANDARD_PACKAGES) $(NATIVE_NETWORK_PACKAGES))

build-test-plugins: build-native-plugins
	@echo "=== Building cdylib test plugins ==="
	cargo build --lib -p drasi-source-mock --features drasi-source-mock/dynamic-plugin
	cargo build --lib -p drasi-reaction-log --features drasi-reaction-log/dynamic-plugin
	cargo build --lib -p drasi-reaction-sse --features drasi-reaction-sse/dynamic-plugin
	cargo build --lib -p drasi-reaction-snapshot-test
	cargo build --lib -p drasi-identity-test --features drasi-identity-test/dynamic-plugin
	cargo build --lib -p drasi-bootstrap-scriptfile --features drasi-bootstrap-scriptfile/dynamic-plugin
	cargo build -p drasi-host-sdk --example native_recovery
	cargo build -p drasi-instance-host --example instance_host_fixture --features drasi-instance-host/dynamic-plugin
	@mkdir -p $(PLUGIN_OUT_DIR)
	@echo "=== Copying plugins to $(PLUGIN_OUT_DIR) ==="
	@for ext in dylib so dll; do \
		for name in drasi_computation_standard drasi_computation_network; do \
			rm -f "$(PLUGIN_OUT_DIR)/lib$$name.$$ext" "$(PLUGIN_OUT_DIR)/$$name.$$ext" || exit 1; \
		done; \
		for name in $(TEST_PLUGIN_NAMES); do \
			for prefix in lib ""; do \
				f="$(PLUGIN_TARGET_DIR)/debug/$$prefix$$name.$$ext"; \
				if [ -f "$$f" ]; then cp "$$f" "$(PLUGIN_OUT_DIR)/" || exit 1; fi; \
			done; \
		done; \
	done
	@echo "=== Test plugins built ==="

# Build test plugins, then run host-sdk integration tests.
test-host-sdk: build-test-plugins
	@echo "=== Running host-sdk integration tests ==="
	cargo test -p drasi-host-sdk --lib --test integration_test --test native_computation_pipeline --test native_network_loopback --test native_network_wire -- --test-threads=1
	@echo "=== host-sdk integration tests passed ==="

build-native-plugins: check-native-dependencies build-native-standard-plugins build-native-network-plugins

check-native-dependencies:
	python3 ci/check_native_dependencies.py

build-native-standard-plugins:
	cargo build --lib $(foreach package,$(NATIVE_STANDARD_PACKAGES),-p $(package)) --features dynamic-plugin

build-native-network-plugins:
	cargo build --lib $(foreach package,$(NATIVE_NETWORK_PACKAGES),-p $(package)) --features dynamic-plugin

test-native-network: build-native-network-plugins
	cargo test -p drasi-computation-network-common -p drasi-computation-sink-sse
	cargo test -p drasi-host-sdk --test native_network_loopback --test native_network_wire

# Deterministic cross-cdylib layout-mismatch regression for issue #602.
#
# Builds the mock-source cdylib with one layout seed and runs the host-sdk
# layout-mismatch test with a DIFFERENT seed under glibc heap hardening. Before
# the #602 fix this aborts (free(): invalid pointer / SIGSEGV); after the fix the
# serialized event transfer is layout-independent and the test passes.
#
# Requires a nightly toolchain (for -Zrandomize-layout / -Zlayout-seed).
PLUGIN_LAYOUT_SEED  ?= 1
HOST_LAYOUT_SEED    ?= 2
test-ffi-layout-mismatch:
	@echo "=== [#602] Building mock plugin with layout seed $(PLUGIN_LAYOUT_SEED) (nightly) ==="
	RUSTFLAGS="-Zrandomize-layout -Zlayout-seed=$(PLUGIN_LAYOUT_SEED)" \
		cargo +nightly build --lib -p drasi-source-mock --features drasi-source-mock/dynamic-plugin
	@mkdir -p $(PLUGIN_OUT_DIR)
	@for ext in dylib so dll; do \
		for f in target/debug/libdrasi_source_mock.$$ext target/debug/drasi_source_mock.$$ext; do \
			[ -f "$$f" ] && cp "$$f" $(PLUGIN_OUT_DIR)/ || true; \
		done; \
	done
	@echo "=== [#602] Running layout-mismatch test with host seed $(HOST_LAYOUT_SEED) + MALLOC_CHECK_=3 ==="
	MALLOC_CHECK_=3 MALLOC_PERTURB_=165 GLIBC_TUNABLES=glibc.malloc.check=3 \
		RUSTFLAGS="-Zrandomize-layout -Zlayout-seed=$(HOST_LAYOUT_SEED)" \
		cargo +nightly test -p drasi-host-sdk --test ffi_layout_mismatch_test -- \
			--ignored --nocapture --test-threads=1
	@echo "=== [#602] layout-mismatch regression passed (no heap corruption) ==="

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
# Usage: make publish-plugins [PRE_RELEASE=dev.1] [REGISTRY=ghcr.io/myorg] [ARCH_SUFFIX=linux-amd64] [SIGN=1]
publish-plugins:
	cargo run -p xtask -- publish-plugins $(if $(PRE_RELEASE),--pre-release $(PRE_RELEASE),) $(if $(REGISTRY),--registry $(REGISTRY),) $(if $(ARCH_SUFFIX),--arch-suffix $(ARCH_SUFFIX),) $(if $(SIGN),--sign,)

# Publish release-built plugins
# Usage: make publish-plugins-release [PRE_RELEASE=dev.1] [REGISTRY=ghcr.io/myorg] [ARCH_SUFFIX=linux-amd64] [SIGN=1]
publish-plugins-release:
	cargo run -p xtask -- publish-plugins --release $(if $(PRE_RELEASE),--pre-release $(PRE_RELEASE),) $(if $(REGISTRY),--registry $(REGISTRY),) $(if $(ARCH_SUFFIX),--arch-suffix $(ARCH_SUFFIX),) $(if $(SIGN),--sign,)

# All architectures to build and publish
PUBLISH_TARGETS ?= x86_64-unknown-linux-gnu:linux-amd64 \
                   aarch64-unknown-linux-gnu:linux-arm64 \
                   x86_64-unknown-linux-musl:linux-musl-amd64 \
                   aarch64-unknown-linux-musl:linux-musl-arm64 \
                   x86_64-pc-windows-gnu:windows-amd64 \
                   x86_64-apple-darwin:darwin-amd64 \
                   aarch64-apple-darwin:darwin-arm64

# Build and publish all architectures
# Usage: make publish-all [PRE_RELEASE=dev.1] [REGISTRY=ghcr.io/myorg] [SIGN=1]
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
		     $(if $(SIGN),--sign,) \
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