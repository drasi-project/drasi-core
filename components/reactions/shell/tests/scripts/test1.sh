#!/bin/bash

## FOR TESTING PURPOSES ONLY. USED WITHIN THE INTEGRATION TESTS IN `integration_tests.rs`

# if STDIN_ENV_VAR is enabled, read the input
if [ "$STDIN_ENV_VAR" = "true" ]; then
    read -r input
    echo "$input"
else
    echo "No input received. STDIN_ENV_VAR is not set to true."
fi