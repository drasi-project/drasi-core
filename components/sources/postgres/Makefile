build:
	cargo build -p drasi-source-postgres

lint:
	cargo clippy -p drasi-source-postgres --all-targets --all-features

test:
	cargo test -p drasi-source-postgres

integration-test:
	cargo test -p drasi-source-postgres --test integration_tests -- --ignored --nocapture
