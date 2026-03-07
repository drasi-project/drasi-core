build:
	cargo build -p drasi-reaction-rabbitmq

test:
	cargo test -p drasi-reaction-rabbitmq

integration-test:
	cargo test -p drasi-reaction-rabbitmq -- --ignored --nocapture

lint:
	cargo clippy -p drasi-reaction-rabbitmq --all-targets --all-features -- -D warnings

fmt:
	cargo fmt --all
