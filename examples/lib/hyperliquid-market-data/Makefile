.PHONY: build test integration-test lint

build:
	cargo build

test:
	cargo test

integration-test:
	cargo test -- --ignored --nocapture

lint:
	cargo clippy --all-targets -- -D warnings
