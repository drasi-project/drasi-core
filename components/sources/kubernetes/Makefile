build:
	cargo build -p drasi-source-kubernetes

lint:
	cargo clippy -p drasi-source-kubernetes --all-targets --all-features

test:
	cargo test -p drasi-source-kubernetes

integration-test:
	cargo test -p drasi-source-kubernetes --test integration_tests -- --ignored --nocapture
