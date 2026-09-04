#!/bin/sh
# One command: a server, and a Go program that writes into it twice.
#
#   ./run.sh            20000 records
#   ./run.sh 60000
#
# The second run is the demonstration. Everything happens in containers; nothing is installed on
# this machine and nothing is left behind but a docker volume, which the last line says how to
# remove.
set -eu

cd "$(dirname "$0")"
rows=${1:-20000}

echo "==> building"
# Both images up front, and as their own step. A compile error should read as a compile error -
# folding this into the `up` below puts every failure behind one message, and the advice that
# message has to give is about ports.
docker compose build big goex

echo
echo "==> starting bigdb"
if ! docker compose up -d big; then
    cat >&2 <<MSG

The server did not start. The usual reason is port ${BIG_PORT:-7654}: something already has it -
\`make start\` and examples/producer both publish exactly that one - so stop that, or use
another.

    BIG_PORT=7655 ./run.sh

MSG
    exit 1
fi

# No schema step here, unlike examples/producer. The Go program creates its table and its
# fields through the client, which is why the credential it holds is a superuser - see the
# comment on `configs:` in docker-compose.yml for why a demo cannot hand it anything smaller.

echo
echo "==> first run"
# `run --rm` rather than `up`: this container is meant to finish, and `up` would report a
# container that exited 0 as the stack falling over.
docker compose run --rm goex big:7654 tx "$rows"

echo
echo "==> second run, the same command"
docker compose run --rm goex big:7654 tx "$rows"

cat <<MSG

The count did not move, and that is the whole example. Every fact went to a record id the client
chose, so the second run set bits that were already set.

examples/producer is the same demo through contrib/big-message, which does not name ids - run
that one twice and the count doubles. Neither is wrong; they are the two halves of the trade,
and both readmes say which is which.

    docker compose down -v      stop everything and delete the data
MSG
