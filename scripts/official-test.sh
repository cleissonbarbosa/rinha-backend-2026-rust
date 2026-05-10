#!/usr/bin/env sh
set -eu

MODE="${1:-full}"
OFFICIAL_DIR="${OFFICIAL_DIR:-../rinha-de-backend-2026}"

if [ ! -d "$OFFICIAL_DIR/.git" ]; then
  git clone --depth 1 https://github.com/zanfranceschi/rinha-de-backend-2026.git "$OFFICIAL_DIR"
fi

case "$MODE" in
  smoke)
    cd "$OFFICIAL_DIR"
    K6_NO_USAGE_REPORT=true k6 run test/smoke.js
    ;;
  full)
    cd "$OFFICIAL_DIR"
    K6_WEB_DASHBOARD=true K6_NO_USAGE_REPORT=true k6 run test/test.js >/dev/null 2>&1
    cat test/results.json
    ;;
  *)
    echo "usage: $0 [smoke|full]" >&2
    exit 2
    ;;
esac

