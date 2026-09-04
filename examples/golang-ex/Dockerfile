# The Go example, as a container, so trying this needs docker and nothing else.
#
# Separate from the root Dockerfile for the same reason examples/producer's is: that one builds
# the product, and an example binary has no business in the image somebody deploys.

FROM golang:1.23-bookworm AS build
WORKDIR /src

# Both modules, because go.mod replaces the client with the copy in this repository - so what
# the demo exercises is the client as it is right now, not the last one published.
COPY clients/go ./clients/go
COPY examples/golang-ex ./examples/golang-ex

WORKDIR /src/examples/golang-ex
# No `go mod download` and no go.sum: the client has an empty require list beyond itself and CI
# fails if one appears. A local replace needs no checksum, so there is nothing to fetch.
#
# CGO off because nothing here needs libc: the client speaks HTTP/1.1 over a plain socket and
# has no TLS stack to link against unless the address says https, which crypto/tls provides in
# pure Go anyway.
RUN CGO_ENABLED=0 go build -trimpath -o /out/goex .

FROM debian:bookworm-slim
COPY --from=build /out/goex /usr/local/bin/goex
# Not root, for the same reason the daemon is not: this process needs a socket and nothing else.
USER 1000:1000
ENTRYPOINT ["/usr/local/bin/goex"]
