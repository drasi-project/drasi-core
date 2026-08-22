.PHONY: build test lint

build:
	cargo build -p drasi-reaction-github-workgraph-dispatcher

test:
	cargo test -p drasi-reaction-github-workgraph-dispatcher

lint:
	cargo clippy -p drasi-reaction-github-workgraph-dispatcher --all-targets --all-features -- -D warnings

