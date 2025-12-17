# Makefile for Drasi Core

.PHONY: clippy help

# Default target
help:
	@echo "Available targets:"
	@echo "  clippy    - Run cargo clippy on all workspace members"
	@echo "  help      - Show this help message"
	@echo ""
	@echo "To run clippy on a specific crate:"
	@echo "  cd <crate-directory> && cargo clippy"
	@echo ""
	@echo "To auto-fix clippy issues:"
	@echo "  cd <crate-directory> && cargo clippy --fix"

clippy:
	cargo clippy --all-targets --all-features