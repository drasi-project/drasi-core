#!/bin/bash

## FOR TESTING PURPOSES ONLY. USED WITHIN THE INTEGRATION TESTS IN `integration_tests.rs`

## Testing the timeout behavior of the shell reaction. If the script runs longer than the specified timeout, it should be terminated.


if [ "$RUN_TIMEOUT_TEST" = "true" ]; then
    echo $SENSOR_ID
    sleep 3
    echo $SENSOR_TEMP
    sleep 3
    echo $SENSOR_STDIN
    sleep 3
    echo $SENSOR_LOCATION
    sleep 3
    exit 0
else
    echo "5"
    sleep 40
    exit 0
fi