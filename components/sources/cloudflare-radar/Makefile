build:
	cargo build -p drasi-source-cloudflare-radar

lint:
	cargo clippy -p drasi-source-cloudflare-radar --all-targets --all-features

test:
	cargo test -p drasi-source-cloudflare-radar

integration-test:
	cargo test -p drasi-source-cloudflare-radar --test integration_test -- --ignored --nocapture
