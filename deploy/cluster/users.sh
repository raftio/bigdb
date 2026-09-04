#!/bin/sh
# Writes the users file the compose file mounts, at the mode `big serve` insists on.
#
# **One file where there used to be two.** `tokens` held who may talk to a node, and `peer.token`
# held the single credential every node presented to every other - which had to appear in
# `tokens` with `admin`, so one leaked string was full control of the whole cluster. Nodes now
# prove themselves with a client certificate instead (see `certs.sh`), so there is no shared
# secret left to write down and this file is only about people.
#
# **A role here is a name, and the privileges behind it live in the catalog.** `read`, `write`
# and `admin` used to be a ladder this file could hand out; they are ordinary names now, and a
# name the catalog does not have is no privileges at all - which is the safe direction, and is
# also why this script would once have written two credentials that could do nothing. `superuser`
# is the exception and the way in: reserved, never stored, holds everything, and is what the
# first `CREATE ROLE` has to be made with. See docs/access-control.md.
set -eu
cd "$(dirname "$0")"

random() {
    # 24 bytes of urandom, hex. No dependency on openssl, which is not always there.
    od -An -tx1 -N24 /dev/urandom | tr -d ' \n'
}

# The binary, because hashing a password is argon2 and a shell cannot do it. `big passwd` is
# also the only thing that writes this file: there is deliberately no route that does, since one
# would let an `admin` credential rewrite the credential file over the network.
big=${BIG:-big}
command -v "$big" >/dev/null || {
    echo "no \`$big\` on PATH; set BIG=/path/to/big" >&2
    exit 1
}

mkdir -p secrets
chmod 700 secrets

if [ -f secrets/users ]; then
    echo "secrets/users already exists; delete it first if you mean to rotate" >&2
    exit 1
fi

admin=$(random)
reader=$(random)

# Piped rather than typed: `big passwd` reads one line from standard input when it is not a
# terminal, which is the only non-tty path and exists for exactly this. It is never a flag,
# because an argument is visible in `ps`.
printf '%s\n' "$admin"  | "$big" passwd secrets/users set ops       --role superuser
# **Named before it exists**, and that is the right order: a role is resolved by name on every
# request, so creating `dashboard` in the catalog later makes this credential work without a
# restart. What needs a restart is changing the *name* in this file - it is read once, at
# startup - which is the trap worth knowing about and the reason the name is chosen now.
printf '%s\n' "$reader" | "$big" passwd secrets/users set dashboard --role dashboard

echo
echo "wrote secrets/users (mode 600)"
echo "  ops:       $admin   (superuser)"
echo "  dashboard: $reader   (role \`dashboard\`, which does not exist yet)"
echo
echo "Run ./certs.sh next: nodes authenticate to each other with certificates, not with these."
echo
echo "Then, once the cluster is up, make the role the dashboard credential names:"
echo
echo "  bigctl --user ops sql \"CREATE ROLE dashboard\""
echo "  bigctl --user ops sql \"GRANT OPERATE ON *.* TO dashboard\"   # /metrics, /verify"
echo "  bigctl --user ops sql \"GRANT SELECT ON *.* TO dashboard\"    # and the data it reads"
echo
echo "Until then that credential authenticates and is allowed nothing. \`ops\` is superuser and"
echo "needs no grants - it is the way in, not a role to hand out."
