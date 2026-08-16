.PHONY: build test integration-test lint

build:
	cargo build -p drasi-source-github-workgraph

test:
	cargo test -p drasi-source-github-workgraph

integration-test:
	cargo test -p drasi-source-github-workgraph --test integration_test -- --ignored --nocapture

lint:
	cargo clippy -p drasi-source-github-workgraph --all-targets --all-features -- -D warnings
