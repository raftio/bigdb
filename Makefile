# Running big on this machine: one daemon, one file, two binaries.
#
# `make serve` in one terminal and `make shell` in another is the whole workflow. Nothing here
# does anything the binaries cannot - every target is a spelling of `big` or `bigctl`, and a
# recipe that hides which one it ran would be another way to run the engine.

CARGO   ?= cargo
# Debug by default, because this is a local playground and a release build of the workspace
# costs minutes that iteration does not get back. `make serve PROFILE=release` when the numbers
# are the point - a debug build of a bitmap engine is not a measurement of anything.
PROFILE ?= debug
ADDR    ?= 127.0.0.1:7654
# Under `.local/`, which git ignores: a database file next to the source is a file somebody
# eventually commits.
RUN     ?= .local
DATA    ?= $(RUN)/data.big
# off|error|warn|info|debug, read by `big serve` as BIG_LOG.
LOG     ?= info
# Anything else for the daemon: --tokens, --durability, --query-timeout, --cluster.
FLAGS   ?=
# Where `make install` puts the two binaries. Anyone who has cargo already has this on PATH.
PREFIX  ?= $(HOME)/.cargo/bin

BIG     := target/$(PROFILE)/big
BIGCTL  := target/$(PROFILE)/bigctl
PIDFILE := $(RUN)/big.pid
LOGFILE := $(RUN)/big.log
RELEASE := $(if $(filter release,$(PROFILE)),--release,)
# Every client call goes to the daemon this Makefile started, whatever ADDR was overridden to.
# One variable where there were two: a load is a subcommand now, not a second binary.
CLIENT  := $(BIGCTL) --addr $(ADDR)

.DEFAULT_GOAL := help
.PHONY: help build install uninstall serve start stop restart status logs shell sql cli load demo clean-data \
	test lint docs check cov e2e e2e-cluster rewrite

help:
	@echo 'big, locally. Server on $(ADDR), database at $(DATA).'
	@echo
	@echo '  make build        build big and bigctl ($(PROFILE))'
	@echo '  make install      link both into $(PREFIX)'
	@echo '  make uninstall    remove those links'
	@echo '  make serve        run big serve in the foreground; ^C stops it'
	@echo '  make start        run big serve in the background, wait for /health'
	@echo '  make stop         stop the background daemon'
	@echo '  make restart      stop, then start'
	@echo '  make status       pid, health and ready'
	@echo '  make logs         follow $(LOGFILE)'
	@echo '  make shell        bigctl shell against the running daemon'
	@echo '  make sql Q="SELECT count(*) FROM tx"'
	@echo '  make cli ARGS="schema"    any other bigctl command'
	@echo '  make load TABLE=tx FILE=facts.txt   bigctl import, chunked and resumable'
	@echo '  make demo         a table, a few facts and one query, to prove it answers'
	@echo '  make clean-data   stop and delete $(RUN)'
	@echo
	@echo 'The gates CI runs, so a change can be cleared before it is pushed:'
	@echo '  make check        lint, then test, then docs'
	@echo '  make test         cargo test, workspace less big-bench'
	@echo '  make lint         cargo fmt --check, then clippy -D warnings'
	@echo '  make docs         cargo doc with warnings denied'
	@echo '  make cov          line coverage per crate (needs cargo-llvm-cov)'
	@echo '  make e2e          both binaries, run as processes'
	@echo '  make e2e-cluster  two real daemons and a failover; slow, run deliberately'
	@echo '  make rewrite      regenerate the SQL test corpora, then read the diff'
	@echo
	@echo 'Variables: PROFILE=debug|release ADDR=host:port DATA=path LOG=level FLAGS="--durability none"'

# One package builds both, so this is one invocation rather than three - and `big` is built
# here for the first time. It used not to be: the offline tool lived in a package this target
# never named, so `make install` linked a binary `make build` had not necessarily produced.
build:
	$(CARGO) build $(RELEASE) -p big-bin --bin big --bin bigctl

# Symlinks rather than copies: the next `cargo build` is then the next `bigctl`, which is what a
# playground wants and what a copy would quietly get wrong. They dangle after a `cargo clean` or
# a change of PROFILE - run this again, it is idempotent.
install: build
	@mkdir -p $(PREFIX)
	ln -sf $(CURDIR)/$(BIG) $(PREFIX)/big
	ln -sf $(CURDIR)/$(BIGCTL) $(PREFIX)/bigctl
	@echo "linked big and bigctl from target/$(PROFILE) into $(PREFIX)"
	@command -v bigctl >/dev/null 2>&1 || echo "warning: $(PREFIX) is not on your PATH"

# Removes only what `install` made. A real file there came from somewhere else - `cargo install`,
# a package manager - and deleting it would be this Makefile reaching outside its own checkout.
uninstall:
	@for b in big bigctl; do \
		if [ -L $(PREFIX)/$$b ]; then rm -f $(PREFIX)/$$b; echo "removed $(PREFIX)/$$b"; \
		elif [ -e $(PREFIX)/$$b ]; then echo "$(PREFIX)/$$b is not a symlink; left alone"; \
		else echo "no $(PREFIX)/$$b"; fi; \
	done

# The foreground one. `big serve` writes its own startup lines to stderr, so there is nothing for
# this recipe to announce that the daemon does not announce better.
serve: build
	@mkdir -p $(dir $(DATA))
	BIG_LOG=$(LOG) $(BIG) serve $(DATA) $(ADDR) $(FLAGS)

# The background one, for a single terminal. It waits for /health rather than returning the
# moment the fork succeeds: `big serve` takes an exclusive lock on the file and refuses a bad
# cluster file *after* the process exists, so "started" has to mean "answered", not "spawned".
start: build
	@mkdir -p $(dir $(DATA))
	@if [ -f $(PIDFILE) ] && kill -0 `cat $(PIDFILE)` 2>/dev/null; then \
		echo "big serve already running (pid `cat $(PIDFILE)`); make stop, or make restart"; exit 1; \
	fi
	@BIG_LOG=$(LOG) nohup $(BIG) serve $(DATA) $(ADDR) $(FLAGS) >>$(LOGFILE) 2>&1 & echo $$! >$(PIDFILE)
	@i=0; while [ $$i -lt 50 ]; do \
		if $(CLIENT) health >/dev/null 2>&1; then \
			echo "big serving $(DATA) on http://$(ADDR) (pid `cat $(PIDFILE)`, log $(LOGFILE))"; \
			exit 0; \
		fi; \
		if ! kill -0 `cat $(PIDFILE)` 2>/dev/null; then \
			echo "big serve exited during startup:"; tail -n 20 $(LOGFILE); rm -f $(PIDFILE); exit 1; \
		fi; \
		i=`expr $$i + 1`; sleep 0.1; \
	done; \
	echo "big serve did not answer /health within 5s; see $(LOGFILE)"; exit 1

# TERM, then wait. A bitmap file left behind by a killed daemon is a file the next start has to
# recover, so the polite signal gets the whole five seconds before anything harsher is sent.
stop:
	@if [ ! -f $(PIDFILE) ]; then echo "no $(PIDFILE); nothing to stop"; exit 0; fi; \
	pid=`cat $(PIDFILE)`; \
	if ! kill -0 $$pid 2>/dev/null; then echo "pid $$pid is gone"; rm -f $(PIDFILE); exit 0; fi; \
	kill $$pid; \
	i=0; while kill -0 $$pid 2>/dev/null && [ $$i -lt 50 ]; do i=`expr $$i + 1`; sleep 0.1; done; \
	if kill -0 $$pid 2>/dev/null; then echo "pid $$pid ignored TERM; sending KILL"; kill -9 $$pid; fi; \
	rm -f $(PIDFILE); echo "stopped"

restart:
	@$(MAKE) stop
	@$(MAKE) start

status:
	@if [ -f $(PIDFILE) ] && kill -0 `cat $(PIDFILE)` 2>/dev/null; then \
		echo "pid `cat $(PIDFILE)`"; \
	else \
		echo "no daemon started by this Makefile"; \
	fi
	@$(CLIENT) health || true
	@$(CLIENT) ready || true

logs:
	@tail -f $(LOGFILE)

# `bigctl shell` has no line editing on purpose; rlwrap does it better than a hand-rolled termios
# mode would, so use it when it is installed and say nothing when it is not.
shell: build
	@if command -v rlwrap >/dev/null 2>&1; then \
		exec rlwrap $(CLIENT) shell; \
	else \
		exec $(CLIENT) shell; \
	fi

sql: build
	@test -n "$(Q)" || { echo 'usage: make sql Q="SELECT count(*) FROM tx"'; exit 2; }
	@$(CLIENT) sql "$(Q)"

cli: build
	@test -n "$(ARGS)" || { echo 'usage: make cli ARGS="schema"'; exit 2; }
	@$(CLIENT) $(ARGS)

# The checkpoint goes next to the file rather than into $(RUN): it belongs to the load, and a
# load is re-run from wherever the file is. Deleted by `bigctl` itself when the load finishes.
load: build
	@test -n "$(TABLE)" -a -n "$(FILE)" || { echo 'usage: make load TABLE=tx FILE=facts.txt'; exit 2; }
	@$(CLIENT) import $(TABLE) $(FILE) --resume $(FILE).ck

# Enough to show the whole path works: schema, ingest, and an aggregate that has to intersect
# something to answer. Re-runnable - the table already existing is not a failure here.
demo: build
	@$(CLIENT) health >/dev/null 2>&1 || { echo "nothing listening on $(ADDR); make start"; exit 3; }
	-@$(CLIENT) create table tx
	-@$(CLIENT) create field tx country --kind set
	-@$(CLIENT) create field tx amount --kind int --bit-depth 20
	@printf 'country 1 GB\ncountry 2 US\ncountry 3 GB\namount 1 100\namount 2 250\namount 3 75\n' \
		| $(CLIENT) import tx -
	@$(CLIENT) sql "SELECT country, count(*), sum(amount) FROM tx GROUP BY country"

clean-data:
	@$(MAKE) stop
	rm -rf $(RUN)

# ---------------------------------------------------------------------------------------------
# The gates CI runs, spelled the same way here.
#
# These existed only in `.github/workflows/ci.yml` until now, which meant the only way to learn
# a change was not clean was to push it. A gate that lives one place is a gate half the work
# happens outside of. Every target below is the CI job verbatim, `big-bench` excluded for the
# same reason CI excludes it: it links rival engines to measure against and building them is
# minutes spent on something no gate is asking about.
WORKSPACE := --workspace --exclude big-bench

test:
	$(CARGO) test $(WORKSPACE)

# The data-driven SQL corpora, regenerated from what the code now does - see docs/sql-testing.md.
#
# Deliberately not part of any gate, and it fails on purpose when it changes anything: a rewrite
# is the cheap way to add a hundred cases and the cheap way to accept a hundred regressions, and
# the only thing between the two is reading the diff.
rewrite:
	BIG_REWRITE=1 $(CARGO) test -p big-sql --test testdata || true
	BIG_REWRITE=1 $(CARGO) test -p big-cluster --test logic || true
	@echo
	@echo 'Read `git diff` on the .test files, then `make test`.'

# fmt first: a formatting failure is one command to fix and would otherwise hide behind clippy
# output nobody reads to the bottom of. `-D warnings` matches CI's RUSTFLAGS, so a warning is a
# failure here as well - the point is that the answer is the same in both places.
lint:
	$(CARGO) fmt --all --check
	RUSTFLAGS='-D warnings' $(CARGO) clippy $(WORKSPACE) --all-targets

# A dead doc link is a broken promise, and `big-api` and `big-http` deny `missing_docs`, so this
# also catches a public item added without a word about it.
docs:
	RUSTDOCFLAGS='-D warnings' $(CARGO) doc $(WORKSPACE) --no-deps

# The binaries, run as an operator runs them. Part of `test` already, because `big-e2e` is a
# workspace member and a suite outside the default run is a suite that rots - this target is for
# running it alone.
e2e:
	$(CARGO) test -p big-e2e

# Two real processes and a failover on the wall clock, so it is ignored by default: a timing
# test in the run everybody makes is a test that eventually fails for reasons nobody changed.
e2e-cluster:
	$(CARGO) test -p big-e2e -- --ignored cluster::

# Everything, in the order that fails cheapest first.
check: lint test docs

# Coverage is not a gate and no number here is a target to hit. It is here to answer one
# question a refactor needs answered - which lines were covered before, and are they still - and
# `--summary-only` keeps that a number per crate rather than a report to browse.
COV ?= --summary-only
cov:
	$(CARGO) llvm-cov $(WORKSPACE) $(COV)
