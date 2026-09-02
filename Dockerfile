# The daemon, and nothing else in the image that is not needed to run it.
#
# Two stages: the first has a Rust toolchain and the source, the second has neither. What ships
# is one static-ish binary, its C runtime, and a client for the health check - see below.

# Pinned to the version `rust-version` promises, so a build here fails for the same reason CI's
# MSRV job does rather than succeeding on something newer and failing there.
FROM rust:1.88-bookworm AS build
WORKDIR /src

# The manifests first, so a change to the source does not throw away the dependency build. The
# crates are all local path dependencies, so this is a smaller win than it is for most projects
# and it is still the difference between a rebuild and a recompile.
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY bench ./bench
# Not built into this image, and copied anyway: `contrib/*` are workspace members, and cargo
# loads every member's manifest before it decides which package to build. A member whose
# directory is missing is a workspace that does not open, so leaving these out fails the build
# below rather than making it smaller.
COPY contrib ./contrib

# `--locked` so the image is built from the lockfile in the repository and not from whatever
# resolved today. A build that needs a newer dependency should fail here and be fixed in the
# lockfile, where the change is reviewable.
#
# `big-bench` is excluded on purpose: it pulls in rival engines to measure against and is no
# part of the product.
# Both binaries. `big` serves the file and backs it up - those were two executables until they
# became two subcommands of one - and `bigctl` is the client, shipped because an image with no
# client is an image you cannot ask anything of from inside.
RUN cargo build --release --locked --bin big --bin bigctl

FROM debian:bookworm-slim
# `curl` is here for one reason: HEALTHCHECK needs a client, and this image ships a database
# rather than a client. Ten megabytes to make a container's health an observable fact rather
# than an assumption is worth it; if it is not worth it to you, drop the HEALTHCHECK and the
# package with it.
# `curl` is for HEALTHCHECK; `setpriv` is how the entrypoint drops privileges after reading a
# credential whose ownership it does not control. Both are why this is `slim` and not
# `distroless`: an image that cannot check its own health or drop its own privileges pushes
# both jobs onto whoever deploys it.
RUN apt-get update \
 && apt-get install -y --no-install-recommends curl ca-certificates util-linux \
 && rm -rf /var/lib/apt/lists/*

# Not root. The database is one file and the daemon needs to write it and nothing else.
#
# `/run/big` is where the entrypoint stages credentials: a bind mount does not always carry
# file modes - on Docker Desktop a `600` token file arrives as `0755` - and `big serve` refuses a
# token file anyone else can read. A directory in the container's own filesystem has a real
# mode, so the check passes without being weakened.
RUN useradd --uid 1000 --create-home --shell /usr/sbin/nologin big \
 && mkdir -p /data /run/big \
 && chown big:big /data /run/big \
 && chmod 700 /run/big

COPY --from=build /src/target/release/big /usr/local/bin/big
COPY --from=build /src/target/release/bigctl /usr/local/bin/bigctl
COPY deploy/entrypoint.sh /usr/local/bin/big-entrypoint

# **Root, and only until the entrypoint has read the credentials.** A bind-mounted token file
# arrives with the host's ownership, which the image cannot predict and `big serve` will not ignore;
# reading it as root and writing a private copy is what makes those two facts agree. The
# entrypoint drops to `big` before the daemon starts and stays there - see deploy/entrypoint.sh,
# which also handles being run unprivileged by a compose file that says so.
#
# The cost is that `docker exec` into this container lands as root, and it is worth naming
# rather than discovering: anybody who can `docker exec` already holds the daemon's socket,
# which is already root-equivalent on the host. It buys an attacker nothing they did not have.
WORKDIR /data
VOLUME ["/data"]
EXPOSE 7654

# Liveness, not readiness: a container that is up is a container that can answer. Whether it
# should be sent traffic is `/ready`, and that is an orchestrator's question rather than
# Docker's.
HEALTHCHECK --interval=10s --timeout=2s --start-period=5s --retries=3 \
  CMD curl -fsS http://127.0.0.1:7654/health || exit 1

# `0.0.0.0` because a container's loopback is its own. `big serve` refuses to bind anywhere but
# loopback without a token file, so the compose files mount one - see deploy/.
#
# The `serve` word is NOT here. The entrypoint puts it on, so that every `command:` in every
# compose file stays a list of arguments rather than gaining a word each - and so that there is
# one place to look when a container starts and exits two lines later.
ENTRYPOINT ["/usr/local/bin/big-entrypoint"]
CMD ["/data/big.db", "0.0.0.0:7654"]
