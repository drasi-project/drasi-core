build:
	cargo build -p drasi-source-salesforce

lint:
	cargo clippy -p drasi-source-salesforce --all-targets -- -D warnings

test:
	cargo test -p drasi-source-salesforce

integration-test:
	cargo test -p drasi-source-salesforce --test integration_test -- --ignored --nocapture
