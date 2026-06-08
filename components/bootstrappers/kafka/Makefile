.PHONY: build test lint

build:
	cargo build -p drasi-bootstrap-kafka

test:
	cargo test -p drasi-bootstrap-kafka

lint:
	cargo clippy -p drasi-bootstrap-kafka --all-targets -- -D warnings
