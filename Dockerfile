# One image, two roles.
#
# `feeds` and `calculator` differ in how much work they do per request, not in what they
# are built from, so they ship as one image with two entry points. The role is chosen by
# the deployment (`BINARY`), so a rollback of one role does not require a second image to
# have been built and promoted.
#
# Build context is the **superproject**, not this directory:
#
#     docker build -f recommender/Dockerfile -t nodus-recommender .
#
# The recommendation service depends on the SDK by path (`../sdk/crates/...`), so a build
# context that excluded the SDK could not resolve its own manifests. That is a
# consequence of D14 — path dependencies over git URLs — and it is the right trade: the
# image is built from one commit that pins `sdk`, `recommender` and `web` together.

# --- build ---------------------------------------------------------------------------
FROM rust:1-bookworm AS build

WORKDIR /src

# Manifests and lockfiles first: a source-only change must not invalidate the dependency
# layer. Every target those manifests name has to exist for `cargo` to resolve the
# workspaces at all, which is why the stubs below are the only sources copied here.
COPY sdk/Cargo.toml sdk/Cargo.lock ./sdk/
COPY sdk/crates/shared/Cargo.toml ./sdk/crates/shared/Cargo.toml
COPY sdk/crates/instance/Cargo.toml ./sdk/crates/instance/Cargo.toml
COPY sdk/crates/sdk/Cargo.toml ./sdk/crates/sdk/Cargo.toml
COPY recommender/Cargo.toml recommender/Cargo.lock ./recommender/
COPY recommender/crates/core/Cargo.toml ./recommender/crates/core/Cargo.toml
COPY recommender/crates/store/Cargo.toml ./recommender/crates/store/Cargo.toml
COPY recommender/crates/feeds/Cargo.toml ./recommender/crates/feeds/Cargo.toml
COPY recommender/crates/calculator/Cargo.toml ./recommender/crates/calculator/Cargo.toml
RUN for crate in shared instance sdk; do \
        mkdir -p "sdk/crates/$crate/src" && echo '' > "sdk/crates/$crate/src/lib.rs"; \
    done \
    && for crate in core store calculator; do \
        mkdir -p "recommender/crates/$crate/src" \
        && echo '' > "recommender/crates/$crate/src/lib.rs"; \
    done \
    && mkdir -p recommender/crates/feeds/src recommender/crates/calculator/src/bin \
    && printf 'fn main() {}\n' > recommender/crates/feeds/src/main.rs \
    && printf 'fn main() {}\n' > recommender/crates/calculator/src/bin/calculator.rs \
    && cargo build --release --locked --manifest-path recommender/Cargo.toml --workspace \
    && rm -rf sdk/crates recommender/crates

# Real sources. The dependency artefacts above stay warm, so this step compiles only our
# crates.
COPY sdk/crates ./sdk/crates
COPY recommender/crates ./recommender/crates
# `COPY` preserves the host mtimes, which can be *older* than the stub artefacts built
# above — and cargo would then decide the real sources are already compiled and keep the
# stubs. Touch them so the fingerprint reflects "these files are new".
RUN find sdk/crates recommender/crates -name '*.rs' -exec touch {} + \
    && cargo build --release --locked \
        --manifest-path recommender/Cargo.toml \
        -p nodus-feeds -p nodus-calculator \
    && strip recommender/target/release/nodus-feeds \
        recommender/target/release/nodus-calculator

# --- runtime -------------------------------------------------------------------------
FROM debian:bookworm-slim AS runtime

# `ca-certificates` is not optional: every request this service makes is TLS to an
# instance whose certificate we have not pinned.
RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --create-home --shell /usr/sbin/nologin nodus

COPY --from=build /src/recommender/target/release/nodus-feeds /usr/local/bin/nodus-feeds
COPY --from=build /src/recommender/target/release/nodus-calculator /usr/local/bin/nodus-calculator

# No migrations are copied: `sqlx::migrate!` embeds them in the binary at compile time, so
# a deployment can never run a migration set that differs from its code.

USER nodus
ENV RUST_LOG=info,sqlx=warn \
    BINARY=nodus-feeds

# Only `feeds` listens; the calculator serves nothing.
EXPOSE 3000

ENTRYPOINT ["/bin/sh", "-c", "exec /usr/local/bin/$BINARY"]
