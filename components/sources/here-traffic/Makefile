build:
	cargo build -p drasi-source-here-traffic

lint:
	cargo clippy -p drasi-source-here-traffic --all-targets --all-features

test:
	cargo test -p drasi-source-here-traffic

integration-test:
	cargo test -p drasi-source-here-traffic --test integration_test -- --ignored --nocapture
