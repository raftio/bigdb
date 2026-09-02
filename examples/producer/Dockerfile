# The `produce` example, as a container, so trying this needs docker and nothing else.
#
# Separate from the root Dockerfile on purpose: that one builds the product, and an example
# binary has no business in the image somebody deploys. The build stage is the same shape, so
# the two share a layer cache when the context is the same.

FROM rust:1.88-bookworm AS build
WORKDIR /src

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY bench ./bench
COPY contrib ./contrib

# `--locked`, like the root image: what is built here is what the lockfile says, or the build
# fails somewhere a diff can fix it.
RUN cargo build --release --locked -p big-message --example produce

FROM debian:bookworm-slim
# Nothing installed. `big-message` has an empty `[dependencies]` section and speaks HTTP/1.1
# over a plain socket, so there is no TLS stack to carry and no certificates to trust.
COPY --from=build /src/target/release/examples/produce /usr/local/bin/produce
# Not root, for the same reason the daemon is not: this process needs a socket and nothing else.
USER 1000:1000
ENTRYPOINT ["/usr/local/bin/produce"]
