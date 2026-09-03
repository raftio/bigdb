#!/bin/sh
# One command: a server, a table, some messages, and a few questions asked of the result.
#
#   ./run.sh            20000 rows
#   ./run.sh 200000
#
# Everything happens in containers. Nothing is installed on this machine and nothing is left
# behind but a docker volume, which the last line says how to remove.
set -eu

cd "$(dirname "$0")"
rows=${1:-20000}

# The daemon's credentials are in docker-compose.yml, as a `config` rather than a mounted file -
# see the comment there for why. Nothing to set up here.

# `bigctl` is in the server's image, so the schema and the queries go through `exec` and this
# machine needs neither a client nor a copy of the credential. The file below is `user:password`,
# which is what `--credentials-file` reads - not the server's own users file, which holds hashes
# and from which no password can be recovered.
ctl() {
    docker compose exec -T big sh -c \
        'umask 077; printf "demo-admin:demo-admin\n" > /tmp/demo.cred; \
         exec bigctl --credentials-file /tmp/demo.cred "$@"' -- "$@"
}

echo "==> building"
# Both images up front, and as their own step. Two reasons, both learned the hard way:
#
# A compile error should read as a compile error - folding this into the `up` below puts every
# failure behind one message, and the advice that message has to give is about ports.
#
# And building the producer *here* rather than with `run --build` keeps `run` from rebuilding
# this service's dependency and recreating the server container underneath a table that has
# already been created. Naming a service explicitly is enough to reach it through its profile.
docker compose build big producer

echo
echo "==> starting bigdb"
if ! docker compose up -d big; then
    cat >&2 <<EOF

The server did not start. The usual reason is port ${BIG_PORT:-7654}: something already has it -
\`make start\` in this repository publishes exactly that one - so stop that, or use another.

    BIG_PORT=7655 ./run.sh

EOF
    exit 1
fi

echo
echo "==> creating the table"
# IF NOT EXISTS so a second run adds to the table rather than failing on it - which is what
# makes the duplicate-count demonstration below possible.
ctl sql 'CREATE TABLE IF NOT EXISTS tx (amount INT, country TEXT)'

echo
echo "==> producing $rows messages"
# `run --rm` rather than `up`: this container is meant to finish, and `up` would report a
# container that exited 0 as the stack falling over.
docker compose run --rm producer big:7654 tx "$rows"

echo
echo "==> how many landed"
ctl sql 'SELECT count(*) FROM tx'

echo
echo "==> and what they were"
ctl sql 'SELECT country, count(*) FROM tx GROUP BY country'

echo
echo "==> the record ids, which the producer never named"
# Not a query, because `_record_id` is not a column a select list can ask for - it is what a
# record is *called*, and `GET /table/{t}/records` is the route that lists them. `bigctl` has no
# subcommand for it, so this is curl, which the server's image already carries for its own
# health check.
docker compose exec -T big curl -fsS \
    -u demo-admin:demo-admin \
    'http://127.0.0.1:7654/table/tx/records?limit=5'
echo

cat <<EOF

Run this again and the count doubles: the producer is at-least-once and the server allocates
the ids, so there is nothing for a second run to overwrite. That is the trade, and readme.md
says what it buys.

    docker compose down -v      stop everything and delete the data
EOF
