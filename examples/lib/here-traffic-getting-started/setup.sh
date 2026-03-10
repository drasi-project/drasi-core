#!/bin/bash
set -e

echo "=== HERE Traffic Source Setup ==="

if [ -n "$HERE_API_KEY" ]; then
    echo "✓ Using API key authentication"
    if [ ${#HERE_API_KEY} -lt 20 ]; then
        echo "⚠️  WARNING: API key seems too short (< 20 chars)"
    fi
elif [ -n "$HERE_ACCESS_KEY_ID" ] && [ -n "$HERE_ACCESS_KEY_SECRET" ]; then
    echo "✓ Using OAuth 2.0 authentication"
else
    echo "❌ ERROR: No HERE credentials found"
    echo ""
    echo "   Option A - API key:"
    echo "     export HERE_API_KEY=your_key_here"
    echo ""
    echo "   Option B - OAuth 2.0:"
    echo "     export HERE_ACCESS_KEY_ID=your_access_key_id"
    echo "     export HERE_ACCESS_KEY_SECRET=your_access_key_secret"
    echo ""
    echo "   Get credentials from: https://developer.here.com/"
    exit 1
fi

BBOX=${HERE_BBOX:-"52.5,13.3,52.6,13.5"}
echo "Using bounding box: $BBOX"

if [ -n "$HERE_API_KEY" ]; then
    echo "Testing HERE API connectivity..."
    for i in {1..60}; do
        RESPONSE=$(curl -s -o /dev/null -w "%{http_code}" \
            "https://data.traffic.hereapi.com/v7/flow?in=bbox:${BBOX}&apiKey=${HERE_API_KEY}")

        if [ "$RESPONSE" = "200" ]; then
            echo "✓ API connectivity OK"
            break
        elif [ "$RESPONSE" = "401" ]; then
            echo "❌ ERROR: API key authentication failed (401)"
            exit 1
        elif [ "$RESPONSE" = "429" ]; then
            echo "⚠️  WARNING: Rate limit hit (429)"
            exit 1
        fi

        if [ "$i" -eq 60 ]; then
            echo "❌ ERROR: Unexpected response code: $RESPONSE"
            exit 1
        fi

        if [ $((i % 10)) -eq 0 ]; then
            echo "  Still waiting... ($i/60)"
        fi
        sleep 1
    done
else
    echo "ℹ️  OAuth connectivity will be tested at runtime (token exchange required)"
fi

echo "Building example..."
cargo build --release

echo ""
echo "=== Setup Complete ==="
echo "Next: Run ./quickstart.sh to start the example"
