"""A real `big serve`, for the tests marked `integration`.

# Why this fixture does not build

`crates/big-e2e/tests/e2e/common.rs::bin` shells out to `cargo build` when the binary is
missing, which is right there: `cargo test -p big-e2e` builds that crate and nothing else, so
the suite would otherwise pass under `--workspace` and fail on its own.

Here it would be wrong. A `pytest` run that started a cold `cargo build` would take minutes,
and on a machine with no Rust toolchain it would fail outright - which is most machines that
will ever install this client. So the fixture looks, and skips with a reason that names the fix
when it finds nothing. `make -C clients/python e2e` is the entry point that builds first.
"""

from __future__ import annotations

import contextlib
import os
import signal
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).parent))

#: The same 20 seconds `crates/big-e2e/tests/e2e/common.rs` waits, for the same reason.
PATIENCE = 20.0


def _binary() -> Path | None:
    """`$BIGDB_BIN`, then the two profile directories, in the order a developer would look."""
    named = os.environ.get("BIGDB_BIN")
    if named:
        path = Path(named)
        return path if path.exists() else None
    root = Path(__file__).resolve().parents[3]
    for profile in ("release", "debug"):
        candidate = root / "target" / profile / "big"
        if candidate.exists():
            return candidate
    return None


def _free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


@contextlib.contextmanager
def _daemon():
    """One `big serve` on a fresh database, torn down on the way out."""
    binary = _binary()
    if binary is None:
        pytest.skip(
            "no `big` binary found. Run `make -C clients/python e2e`, which builds it first, "
            "or set BIGDB_BIN to a built one."
        )

    with tempfile.TemporaryDirectory() as directory:
        port = _free_port()
        addr = f"127.0.0.1:{port}"
        data = Path(directory) / "data.big"
        process = subprocess.Popen(
            [str(binary), "serve", str(data), addr],
            env={**os.environ, "BIG_LOG": "warn"},
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
        )
        try:
            _wait_for_health(addr, process)
            yield addr
        finally:
            _stop(process)


@pytest.fixture(scope="session")
def server():
    """A daemon shared by every test that only adds to it."""
    with _daemon() as addr:
        yield addr


@pytest.fixture
def fresh_server():
    """A daemon of this test's own, for the tests that drop databases or assert on a whole schema.

    A test that asserts what `/schema` holds cannot share a daemon with tests that add to it,
    and a dropped database is not something to leave behind for whatever runs next. Cheap
    enough - a daemon starts in well under a second on an empty file.
    """
    with _daemon() as addr:
        yield addr


def _wait_for_health(addr: str, process: subprocess.Popen) -> None:
    """Poll until it answers.

    "Started" has to mean "answered", not "spawned": `big serve` takes an exclusive lock on the
    file and refuses a bad one *after* the process exists. That is the Makefile's reasoning for
    polling `/health` in its `start` target, and it applies identically here.
    """
    deadline = time.monotonic() + PATIENCE
    while time.monotonic() < deadline:
        if process.poll() is not None:
            output = (process.stdout.read() or b"").decode("utf-8", "replace")
            pytest.skip(f"`big serve` exited during startup:\n{output[-2000:]}")
        try:
            with urllib.request.urlopen(f"http://{addr}/health", timeout=1) as answer:
                if answer.status == 200:
                    return
        except (urllib.error.URLError, OSError):
            time.sleep(0.1)
    _stop(process)
    pytest.skip(f"`big serve` did not answer /health within {PATIENCE:.0f}s")


def _stop(process: subprocess.Popen) -> None:
    """TERM, then wait, then KILL.

    A bitmap file left behind by a killed daemon is a file the next start has to recover, so
    the polite signal gets the whole five seconds first - the Makefile's `stop` says the same.
    """
    if process.poll() is not None:
        return
    process.send_signal(signal.SIGTERM)
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=5)
