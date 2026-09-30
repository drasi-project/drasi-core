.PHONY: build test integration-test lint

build:
	cargo build -p drasi-reaction-a2a

test:
	cargo test -p drasi-reaction-a2a

integration-test:
	cargo test -p drasi-reaction-a2a --test integration_tests -- --nocapture

lint:
	cargo clippy -p drasi-reaction-a2a --all-targets -- -D warnings
