#!/usr/bin/env bash
# End-to-end: the release binary against a mock Riot + Telegram (tests/e2e/mock.py).
# Exercises the real HTTP transport, both threads and the scheduler. Needs python3.
set -euo pipefail
cd "$(dirname "$0")/../.."
cargo build --release --quiet
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
port=$((20000 + RANDOM % 10000))
cat > "$work/config.toml" <<TOML
[riot]
poll_interval_seconds = 10
[notifications]
session_gap_minutes = 1
flush_recheck_minutes = 1
[weekly]
timezone = "UTC"
[telegram]
admin_user_ids = [4242]
TOML
python3 -I tests/e2e/mock.py "$port" 40 > "$work/mock.out" 2>/dev/null &
sleep 1
RIOT_API_KEY=RGAPI-e2e TELEGRAM_BOT_TOKEN=TEST:token LOLWATCH_DB="$work/e2e.db" \
  LOLWATCH_CONFIG="$work/config.toml" \
  LOLWATCH_RIOT_BASE_URL="http://127.0.0.1:$port" LOLWATCH_TELEGRAM_BASE_URL="http://127.0.0.1:$port" \
  timeout 36 ./target/release/lolwatch > "$work/bot.out" 2>&1 || true
wait
fail=0
for want in "will now get session" "Now tracking <b>Alice#EUW</b> (euw1, level 321)" "Imported 1 recent game." \
            "last game 2d ago" "Ahri · Ranked Solo · LOSS 7/3/9 · 33m" "not in game" "<b>Commands</b>"; do
  if grep -qF "$want" "$work/mock.out"; then echo "ok   $want"; else echo "MISSING $want"; fail=1; fi
done
grep -q "(server limit)); waiting" "$work/bot.out" && echo "ok   429 'server' waited out" || { echo "MISSING 429 retry"; fail=1; }
if grep -q "Poller error" "$work/mock.out"; then echo "UNEXPECTED alert"; fail=1; fi
[ "$fail" = 0 ] && echo "e2e: PASS" || { echo "e2e: FAIL"; cat "$work/bot.out"; exit 1; }
