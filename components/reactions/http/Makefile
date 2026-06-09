build:
	cargo build -p drasi-reaction-http

test:
	cargo test -p drasi-reaction-http

integration-test:
	cargo test -p drasi-reaction-http --test integration_tests -- --nocapture

lint:
	cargo clippy -p drasi-reaction-http --all-targets -- -D warnings
