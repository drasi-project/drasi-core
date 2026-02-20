.PHONY: build test integration-test lint

build:
	cargo build -p drasi-source-mysql

test:
	cargo test -p drasi-source-mysql

integration-test:
	cargo test -p drasi-source-mysql -- --ignored --nocapture

lint:
	cargo clippy -p drasi-source-mysql --all-targets -- -D warnings
