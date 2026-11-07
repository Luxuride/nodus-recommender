#!/usr/bin/env bash
# Write `/etc/nodus.env` and the two systemd units.
#
#   ssh root@hyper.luxuride.eu 'pct exec 118 -- bash -s' < recommender/deploy/lxc/setup.sh
#
# The token key is generated once and kept: replacing it makes every stored Mastodon
# token undecryptable, which looks exactly like every user revoking access at once.
set -euo pipefail

ENV_FILE=/etc/nodus.env
PUBLIC_BASE_URL="${PUBLIC_BASE_URL:-http://10.0.1.18:3000}"
WEB_ORIGINS="${WEB_ORIGINS:-http://10.0.1.18:8080}"

if [ ! -f "${ENV_FILE}" ]; then
  umask 077
  cat >"${ENV_FILE}" <<EOF
DATABASE_URL=postgres://nodus:nodus@127.0.0.1:5432/nodus
LISTEN_ADDR=0.0.0.0:3000
REDIS_URL=redis://127.0.0.1:6379
TOKEN_ENC_KEY=$(openssl rand -base64 32)
PUBLIC_BASE_URL=${PUBLIC_BASE_URL}
WEB_ORIGINS=${WEB_ORIGINS}
RUST_LOG=info,nodus_feeds=debug,nodus_calculator=debug,sqlx=warn
FEEDS_DB_MAX_CONNECTIONS=10
CALCULATOR_DB_MAX_CONNECTIONS=4
CALCULATOR_REPLICAS=1
POST_RETENTION_DAYS=14
EVENT_RETENTION_DAYS=90
EOF
  echo "==> wrote ${ENV_FILE}"
else
  echo "==> kept existing ${ENV_FILE}"
fi
chmod 0600 "${ENV_FILE}"

unit() {
  local role="$1" restart="$2"
  cat >"/etc/systemd/system/nodus-${role}.service" <<EOF
[Unit]
Description=nodus ${role}
After=network-online.target postgresql.service redis-server.service
Wants=network-online.target

[Service]
Type=simple
User=nodus
Group=nodus
EnvironmentFile=${ENV_FILE}
ExecStart=/usr/local/bin/nodus-${role}
Restart=always
RestartSec=${restart}
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
StateDirectory=nodus
ReadWritePaths=/var/lib/nodus

[Install]
WantedBy=multi-user.target
EOF
}

# Names served by the Caddy container on this bridge.
#
# Public DNS points these at the host's public address, and hairpin NAT does not
# reach that address from inside the bridge, so a service that verifies a token by
# calling its own instance would fail with a 503 on every request. The local
# address is the same Caddy instance, with the same certificates.
if ! grep -q "mastodon.luxuride.eu" /etc/hosts; then
	cat >> /etc/hosts <<'HOSTS'

# Names served by the Caddy container on this bridge (see deploy/lxc/README.md).
10.0.1.2 nodus.luxuride.eu mastodon.luxuride.eu hyper.luxuride.eu luxuride.eu
HOSTS
fi

unit feeds 2
unit calculator 5
systemctl daemon-reload
systemctl enable nodus-feeds nodus-calculator
echo "==> units written and enabled"
