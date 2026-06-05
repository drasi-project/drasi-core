build:
	cargo build -p drasi-reaction-azure-storage

test:
	cargo test -p drasi-reaction-azure-storage

integration-test:
	cargo test -p drasi-reaction-azure-storage --test integration_tests -- --ignored --nocapture

lint:
	cargo clippy -p drasi-reaction-azure-storage --all-targets -- -D warnings
