#!/usr/bin/env bash
# Install the runtime dependencies inside a fresh Debian 12 container.
#
#   ssh root@hyper.luxuride.eu 'pct exec 118 -- bash -s' < recommender/deploy/lxc/install.sh
#
# Idempotent: safe to re-run. Creates the `nodus` role and database but never drops
# either, because re-running the installer after an upgrade must not delete a feed.
set -euo pipefail

DB_USER="${DB_USER:-nodus}"
DB_NAME="${DB_NAME:-nodus}"
DB_PASSWORD="${DB_PASSWORD:-nodus}"

echo "==> packages"
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq postgresql postgresql-contrib redis-server ca-certificates curl

echo "==> postgres role and database"
su - postgres -c "psql -tAc \"SELECT 1 FROM pg_roles WHERE rolname = '${DB_USER}'\"" | grep -q 1 ||
  su - postgres -c "psql -c \"CREATE ROLE ${DB_USER} LOGIN SUPERUSER PASSWORD '${DB_PASSWORD}'\""
su - postgres -c "psql -tAc \"SELECT 1 FROM pg_database WHERE datname = '${DB_NAME}'\"" | grep -q 1 ||
  su - postgres -c "createdb -O ${DB_USER} ${DB_NAME}"

# SUPERUSER on purpose: `sqlx migrate` creates the `citext` and `pg_trgm` extensions,
# and granting that later is a second thing to remember at deploy time.
su - postgres -c "psql -d ${DB_NAME} -c 'CREATE EXTENSION IF NOT EXISTS citext' -c 'CREATE EXTENSION IF NOT EXISTS pg_trgm'"

echo "==> redis"
systemctl enable --now redis-server
redis-cli ping

echo "==> service user"
id -u nodus >/dev/null 2>&1 || useradd --system --create-home --shell /usr/sbin/nologin nodus

echo "==> done: $(su - postgres -c 'psql -tAc "SHOW server_version"') postgres"
