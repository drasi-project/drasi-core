#!/usr/bin/env bash
set -euo pipefail

trip_url="${GTFS_RT_TRIP_UPDATES_URL:-https://www.rtd-denver.com/files/gtfs-rt/TripUpdate.pb}"
vp_url="${GTFS_RT_VEHICLE_POSITIONS_URL:-https://www.rtd-denver.com/files/gtfs-rt/VehiclePosition.pb}"
alerts_url="${GTFS_RT_ALERTS_URL:-https://www.rtd-denver.com/files/gtfs-rt/Alerts.pb}"

diagnose() {
  local name="$1"
  local url="$2"

  if [[ -z "$url" ]]; then
    echo "[$name] disabled"
    return
  fi

  echo "[$name] $url"
  curl -sS -o /tmp/gtfs-rt-diagnose.pb -w "  status=%{http_code} bytes=%{size_download} time=%{time_total}s\n" "$url"
}

diagnose "TripUpdates" "$trip_url"
diagnose "VehiclePositions" "$vp_url"
diagnose "Alerts" "$alerts_url"

echo "Done."
