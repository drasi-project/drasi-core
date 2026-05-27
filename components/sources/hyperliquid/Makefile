.PHONY: build test integration-test lint

build:
	cargo build -p drasi-source-hyperliquid

test:
	cargo test -p drasi-source-hyperliquid

integration-test:
	cargo test -p drasi-source-hyperliquid -- --ignored --nocapture

lint:
	cargo clippy -p drasi-source-hyperliquid --all-targets -- -D warnings
