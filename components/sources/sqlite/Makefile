.PHONY: build test lint integration-test clean

build:
	cargo build --release

test:
	cargo test

lint:
	cargo clippy -- -D warnings
	cargo fmt --check

integration-test:
	cargo test -- --ignored --nocapture

clean:
	cargo clean
