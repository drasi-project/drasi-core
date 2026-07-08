build:
	cargo build -p drasi-reaction-dashboard

test: test-js
	cargo test -p drasi-reaction-dashboard

test-js:
	node tests/handlebars_helpers.test.js
	node tests/apply_result_diff.test.js

integration-test:
	cargo test -p drasi-reaction-dashboard -- --ignored --nocapture

lint:
	cargo clippy -p drasi-reaction-dashboard --all-targets -- -D warnings
