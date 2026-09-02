#!/bin/sh
# Writes the two token files the compose file mounts, with the modes `big serve` insists on.
#
# Two files, and they are not the same thing. `tokens` is who may talk to a node and what they
# may do. `peer.token` is the single credential a node presents when it talks to the others -
# so it has to appear in `tokens` with `admin`, because a schema change and a vote both travel
# between nodes.
set -eu
cd "$(dirname "$0")"

random() {
    # 32 bytes of urandom, hex. No dependency on openssl, which is not always there.
    od -An -tx1 -N32 /dev/urandom | tr -d ' \n'
}

mkdir -p secrets
chmod 700 secrets
cd secrets

if [ -f peer.token ] || [ -f tokens ]; then
    echo "tokens already exist here; delete them first if you mean to rotate" >&2
    exit 1
fi

peer=$(random)
reader=$(random)

printf '%s\n' "$peer" > peer.token
{
    printf '# The credential every node presents to every other. Needs admin: a schema change\n'
    printf '# and a vote both travel between nodes.\n'
    printf '%s admin\n' "$peer"
    printf '\n# Something for a dashboard, which has no business writing anything.\n'
    printf '%s read\n' "$reader"
} > tokens

chmod 600 peer.token tokens
echo "wrote secrets/peer.token and secrets/tokens (mode 600)"
echo "admin: $peer"
echo "read:  $reader"
