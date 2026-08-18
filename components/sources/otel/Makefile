build:
	cargo build -p drasi-source-otel

lint:
	cargo clippy -p drasi-source-otel --all-targets --all-features

test:
	cargo test -p drasi-source-otel

integration-test:
	cargo test -p drasi-source-otel --test integration_test -- --ignored --nocapture
