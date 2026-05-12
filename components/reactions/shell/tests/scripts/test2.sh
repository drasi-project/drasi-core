#!/bin/bash

## FOR TESTING PURPOSES ONLY. USED WITHIN THE INTEGRATION TESTS IN `integration_tests.rs`

## if FAIL_EXIT is true, exit with status 1 to simulate a failure, else print the stdin input

if [ "$FAIL_EXIT" = "true" ]; then
    echo "Simulating failure by exiting with status 1."
    exit 1
else
    read -r input
    echo "$input"
    exit 0 
fi