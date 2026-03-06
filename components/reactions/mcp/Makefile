.PHONY: build test lint integration-test

build:
	cargo build -p drasi-reaction-mcp

test:
	cargo test -p drasi-reaction-mcp

lint:
	cargo clippy -p drasi-reaction-mcp --all-targets --all-features -- -D warnings

integration-test:
	cargo test -p drasi-reaction-mcp -- --ignored --nocapture
