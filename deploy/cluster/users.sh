#!/bin/sh
# Writes the users file the compose file mounts, at the mode `big serve` insists on.
#
# **One file where there used to be two.** `tokens` held who may talk to a node, and `peer.token`
# held the single credential every node presented to every other - which had to appear in
# `tokens` with `admin`, so one leaked string was full control of the whole cluster. Nodes now
# prove themselves with a client certificate instead (see `certs.sh`), so there is no shared
# secret left to write down and this file is only about people.
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
printf '%s\n' "$admin"  | "$big" passwd secrets/users set ops       --role admin
printf '%s\n' "$reader" | "$big" passwd secrets/users set dashboard --role read

echo
echo "wrote secrets/users (mode 600)"
echo "  ops:       $admin"
echo "  dashboard: $reader"
echo
echo "Run ./certs.sh next: nodes authenticate to each other with certificates, not with these."
