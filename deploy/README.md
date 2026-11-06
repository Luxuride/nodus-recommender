# Deploying nodus

Two roles, one image, one database.

```text
┌────────────┐  HTTP   ┌───────────────┐   reads   ┌────────────┐
│  browser   │────────▶│  feeds (N)    │──────────▶│            │
│ (CDN asset)│         │  stateless    │           │  Postgres  │
└────────────┘         └───────────────┘           │  (1 primary)│
       │                                          │            │
       │ direct to instances                      │            │
       ▼                                          │            │
┌────────────┐         ┌───────────────┐   writes  │            │
│ Mastodon   │◀────────│ calculator (M)│──────────▶│            │
│ instances  │ 1 req/s │  leased jobs  │           └────────────┘
└────────────┘  /host   └───────────────┘
```

## Why one image

`feeds` and `calculator` are built from the same crates and differ in which binary runs,
so the image carries both and the deployment picks one with `BINARY`. A rollback of a
single role is then a rollout restart, not a second image build.

## Apply

```sh
kubectl apply -k deploy/k8s
```

`secret.example.yaml` is **not** in the kustomization on purpose: applying it to a real
cluster would install placeholder credentials. Create the secret from your manager of
choice, then apply the rest:

```sh
kubectl -n nodus create secret generic nodus-secrets \
  --from-literal=database-url="postgres://nodus_feeds:...@postgres:5432/nodus" \
  --from-literal=token-enc-key="$(head -c 32 /dev/urandom | base64)"
```

## What the manifests deliberately do not include

- **Redis.** The M1 calculator runs one replica with in-process budgets, which is
  correct for one replica. Set `REDIS_URL` and raise `CALCULATOR_REPLICAS` together, or
  the service refuses to start — see `docs/02-architecture.md` § The seam.
- **Postgres.** A production deployment runs a managed or operator-managed primary with
  a replica; the manifests assume `postgres.nodus.svc` exists and do not create it.
- **Ingress.** The API is plain HTTP on port 80 inside the cluster; TLS termination and
  the CDN origin for the client are deployment-specific.
- **Migrations as a Job.** `sqlx::migrate!` runs at boot, and the first replica to start
  applies what is pending. With more than one replica starting at once this is safe
  (Postgres serialises the migration table lock) but it is not a substitute for a
  deliberate migration step once the schema change is not additive.
