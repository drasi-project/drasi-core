.PHONY: build test lint

build:
	cargo build -p drasi-reaction-github-workgraph

test:
	cargo test -p drasi-reaction-github-workgraph

lint:
	cargo clippy -p drasi-reaction-github-workgraph --all-targets --all-features -- -D warnings
