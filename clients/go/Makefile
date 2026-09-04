# The Go client. Kept out of the repository's `check` target on purpose: that one is Rust-only,
# and adding a Go toolchain requirement to it would break `make check` on a machine without one.

GO ?= go

.PHONY: help check fmt lint build test race cover e2e tidy deps tag clean

help:
	@echo "make fmt     - gofmt -w"
	@echo "make lint    - gofmt -l, go vet"
	@echo "make test    - go test -race -cover"
	@echo "make e2e     - build the server, then the integration tests"
	@echo "make deps    - assert this module has no dependencies"
	@echo "make check   - lint, then test, then deps"
	@echo "make tag     - print the tag a release needs (it is not just vX.Y.Z)"

# The order is the same one the repository's Rust Makefile uses for fmt before clippy: the
# cheapest gate that can fail runs first, so a formatting slip does not wait on a test run.
check: lint test deps

fmt:
	$(GO) fmt ./...

lint:
	@out=$$(gofmt -l .); if [ -n "$$out" ]; then echo "not gofmt'd:"; echo "$$out"; exit 1; fi
	$(GO) vet ./...

build:
	$(GO) build ./...

test:
	$(GO) test -race -cover -coverprofile=coverage.out ./...

cover: test
	$(GO) tool cover -func=coverage.out | tail -1
	$(GO) tool cover -html=coverage.out

# Integration needs a real `big`. The fixture deliberately does not build one - a test run that
# shells out to cargo takes minutes on a cold tree and fails outright without Rust - so the
# build is here, where it is visible.
e2e:
	$(MAKE) -C ../.. build
	$(GO) test -tags integration -count=1 ./...

tidy:
	$(GO) mod tidy

# The zero-dependency claim, asserted rather than stated. Mirrors the `deps` job in the
# repository's ci.yml.
deps:
	@test ! -f go.sum || { echo "go.sum exists: this module has taken a dependency"; exit 1; }
	@! grep -q '^require' go.mod || { echo "go.mod has a require block"; exit 1; }
	@$(GO) list -deps ./... \
	  | grep -v '^github.com/raftio/bigdb/clients/go' \
	  | while read -r p; do \
	      if [ "$$($(GO) list -f '{{.Standard}}' "$$p")" != "true" ]; then \
	        echo "not stdlib: $$p"; exit 1; \
	      fi; \
	    done
	@echo "no runtime dependencies"

# A nested module is tagged with its path. `v0.1.0` would publish the Rust repository's root,
# which has no Go in it, and `go get` would not find this package at all.
tag:
	@echo "git tag clients/go/vX.Y.Z && git push origin clients/go/vX.Y.Z"

clean:
	rm -f coverage.out
	$(GO) clean -testcache
