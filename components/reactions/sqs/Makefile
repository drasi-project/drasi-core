build:
	cargo build -p drasi-reaction-sqs

test:
	cargo test -p drasi-reaction-sqs

integration-test:
	cargo test -p drasi-reaction-sqs -- --ignored --nocapture

lint:
	cargo clippy -p drasi-reaction-sqs --all-targets -- -D warnings
