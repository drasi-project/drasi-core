build:
	cargo build -p drasi-reaction-grpc

test:
	cargo test -p drasi-reaction-grpc

integration-test:
	cargo test -p drasi-reaction-grpc -- --ignored --nocapture

lint:
	cargo clippy -p drasi-reaction-grpc --all-targets -- -D warnings
