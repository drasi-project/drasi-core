.PHONY: build test integration-test lint clean

build:
	cargo build --release

test:
	cargo test

integration-test:
	cargo test -- --ignored --nocapture

lint:
	cargo clippy -- -D warnings
	cargo fmt --check

clean:
	cargo clean
