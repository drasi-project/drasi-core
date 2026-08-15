.PHONY: build test integration-test lint fmt

build:
	cargo build -p drasi-reaction-workgraph-router

test:
	cargo test -p drasi-reaction-workgraph-router

integration-test:
	cargo test -p drasi-reaction-workgraph-router --test integration_test -- --ignored --nocapture

lint:
	cargo clippy -p drasi-reaction-workgraph-router --all-targets -- -D warnings
	cargo fmt -p drasi-reaction-workgraph-router -- --check

fmt:
	cargo fmt -p drasi-reaction-workgraph-router
