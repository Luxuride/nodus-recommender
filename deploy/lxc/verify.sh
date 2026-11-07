#!/usr/bin/env bash
# Check a running deployment from the inside.
#
#   ssh root@hyper.luxuride.eu 'pct exec 118 -- bash -s' < recommender/deploy/lxc/verify.sh
#
# Every line prints something a human can read and a script can grep: the three HTTP
# endpoints, both units, and the tail of the calculator log where the maintenance tick
# and the retention pass announce themselves.
set -euo pipefail

BASE="${BASE:-http://127.0.0.1:3000}"

echo "== units"
systemctl is-active nodus-feeds nodus-calculator

echo "== HTTP"
for path in healthz readyz api/v1/meta; do
  printf '%-14s ' "${path}"
  curl -fsS -o /dev/stdout -w ' [%{http_code}]\n' "${BASE}/${path}" || echo " [failed]"
done

echo "== feed without credentials (expects 400)"
curl -sS -o /dev/null -w '%{http_code}\n' "${BASE}/api/v1/feed/accounts"

echo "== calculator log"
journalctl -u nodus-calculator --no-pager -n 12 -o cat || true
