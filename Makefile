# Makefile for Drasi Core

# RUSTFLAGS for Clippy linting (matching ci-lint.yml workflow)
RUSTFLAGS := -Dwarnings \
	-W clippy::print_stdout \
	-W clippy::unwrap_used \
	-A unused \
	-A clippy::module_inception \
	-A clippy::ptr_arg \
	-A clippy::type_complexity

.PHONY: clippy clippy-fix help build-test-plugins test-host-sdk

# Default target
help:
	@echo "Available targets:"
	@echo "  clippy             - Run cargo clippy with same configuration as CI"
	@echo "  build-test-plugins - Build cdylib plugins needed for host-sdk integration tests"
	@echo "  test-host-sdk      - Build test plugins and run host-sdk integration tests"
	@echo "  help               - Show this help message"

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