.PHONY: build test integration-test lint

build:
	cargo build -p drasi-source-kafka

test:
	cargo test -p drasi-source-kafka --lib

integration-test:
	cargo test -p drasi-source-kafka --test integration -- --ignored --nocapture

lint:
	cargo clippy -p drasi-source-kafka --all-targets -- -D warnings
