.PHONY: build test integration-test lint

build:
	cargo build -p drasi-bootstrap-mysql

test:
	cargo test -p drasi-bootstrap-mysql

integration-test:
	cargo test -p drasi-bootstrap-mysql -- --ignored --nocapture

lint:
	cargo clippy -p drasi-bootstrap-mysql --all-targets -- -D warnings
