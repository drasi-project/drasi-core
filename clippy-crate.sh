#!/bin/bash

# Script to run clippy on a specific crate with CI configuration
# This matches the configuration used in .github/workflows/ci-lint.yml

set -e

# RUSTFLAGS matching ci-lint.yml workflow
export RUSTFLAGS="-Dwarnings -W clippy::print_stdout -W clippy::unwrap_used -A unused -A clippy::module_inception -A clippy::ptr_arg -A clippy::type_complexity"

# List of workspace members
CRATES=(
    "core"
    "query-ast"
    "query-cypher"
    "query-gql"
    "functions-cypher"
    "functions-gql"
    "index-rocksdb"
    "index-garnet"
    "middleware"
    "shared-tests"
    "query-perf"
    "examples"
)

# Function to display usage
usage() {
    echo "Usage: $0 [crate-name]"
    echo ""
    echo "Run clippy on a specific crate with the same configuration as CI."
    echo ""
    echo "Available crates:"
    for crate in "${CRATES[@]}"; do
        echo "  - $crate"
    done
    echo ""
    echo "Examples:"
    echo "  $0 core              # Run clippy on the core crate"
    echo "  $0 query-cypher      # Run clippy on the query-cypher crate"
    echo ""
    echo "To run clippy on all crates at once (as CI does):"
    echo "  make clippy"
    echo ""
    echo "To run clippy on all crates one at a time:"
    echo "  make clippy-all-crates"
    exit 1
}

# Check if crate argument is provided
if [ $# -eq 0 ]; then
    usage
fi

CRATE=$1

# Validate crate name
if [[ ! " ${CRATES[@]} " =~ " ${CRATE} " ]]; then
    echo "Error: '$CRATE' is not a valid crate name"
    echo ""
    usage
fi

echo "===========================================";
echo "Running clippy on crate: $CRATE"
echo "===========================================";
echo ""
echo "RUSTFLAGS: $RUSTFLAGS"
echo ""

cargo clippy --manifest-path "$CRATE/Cargo.toml" --all-targets --all-features

echo ""
echo "===========================================";
echo "Clippy check passed for crate: $CRATE"
echo "===========================================";
