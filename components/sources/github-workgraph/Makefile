.PHONY: build test lint

build:
	cargo build -p drasi-source-github-workgraph

test:
	cargo test -p drasi-source-github-workgraph

lint:
	cargo clippy -p drasi-source-github-workgraph --all-targets --all-features -- -D warnings
