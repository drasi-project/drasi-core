.PHONY: build test integration-test lint fmt

build:
	cargo build -p drasi-reaction-workgraph-admission

test:
	cargo test -p drasi-reaction-workgraph-admission

integration-test:
	cargo test -p drasi-reaction-workgraph-admission --test integration_test -- --ignored --nocapture

lint:
	cargo clippy -p drasi-reaction-workgraph-admission --all-targets -- -D warnings
	cargo fmt -p drasi-reaction-workgraph-admission -- --check

fmt:
	cargo fmt -p drasi-reaction-workgraph-admission
