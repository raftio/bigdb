# The Python client's own gates. The root Makefile delegates here so `make py-check` works
# from either place and means the same thing.

PY      ?= python3
REPO    := $(CURDIR)/../..

.DEFAULT_GOAL := help

help:
	@echo 'make lint     ruff format --check, ruff check, mypy --strict'
	@echo 'make test     the suite, without the integration tests'
	@echo 'make e2e      build `big`, then run only the integration tests'
	@echo 'make cov      the suite with a coverage report'
	@echo 'make check    lint + test'
	@echo 'make dev      install the dev dependencies into the current environment'

dev:
	$(PY) -m pip install -e '.[dev]'

# fmt before lint, and lint before types, for the reason the root Makefile orders its own the
# same way: a formatting failure is one command to fix and would otherwise hide at the bottom
# of output nobody reads to the end of.
lint:
	$(PY) -m ruff format --check .
	$(PY) -m ruff check .
	$(PY) -m mypy bigdb

test:
	$(PY) -m pytest

cov:
	$(PY) -m pytest --cov=bigdb --cov-report=term-missing

# Builds first, because the fixture deliberately does not: a pytest run that started a cold
# cargo build would take minutes and would fail outright without a Rust toolchain.
# PROFILE is passed through rather than assumed: the root Makefile defaults to debug, and
# pointing BIGDB_BIN at a release path it did not build is how this target would silently skip.
PROFILE ?= release

e2e:
	$(MAKE) -C $(REPO) build PROFILE=$(PROFILE)
	BIGDB_BIN=$(REPO)/target/$(PROFILE)/big $(PY) -m pytest -m integration

check: lint test

.PHONY: help dev lint test cov e2e check
