#!/bin/bash

# HTTP Integration Test Driver Script
# This script sends test data to a running DrasiLib HTTP source
# to manually test the data flow from source through query to reaction.

set -e

# Configuration
HTTP_SOURCE_URL="${HTTP_SOURCE_URL:-http://127.0.0.1:8080/sources/http-source}"
COLOR_GREEN='\033[0;32m'
COLOR_BLUE='\033[0;34m'
COLOR_YELLOW='\033[1;33m'
COLOR_RED='\033[0;31m'
COLOR_RESET='\033[0m'

# Print colored message
log_info() {
    echo -e "${COLOR_BLUE}[INFO]${COLOR_RESET} $1"
}

log_success() {
    echo -e "${COLOR_GREEN}[SUCCESS]${COLOR_RESET} $1"
}

log_warning() {
    echo -e "${COLOR_YELLOW}[WARNING]${COLOR_RESET} $1"
}

log_error() {
    echo -e "${COLOR_RED}[ERROR]${COLOR_RESET} $1"
}

# Check if curl is available
if ! command -v curl &> /dev/null; then
    log_error "curl is required but not installed. Please install curl."
    exit 1
fi

# Display usage
show_usage() {
    cat << EOF
Usage: $0 [OPTIONS]

Options:
    --url URL           HTTP source URL (default: $HTTP_SOURCE_URL)
    --insert            Send insert operation
    --update            Send update operation
    --delete            Send delete operation
    --batch             Send batch operations
    --all               Send all operation types (default)
    --help              Show this help message

Environment Variables:
    HTTP_SOURCE_URL     Override default HTTP source URL

Examples:
    # Send all operations to default URL
    $0

    # Send only insert operation
    $0 --insert

    # Send to custom URL
    $0 --url http://localhost:9080/sources/my-source

    # Send batch operations
    $0 --batch
EOF
}

# Send single event to HTTP source
send_event() {
    local endpoint="$1"
    local data="$2"
    local description="$3"

    log_info "Sending $description..."

    response=$(curl -s -w "\n%{http_code}" -X POST \
        -H "Content-Type: application/json" \
        -d "$data" \
        "$endpoint")

    http_code=$(echo "$response" | tail -n1)
    body=$(echo "$response" | head -n-1)

    if [ "$http_code" -ge 200 ] && [ "$http_code" -lt 300 ]; then
        log_success "$description sent successfully (HTTP $http_code)"
        return 0
    else
        log_error "$description failed (HTTP $http_code)"
        echo "Response: $body"
        return 1
    fi
}

# Send insert operation
send_insert() {
    local user_id="${1:-user_$(date +%s)}"
    local user_name="${2:-Test User}"
    local user_email="${3:-test@example.com}"

    local data=$(cat <<EOF
{
  "operation": "insert",
  "element": {
    "type": "node",
    "id": "$user_id",
    "labels": ["User"],
    "properties": {
      "id": "$user_id",
      "name": "$user_name",
      "email": "$user_email"
    }
  },
  "timestamp": $(date +%s%3N)
}
EOF
)

    send_event "$HTTP_SOURCE_URL/events" "$data" "INSERT operation for $user_id"
}

# Send update operation
send_update() {
    local user_id="${1:-user_update}"
    local user_name="${2:-Updated User}"
    local user_email="${3:-updated@example.com}"

    local data=$(cat <<EOF
{
  "operation": "update",
  "element": {
    "type": "node",
    "id": "$user_id",
    "labels": ["User"],
    "properties": {
      "id": "$user_id",
      "name": "$user_name",
      "email": "$user_email"
    }
  },
  "timestamp": $(date +%s%3N)
}
EOF
)

    send_event "$HTTP_SOURCE_URL/events" "$data" "UPDATE operation for $user_id"
}

# Send delete operation
send_delete() {
    local user_id="${1:-user_delete}"

    local data=$(cat <<EOF
{
  "operation": "delete",
  "id": "$user_id",
  "labels": ["User"],
  "timestamp": $(date +%s%3N)
}
EOF
)

    send_event "$HTTP_SOURCE_URL/events" "$data" "DELETE operation for $user_id"
}

# Send batch operations
send_batch() {
    local timestamp=$(date +%s%3N)

    local data=$(cat <<EOF
{
  "events": [
    {
      "operation": "insert",
      "element": {
        "type": "node",
        "id": "batch_user_1",
        "labels": ["User"],
        "properties": {
          "id": "batch_user_1",
          "name": "Batch User 1",
          "email": "batch1@example.com"
        }
      },
      "timestamp": $timestamp
    },
    {
      "operation": "insert",
      "element": {
        "type": "node",
        "id": "batch_user_2",
        "labels": ["User"],
        "properties": {
          "id": "batch_user_2",
          "name": "Batch User 2",
          "email": "batch2@example.com"
        }
      },
      "timestamp": $((timestamp + 1))
    },
    {
      "operation": "insert",
      "element": {
        "type": "node",
        "id": "batch_user_3",
        "labels": ["User"],
        "properties": {
          "id": "batch_user_3",
          "name": "Batch User 3",
          "email": "batch3@example.com"
        }
      },
      "timestamp": $((timestamp + 2))
    }
  ]
}
EOF
)

    send_event "$HTTP_SOURCE_URL/events/batch" "$data" "BATCH operations (3 inserts)"
}

# Send comprehensive test sequence
send_all() {
    log_info "Starting comprehensive test sequence..."
    echo

    # Insert a user
    send_insert "test_user_1" "Alice Johnson" "alice@example.com"
    sleep 1

    # Update the user
    send_update "test_user_1" "Alice Smith" "alice.smith@example.com"
    sleep 1

    # Insert another user
    send_insert "test_user_2" "Bob Williams" "bob@example.com"
    sleep 1

    # Delete first user
    send_delete "test_user_1"
    sleep 1

    # Send batch
    send_batch

    echo
    log_success "Test sequence completed!"
    log_info "Check your HTTP reaction endpoint to verify data flow."
}

# Parse command line arguments
DO_INSERT=false
DO_UPDATE=false
DO_DELETE=false
DO_BATCH=false
DO_ALL=true

while [[ $# -gt 0 ]]; do
    case $1 in
        --url)
            HTTP_SOURCE_URL="$2"
            shift 2
            ;;
        --insert)
            DO_INSERT=true
            DO_ALL=false
            shift
            ;;
        --update)
            DO_UPDATE=true
            DO_ALL=false
            shift
            ;;
        --delete)
            DO_DELETE=true
            DO_ALL=false
            shift
            ;;
        --batch)
            DO_BATCH=true
            DO_ALL=false
            shift
            ;;
        --all)
            DO_ALL=true
            shift
            ;;
        --help)
            show_usage
            exit 0
            ;;
        *)
            log_error "Unknown option: $1"
            show_usage
            exit 1
            ;;
    esac
done

# Main execution
log_info "HTTP Integration Test Driver"
log_info "Target URL: $HTTP_SOURCE_URL"
echo

# Check if HTTP source is accessible
if ! curl -s -f "$HTTP_SOURCE_URL/../../health" > /dev/null 2>&1; then
    log_warning "Cannot reach HTTP source at $HTTP_SOURCE_URL"
    log_warning "Make sure DrasiLib is running with HTTP source enabled"
    echo
fi

# Execute requested operations
if [ "$DO_ALL" = true ]; then
    send_all
else
    [ "$DO_INSERT" = true ] && send_insert
    [ "$DO_UPDATE" = true ] && send_update
    [ "$DO_DELETE" = true ] && send_delete
    [ "$DO_BATCH" = true ] && send_batch
fi

log_info "Done!"
