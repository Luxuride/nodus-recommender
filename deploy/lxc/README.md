# Deploying to an LXC container

The reference deployment is a Debian 12 LXC on Proxmox with PostgreSQL, Redis and both
binaries on one machine. It is deliberately unglamorous: one container, two systemd
units, `restart=always`. It exists so the engine can be exercised against a real
instance before anything is clustered.

`deploy/k8s/` describes the scaled topology (N feeds replicas, M calculator replicas,
Redis for the shared budgets); this directory describes the smallest thing that runs.

## The container

```sh
pct create 118 local:vztmpl/debian-12-standard_12.7-1_amd64.tar.zst \
  --hostname nodus --cores 4 --memory 4096 --swap 1024 --rootfs local:8 \
  --net0 name=eth0,bridge=vmbr1,ip=10.0.1.18/24,gw=10.0.1.1,type=veth \
  --unprivileged 1 --features nesting=1,keyctl=1 --onboot 1 --tags app --start 1
```

Unprivileged with `nesting=1` matches the other application containers on the host;
neither the service nor its units need any capability beyond filesystem access to
`/var/lib/nodus`.

## Scripts

Run them in this order, each piped into `pct exec` so nothing has to be copied in:

```sh
ssh root@hyper.luxuride.eu 'pct exec 118 -- bash -s' < recommender/deploy/lxc/install.sh
ssh root@hyper.luxuride.eu 'pct exec 118 -- bash -s' < recommender/deploy/lxc/setup.sh
./recommender/deploy/lxc/deploy.sh          # builds for bookworm, pushes, restarts
ssh root@hyper.luxuride.eu 'pct exec 118 -- bash -s' < recommender/deploy/lxc/verify.sh
```

* `install.sh` — packages, the `nodus` role (SUPERUSER, because the migrations create
  `citext` and `pg_trgm`), the `nodus` database, Redis, and the service user.
* `setup.sh` — `/etc/nodus.env` (mode 0600, `TOKEN_ENC_KEY` generated once and never
  rotated) and the two hardened units.
* `deploy.sh` — builds inside `rust:1-bookworm` and replaces the binaries.
* `verify.sh` — endpoints, unit states, calculator log.

Curl from inside the container:

```sh
curl -s localhost:3000/healthz
curl -s localhost:3000/readyz
curl -s localhost:3000/api/v1/meta
```

## Why the build runs in a container

The development container is trixie (glibc 2.38+); the LXC is Debian 12 (glibc 2.36).
A binary built in the development container starts and dies:

```text
/usr/local/bin/nodus-feeds: /lib/x86_64-linux-gnu/libc.so.6:
version `GLIBC_2.38' not found (required by /usr/local/bin/nodus-feeds)
```

which reads like a corrupt upload and is really a build-target mismatch. `deploy.sh`
therefore builds in `rust:1-bookworm`, the same base as `recommender/Dockerfile`.

## What this deployment does not have

* **TLS and a hostname.** The API listens on plain HTTP on the internal bridge. Putting
  it behind the reverse proxy needs a hostname, a certificate and the matching
  `PUBLIC_BASE_URL` / `WEB_ORIGINS` pair — the web client is served from a different
  origin, so CORS is a deployment decision, not a build one.
* **Backups.** `pg_dump` on a schedule is the whole story; the 7-day restore guarantee in
  `docs/08-privacy.md` assumes it exists.
* **PostgreSQL tuning.** The defaults are used; `shared_buffers` and
  `effective_cache_size` are set in `compose.yml` for development and should be set here
  once the instance has real users.
* **A second calculator replica.** One replica does not need Redis for correctness, only
  for restarts; `CALCULATOR_REPLICAS` exists so the check in `Config::validate` fires
  before the budgets are silently multiplied.
