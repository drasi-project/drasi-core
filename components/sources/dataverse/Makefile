build:
	cargo build -p drasi-source-dataverse

lint:
	cargo clippy -p drasi-source-dataverse --all-targets --all-features

test:
	cargo test -p drasi-source-dataverse

integration-test:
	cargo test -p drasi-source-dataverse --test integration_test -- --ignored --nocapture
