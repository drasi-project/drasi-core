build:
	cargo build -p drasi-reaction-workgraph-router

test:
	cargo test -p drasi-reaction-workgraph-router

integration-test:
	cargo test -p drasi-reaction-workgraph-router --test integration_test -- --ignored --nocapture

lint:
	cargo clippy -p drasi-reaction-workgraph-router --all-targets --all-features -- -D warnings

fmt:
	cargo fmt --all
