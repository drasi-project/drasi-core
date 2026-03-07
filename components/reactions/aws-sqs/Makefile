build:
	cargo build -p drasi-reaction-aws-sqs

test:
	cargo test -p drasi-reaction-aws-sqs

integration-test:
	cargo test -p drasi-reaction-aws-sqs -- --ignored --nocapture

lint:
	cargo clippy -p drasi-reaction-aws-sqs --all-targets -- -D warnings
