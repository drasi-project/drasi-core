build:
	cargo build -p drasi-source-websocket

lint:
	cargo clippy -p drasi-source-websocket --all-targets --all-features -- -D warnings

test:
	cargo test -p drasi-source-websocket

integration-test:
	cargo test -p drasi-source-websocket --test integration -- --nocapture

plugin-test:
	cargo build -p drasi-source-websocket --release --features dynamic-plugin
