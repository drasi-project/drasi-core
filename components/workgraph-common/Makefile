.PHONY: build test integration-test lint fmt

build:
	cargo build -p drasi-workgraph-common

test:
	cargo test -p drasi-workgraph-common

# The shared crate has no external target: its cross-language contract is
# verified by the committed test vectors, which run as ordinary tests.
integration-test:
	cargo test -p drasi-workgraph-common --test vectors -- --nocapture

lint:
	cargo clippy -p drasi-workgraph-common --all-targets -- -D warnings
	cargo fmt -p drasi-workgraph-common -- --check

fmt:
	cargo fmt -p drasi-workgraph-common
