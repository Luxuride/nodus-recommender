#!/usr/bin/env bash
# Build the two binaries for Debian 12 (glibc 2.36) and push them into the container.
#
#   ./recommender/deploy/lxc/deploy.sh
#   HOST=root@hyper.luxuride.eu VMID=118 ./recommender/deploy/lxc/deploy.sh
#
# Why a container is used for the build: the development container is trixie, whose glibc
# is 2.38+, and a binary linked against it fails on Debian 12 with
#   /usr/local/bin/nodus-feeds: /lib/x86_64-linux-gnu/libc.so.6: version `GLIBC_2.38' not found
# which reads like a corrupted binary and is really a build-target mismatch.
set -euo pipefail

# Every ssh line interpolates the target on purpose: the local shell decides where the
# command runs, and SC2029 wants the opposite. Declared once for the whole file.
# shellcheck disable=SC2029

HOST="${HOST:-root@hyper.luxuride.eu}"
VMID="${VMID:-118}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
CACHE_VOLUME="${CACHE_VOLUME:-nodus-cargo}"

echo "==> building for bookworm in ${ROOT}"
docker run --rm \
  -v "${ROOT}:/src" \
  -v "${CACHE_VOLUME}:/cargo" \
  -w /src/recommender \
  -e CARGO_HOME=/cargo \
  rust:1-bookworm \
  cargo build --release --locked --bins --target-dir /src/recommender/target-bookworm

echo "==> pushing binaries to ${HOST} container ${VMID}"
scp -q "${ROOT}/recommender/target-bookworm/release/nodus-feeds" \
       "${ROOT}/recommender/target-bookworm/release/nodus-calculator" \
       "${HOST}:/tmp/"
for role in feeds calculator; do
  ssh "${HOST}" "pct push ${VMID} /tmp/nodus-${role} /usr/local/bin/nodus-${role} --perms 0755"
done

echo "==> restarting units"
ssh "${HOST}" "pct exec ${VMID} -- systemctl restart nodus-feeds nodus-calculator"
ssh "${HOST}" "pct exec ${VMID} -- systemctl --no-pager --lines=0 status nodus-feeds nodus-calculator" || true
echo "==> done"
