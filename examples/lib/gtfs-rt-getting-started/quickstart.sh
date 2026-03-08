#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

export GTFS_RT_TRIP_UPDATES_URL="${GTFS_RT_TRIP_UPDATES_URL:-https://www.rtd-denver.com/files/gtfs-rt/TripUpdate.pb}"
export GTFS_RT_VEHICLE_POSITIONS_URL="${GTFS_RT_VEHICLE_POSITIONS_URL:-https://www.rtd-denver.com/files/gtfs-rt/VehiclePosition.pb}"
export GTFS_RT_ALERTS_URL="${GTFS_RT_ALERTS_URL:-https://www.rtd-denver.com/files/gtfs-rt/Alerts.pb}"
export GTFS_RT_POLL_INTERVAL_SECS="${GTFS_RT_POLL_INTERVAL_SECS:-30}"
export GTFS_RT_DASHBOARD_PORT="${GTFS_RT_DASHBOARD_PORT:-8090}"

cargo run
