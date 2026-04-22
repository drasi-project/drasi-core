build:
	cargo build -p drasi-source-ris-live

lint:
	cargo clippy -p drasi-source-ris-live --all-targets --all-features -- -D warnings

test:
	cargo test -p drasi-source-ris-live

integration-test:
	cargo test -p drasi-source-ris-live --test integration_test -- --ignored --nocapture
