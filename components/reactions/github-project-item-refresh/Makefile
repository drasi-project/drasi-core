.PHONY: build test integration-test lint fmt

build:
	cargo build -p drasi-reaction-github-project-item-refresh

test:
	cargo test -p drasi-reaction-github-project-item-refresh

integration-test:
	cargo test -p drasi-reaction-github-project-item-refresh --test integration_test -- --ignored --nocapture

lint:
	cargo clippy -p drasi-reaction-github-project-item-refresh --all-targets -- -D warnings

fmt:
	cargo fmt --all
