build:
	cargo build -p drasi-bootstrap-salesforce

lint:
	cargo clippy -p drasi-bootstrap-salesforce --all-targets -- -D warnings

test:
	cargo test -p drasi-bootstrap-salesforce

integration-test:
	cargo test -p drasi-bootstrap-salesforce -- --ignored --nocapture
