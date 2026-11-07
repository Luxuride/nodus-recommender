#!/usr/bin/env bash
# End-to-end smoke for the two services and the schema they share.
#
#   cd recommender && cargo build --bins && bash scripts/smoke.sh
#
# It starts `nodus-feeds` and `nodus-calculator` against the local Postgres,
# exercises the routes a client actually calls, then queues one job and checks that
# the calculator claims it and fails it the way the handler says it should. This is
# a working-version check, not a test suite: it proves the wiring survives real
# startup, real SQL and a real shutdown, which `cargo test` cannot.
#
# A `sync_user` job deliberately has no subject in `app_user`, so the expected
# outcome is a *permanent* failure ("background sync is off for this user"). Replace
# the inserted row with a user that has a sealed token to exercise the network path.
set -uo pipefail

export DATABASE_URL="${DATABASE_URL:-postgres://nodus:nodus@host.docker.internal:5432/nodus}"
export TOKEN_ENC_KEY="${TOKEN_ENC_KEY:-AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=}"
export LISTEN_ADDR="${LISTEN_ADDR:-127.0.0.1:3000}"
export RUST_LOG="${RUST_LOG:-warn,nodus_feeds=info,nodus_calculator=info}"
export CALCULATOR_POLL_MS="${CALCULATOR_POLL_MS:-200}"
export CALCULATOR_LEASE_SECONDS="${CALCULATOR_LEASE_SECONDS:-60}"
export CALCULATOR_ID="smoke"

POSTGRES_CONTAINER="${POSTGRES_CONTAINER:-nodus-postgres-1}"

psql_local() { docker exec "$POSTGRES_CONTAINER" psql -U nodus -d nodus -tAc "$1"; }

./target/debug/nodus-feeds >/tmp/feeds.log 2>&1 &
FEEDS=$!
./target/debug/nodus-calculator >/tmp/calc.log 2>&1 &
CALC=$!
trap 'kill $FEEDS $CALC 2>/dev/null' EXIT

for _ in $(seq 1 40); do
    curl -sf http://127.0.0.1:3000/healthz >/dev/null && break
    sleep 0.25
done

echo "--- feeds"
echo "healthz            : HTTP $(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:3000/healthz)"
echo "readyz             : $(curl -s http://127.0.0.1:3000/readyz)"
echo "meta               : $(curl -s http://127.0.0.1:3000/api/v1/meta)"
echo "feed, no headers   : HTTP $(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:3000/api/v1/feed/accounts)"
echo "feed, old contract : $(curl -s -H 'x-contract-version: 0.0.1' http://127.0.0.1:3000/api/v1/feed/accounts)"
echo "feed, unknown host : $(curl -s -H 'x-instance-host: nope.invalid' -H 'authorization: Bearer x' http://127.0.0.1:3000/api/v1/feed/accounts | head -c 200)"
echo "events, bad body   : HTTP $(curl -s -o /dev/null -w '%{http_code}' -X POST -H 'x-instance-host: nope.invalid' -H 'authorization: Bearer x' -H 'content-type: application/json' -d '{}' http://127.0.0.1:3000/api/v1/events)"

echo "undo, no headers  : HTTP $(curl -s -o /dev/null -w '%{http_code}' -X DELETE http://127.0.0.1:3000/api/v1/feedback/1)"
echo "undo, bad id      : HTTP $(curl -s -o /dev/null -w '%{http_code}' -X DELETE -H 'x-instance-host: nope.invalid' -H 'authorization: Bearer x' http://127.0.0.1:3000/api/v1/feedback/not-a-number)"

echo "--- shared links"
echo "share, bad host    : HTTP $(curl -s -o /dev/null -w '%{http_code}' 'http://127.0.0.1:3000/s/%2E%2E%2Fx/1')"
echo "share, non-digit id: HTTP $(curl -s -o /dev/null -w '%{http_code}' 'http://127.0.0.1:3000/s/mastodon.social/abc')"
echo "share, unreachable : HTTP $(curl -s -o /dev/null -w '%{http_code}' 'http://127.0.0.1:3000/s/does-not-exist.invalid/1')"
echo "share, og tags     : $(curl -s 'http://127.0.0.1:3000/s/does-not-exist.invalid/1' | grep -c 'og:') og: tags, $(curl -s -o /dev/null -w '%{content_type}' 'http://127.0.0.1:3000/s/does-not-exist.invalid/1')"

echo "--- queue"
psql_local "INSERT INTO job (kind, priority, subject_kind, subject_id) VALUES ('sync_user', 0, 'user', gen_random_uuid()::text)" >/dev/null
sleep 2
psql_local "SELECT kind, status, attempts, left(coalesce(last_error, ''), 60) FROM job ORDER BY id DESC LIMIT 1" | tr '|' ' '
echo "--- calculator log"
grep -E "claimed|failed|retry|stopped" /tmp/calc.log | tail -3

echo "--- schema"
psql_local "SELECT count(*) || ' recommendation_item rows' FROM recommendation_item"
