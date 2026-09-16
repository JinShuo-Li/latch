#!/bin/sh
# Deterministic dogfood test entrypoint (test image stage only).
#
# Starts the loopback mock provider (unless disabled), writes a provider-neutral
# config pointing at it, then execs the real machine CLI so its exit status is
# the container's exit status.
#
# Environment:
#   DOGFOOD_MOCK          1 (default) to start the mock provider, 0 to skip it
#   DOGFOOD_MOCK_SCRIPT   script JSON (default /mock/script.json)
#   DOGFOOD_MOCK_PORT     loopback port (default 8731)
#   DOGFOOD_BASE_URL      provider base URL when the mock is disabled
#   DOGFOOD_SAFETY        safety level (default standard)
#   DOGFOOD_PROMPT        prompt text (default "Say hello")
#   DOGFOOD_OUTPUT        output format (default json)
#   DOGFOOD_WORKSPACE     workspace path (default /workspace)
#   DOGFOOD_STATE         state dir (default /state)
set -eu

PORT="${DOGFOOD_MOCK_PORT:-8731}"
STATE="${DOGFOOD_STATE:-/state}"
WORKSPACE="${DOGFOOD_WORKSPACE:-/workspace}"
CONFIG="${DOGFOOD_CONFIG:-/tmp/dogfood-config.toml}"

if [ "${DOGFOOD_MOCK:-1}" = "1" ]; then
    SCRIPT="${DOGFOOD_MOCK_SCRIPT:-/mock/script.json}"
    if [ ! -f "$SCRIPT" ]; then
        echo "mock script not found: $SCRIPT" >&2
        exit 1
    fi
    latch-mock-provider --script "$SCRIPT" --port "$PORT" --host 127.0.0.1 \
        >/tmp/mock-provider.log 2>&1 &
    tries=0
    while ! grep -q "MOCK_PROVIDER_READY" /tmp/mock-provider.log 2>/dev/null; do
        tries=$((tries + 1))
        if [ "$tries" -gt 100 ]; then
            echo "mock provider did not become ready:" >&2
            cat /tmp/mock-provider.log >&2 || true
            exit 1
        fi
        sleep 0.1
    done
    BASE_URL="http://127.0.0.1:${PORT}/v1"
else
    BASE_URL="${DOGFOOD_BASE_URL:-http://127.0.0.1:1/v1}"
fi

mkdir -p "$STATE"
cat > "$CONFIG" <<EOF
state_dir = "$STATE"
default_mode = "WORK"

[providers.mock]
kind = "openai-compatible"
base_url = "$BASE_URL"
credential = "env:MOCK_API_KEY"
default_model = "mock-model"

[inference]
provider = "mock"
model = "mock-model"

[permissions]
mode = "human"

[safety]
level = "${DOGFOOD_SAFETY:-standard}"
EOF

# A fixed test credential keeps the run deterministic and is never printed.
export MOCK_API_KEY="${MOCK_API_KEY:-test-secret-key}"

exec latch run \
    --config "$CONFIG" \
    --workspace "$WORKSPACE" \
    --prompt "${DOGFOOD_PROMPT:-Say hello}" \
    --output "${DOGFOOD_OUTPUT:-json}"
