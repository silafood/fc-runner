#!/bin/bash
# Fetch runner environment from Firecracker MMDS (169.254.169.254)
# Falls back to /etc/fc-runner-env if MMDS is unavailable.
# This script writes /etc/fc-runner-env from MMDS data.

MMDS_URL="http://169.254.169.254"
ENV_FILE="/etc/fc-runner-env"
TOKEN_HEADER="X-metadata-token"
MAX_RETRIES=30
RETRY_INTERVAL=1

# Try MMDS first
for i in $(seq 1 $MAX_RETRIES); do
    # Acquire MMDS v2 session token
    TOKEN=$(curl -s -X PUT "${MMDS_URL}/latest/api/token" \
        -H "X-metadata-token-ttl-seconds: 300" 2>/dev/null)

    if [ -n "$TOKEN" ]; then
        # Fetch metadata
        METADATA=$(curl -s -H "${TOKEN_HEADER}: ${TOKEN}" \
            "${MMDS_URL}/latest/meta-data/" 2>/dev/null)

        if [ -n "$METADATA" ] && [ "$METADATA" != "Not Found" ]; then
            # Parse JSON and write as KEY=VALUE
            echo "$METADATA" | jq -r 'to_entries[] | "\(.key | ascii_upcase)=\(.value)"' > "$ENV_FILE"
            chmod 600 "$ENV_FILE"
            echo "MMDS: environment loaded from metadata service"
            exit 0
        fi
    fi

    if [ $i -lt $MAX_RETRIES ]; then
        sleep $RETRY_INTERVAL
    fi
done

# Fall back to on-disk env file
if [ -f "$ENV_FILE" ]; then
    echo "MMDS: unavailable, using on-disk $ENV_FILE"
    exit 0
fi

echo "ERROR: No environment source available (MMDS and $ENV_FILE both missing)" >&2
exit 1
