# Makefile for Drasi Core

# RUSTFLAGS for Clippy linting (matching ci-lint.yml workflow)
RUSTFLAGS := -Dwarnings \
	-W clippy::print_stdout \
	-W clippy::unwrap_used \
	-A unused \
	-A clippy::module_inception \
	-A clippy::ptr_arg \
	-A clippy::type_complexity

.PHONY: clippy clippy-crate clippy-all-crates help

# Default target
help:
	@echo "Available targets:"
	@echo "  clippy              - Run cargo clippy on all workspace members with same configuration as CI"
	@echo "  clippy-crate CRATE=<crate-name>"
	@echo "                      - Run cargo clippy on a specific crate with same configuration as CI"
	@echo "                        Example: make clippy-crate CRATE=core"
	@echo "  clippy-all-crates   - Run cargo clippy on each workspace member individually"
	@echo "  help                - Show this help message"

clippy:
	RUSTFLAGS="$(RUSTFLAGS)" cargo clippy --all-targets --all-features

clippy-crate:
ifndef CRATE
	@echo "Error: CRATE variable must be set"
	@echo "Usage: make clippy-crate CRATE=<crate-name>"
	@echo "Available crates: core, query-ast, query-cypher, query-gql, functions-cypher,"
	@echo "                  functions-gql, index-rocksdb, index-garnet, middleware,"
	@echo "                  shared-tests, query-perf, examples"
	@exit 1
endif
	@echo "Running clippy on crate: $(CRATE)"
	RUSTFLAGS="$(RUSTFLAGS)" cargo clippy --manifest-path $(CRATE)/Cargo.toml --all-targets --all-features

clippy-all-crates:
	@echo "Running clippy on each workspace member individually..."
	@for crate in core query-ast query-cypher query-gql functions-cypher functions-gql index-rocksdb index-garnet middleware shared-tests query-perf examples; do \
		echo ""; \
		echo "==========================================="; \
		echo "Running clippy on crate: $$crate"; \
		echo "==========================================="; \
		RUSTFLAGS="$(RUSTFLAGS)" cargo clippy --manifest-path $$crate/Cargo.toml --all-targets --all-features || exit 1; \
	done
	@echo ""
	@echo "==========================================="; \
	@echo "All crates passed clippy checks!"; \
	@echo "==========================================="