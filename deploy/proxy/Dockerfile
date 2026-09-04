# The front door, and nothing else in the image that is not needed to run it.
#
# A separate image rather than a third `[[bin]]` in the daemon's, and the reason is the whole
# point of the component: this process opens no file, holds no lock and stores no credential.
# One image with both would make that unobservable from outside — `docker inspect` would show a
# `VOLUME` and a data directory that this binary never touches. Two images say what each is.
#
# It is also *much* smaller, and for a reason worth knowing: `big-proxy` depends on `big-wire`
# and `big-tls` and nothing else. No engine, no SQL planner, no argon2. CI asserts that.

FROM rust:1.88-bookworm AS build
WORKDIR /src

# The manifests first, so a change to the source does not throw away the dependency build.
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY bench ./bench
# Workspace members whose directories must exist for cargo to open the workspace at all, even
# though nothing here is built from them. Same reason the daemon's Dockerfile copies them.
COPY contrib ./contrib

# `--locked` so the image is built from the lockfile in the repository rather than from whatever
# resolved today. `--features tls` because a front door that cannot terminate TLS is a front
# door that needs another one in front of it.
RUN cargo build --release --locked --features tls --bin bigproxy

FROM debian:bookworm-slim
# `curl` for HEALTHCHECK, `setpriv` for the privilege drop after reading a key whose ownership
# this image cannot predict. The same two packages the daemon's image takes, for the same two
# reasons.
RUN apt-get update \
 && apt-get install -y --no-install-recommends curl ca-certificates util-linux \
 && rm -rf /var/lib/apt/lists/*

# **No `/data` and no `VOLUME`.** This process has nothing to persist. `/run/bigproxy` is where
# the entrypoint stages a private key, because a bind mount does not always carry file modes and
# `--tls-key` is held to the same `600` rule `big serve` holds its own to.
RUN useradd --uid 1000 --create-home --shell /usr/sbin/nologin bigproxy \
 && mkdir -p /run/bigproxy \
 && chown bigproxy:bigproxy /run/bigproxy \
 && chmod 700 /run/bigproxy

COPY --from=build /src/target/release/bigproxy /usr/local/bin/bigproxy
COPY deploy/proxy/entrypoint.sh /usr/local/bin/bigproxy-entrypoint

EXPOSE 7650

# Liveness, not readiness. A proxy whose upstreams are all down is still a healthy proxy — it is
# answering, and it is answering `503`, which is the correct answer. Restarting it would not
# bring a database back, and `/ready` is where an orchestrator asks the other question.
HEALTHCHECK --interval=10s --timeout=2s --start-period=5s --retries=3 \
  CMD curl -fsS http://127.0.0.1:7650/health || exit 1

ENTRYPOINT ["/usr/local/bin/bigproxy-entrypoint"]
CMD ["0.0.0.0:7650"]
